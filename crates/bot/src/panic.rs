//! Panic mode (spec 4.2): runs the antinuke crate's plans when several accounts get quarantined at once.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tommybot_antinuke::{panic_end_plan, panic_plan, PanicStep};
use tommybot_automod::lockdown::{self as lockdown_bits, LockKind};
use tommybot_core::{jobs, sections::PanicCfg};
use tommybot_logging::emoji::{PANIC, QUARANTINED};
use tommybot_logging::{
    embed::{self, ALERT, ARROW, DONE, NO, YES},
    Log, LogKind,
};
use twilight_model::{
    channel::permission_overwrite::PermissionOverwriteType as ModelOverwriteType,
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{App, PanicRun},
    backup,
    events::{send_log, send_log_pinging},
    lockdown,
};

/// True while heat panic is on for the guild and `user` is one of its raiders.
pub fn is_raider(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> bool {
    let mut m = app.heat_raiders.lock().unwrap();
    match m.get(&gid.get()) {
        Some((_, until)) if *until <= crate::app::now_secs() => {
            m.remove(&gid.get());
            false
        }
        Some((set, _)) => set.contains(&user.get()),
        None => false,
    }
}

/// Counts `user` towards panic mode (a quarantined nuker, or with `heat` an automod heat trip)
/// and starts it when enough distinct accounts hit the window. No-op while panic mode runs.
pub async fn note(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, heat: bool) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?.panic.clone();
    let (executors, tracker) = if heat {
        (
            if cfg.heat_algorithm {
                cfg.heat_executors
            } else {
                0
            },
            &app.panic_heat,
        )
    } else {
        (cfg.executors, &app.panic)
    };
    if executors == 0 || app.panic_runs.lock().unwrap().contains_key(&gid.get()) {
        return Ok(());
    }
    let (tripped, mut raiders) = {
        let mut t = tracker.lock().unwrap();
        t.window = Duration::from_secs(cfg.window_secs);
        t.executors = executors;
        let now = Instant::now();
        let raiders = t.involved(gid.get(), now);
        (t.record(gid.get(), user.get(), now), raiders)
    };
    if !tripped {
        return Ok(());
    }
    if !heat {
        return start(app, gid, &cfg).await;
    }
    // Heat panic: every message from these accounts is deleted and timed out until it ends.
    raiders.insert(user.get());
    let until = crate::app::now_secs() + cfg.duration_secs as i64;
    let log = Log::new("Heat Panic Mode Triggered!", ALERT).text(&format!(
        "{ARROW} {} raiders will be timed out on every message for {}.",
        raiders.len(),
        embed::duration(cfg.duration_secs as i64)
    ));
    let ids: Vec<i64> = raiders.iter().map(|r| *r as i64).collect();
    app.heat_raiders
        .lock()
        .unwrap()
        .insert(gid.get(), (raiders, until));
    // Kept for a restart (N3, `seed_windows`); a lost row ends heat panic early on one.
    let kept = sqlx::query(
        "INSERT INTO heat_panic (guild_id, raiders, until) VALUES ($1, $2, $3)
         ON CONFLICT (guild_id) DO UPDATE SET raiders = $2, until = $3",
    )
    .bind(gid.get() as i64)
    .bind(&ids)
    .bind(until)
    .execute(&app.pool)
    .await;
    if let Err(e) = kept {
        tracing::warn!(?e, "saving heat panic failed");
    }
    // Heat panic is on already: a failed log must not fail the caller's punishment.
    if let Err(e) = send_log_pinging(app, gid.get(), LogKind::Panic, log, &cfg.warned_roles).await {
        tracing::warn!(?e, "heat panic log failed");
    }
    Ok(())
}

/// [`note`] for a heat trip. Its failure is only logged: the caller's punishment still runs.
pub(crate) async fn note_heat(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) {
    if let Err(e) = note(app, gid, user, true).await {
        tracing::warn!(?e, "counting towards heat panic failed");
    }
}

async fn put_role(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    role: u64,
    allow: Permissions,
    deny: Permissions,
) -> Result<()> {
    let o = PermissionOverwrite {
        id: Id::new(role),
        kind: PermissionOverwriteType::Role,
        allow: Some(allow),
        deny: Some(deny),
    };
    crate::verify::given::mark_own(gid, channel.get());
    app.http.update_channel_permission(channel, &o).await?;
    Ok(())
}

/// The bits panic's server lock will take from each main role (@everyone when none are set),
/// read before the lock. A role an earlier lock already stripped has nothing left to take. Never a
/// bit the role lock takes too (Mention @everyone): a category allow would beat that lock.
fn taken(
    roles: &[twilight_model::guild::Role],
    main: &[u64],
    gid: u64,
    voice: bool,
) -> Vec<(u64, u64)> {
    let mask = (lockdown_bits::SILENCE | if voice { lockdown_bits::VOICE } else { 0 })
        & !lockdown_bits::DANGEROUS;
    roles
        .iter()
        .map(|r| (r.id.get(), r.permissions.bits() & mask))
        .filter(|(id, bits)| *bits != 0 && (main.contains(id) || (main.is_empty() && *id == gid)))
        .collect()
}

/// The bits to allow for `role` on a category so it can do there what the server lock took
/// (`taken`) and nothing more: never a bit the category already allows or denies the role, nor
/// one `others_deny` (what other overwrites deny there) holds. A role allow beats every role deny,
/// so giving such a bit would let a muted member write; the category stays locked for it instead.
fn give_back(taken: u64, role_ow: (u64, u64), others_deny: u64) -> u64 {
    taken & !role_ow.0 & !role_ow.1 & !others_deny
}

/// One exemption to make: the overwrite `role` has on `category` now (`saved`, for [`unexempt`]),
/// and the one to put.
struct Exemption {
    category: Id<ChannelMarker>,
    role: u64,
    allow: Permissions,
    deny: Permissions,
    saved: Value,
}

/// What a whitelisted category needs so each main role can do there what panic's server lock
/// takes. Changes nothing; [`start`] saves each one in the `panic_end` job before it puts it.
async fn exemptions(
    app: &App,
    gid: Id<GuildMarker>,
    category: Id<ChannelMarker>,
    taken: &[(u64, u64)],
    quarantine_role: Option<u64>,
) -> Result<Vec<Exemption>> {
    let ch = app.http.channel(category).await?.model().await?;
    anyhow::ensure!(ch.guild_id == Some(gid), "category is not in this server");
    let ows: Vec<_> = ch
        .permission_overwrites
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.kind == ModelOverwriteType::Role)
        .collect();
    let mut out = vec![];
    for &(role, bits) in taken {
        let old = ows.iter().find(|o| o.id.get() == role);
        let (allow, deny) = old.map_or((0, 0), |o| (o.allow.bits(), o.deny.bits()));
        // @everyone's allow comes before every role deny, so only a role's allow needs them.
        // Quarantined members lose their other roles, so the Quarantine role's deny never meets
        // a main role's allow.
        let others_deny = if role == gid.get() {
            0
        } else {
            ows.iter()
                .filter(|o| o.id.get() != role && Some(o.id.get()) != quarantine_role)
                .fold(0, |d, o| d | o.deny.bits())
        };
        let add = give_back(bits, (allow, deny), others_deny);
        if add == 0 {
            continue;
        }
        out.push(Exemption {
            category,
            role,
            allow: Permissions::from_bits_retain(allow | add),
            deny: Permissions::from_bits_retain(deny),
            saved: json!({
                "channel": category.get(), "role": role, "allow": allow, "deny": deny, "had": old.is_some()
            }),
        });
    }
    Ok(out)
}

/// `channels` with panic's category exemptions undone as [`unexempt`] would: the overwrite each
/// main role had on a whitelisted category before (or none), from the pending `panic_end` job.
/// shortcut: only the category itself; a child channel synced to it is read live.
pub(crate) async fn unexempted(
    app: &App,
    gid: Id<GuildMarker>,
    channels: &mut [twilight_model::channel::Channel],
) -> Result<()> {
    let ends: Vec<Value> = sqlx::query_scalar(
        "SELECT payload FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    for saved in ends
        .iter()
        .flat_map(|p| p["exempt"].as_array().into_iter().flatten())
    {
        let Some(c) = saved["channel"]
            .as_u64()
            .and_then(|id| channels.iter_mut().find(|c| c.id.get() == id))
        else {
            continue;
        };
        let role = exempt_role(gid, saved);
        let all = c.permission_overwrites.get_or_insert_with(Vec::new);
        all.retain(|o| o.id.get() != role);
        if saved["had"].as_bool().unwrap_or(false) {
            let bits = |k: &str| Permissions::from_bits_retain(saved[k].as_u64().unwrap_or(0));
            all.push(
                twilight_model::channel::permission_overwrite::PermissionOverwrite {
                    id: Id::new(role),
                    kind: ModelOverwriteType::Role,
                    allow: bits("allow"),
                    deny: bits("deny"),
                },
            );
        }
    }
    Ok(())
}

/// The role a saved exemption is for; jobs from before roles were saved only exempted @everyone.
fn exempt_role(gid: Id<GuildMarker>, saved: &Value) -> u64 {
    saved["role"].as_u64().unwrap_or(gid.get())
}

async fn unexempt(app: &App, gid: Id<GuildMarker>, saved: &Value) -> Result<()> {
    let channel = Id::new(
        saved["channel"]
            .as_u64()
            .context("saved overwrite has no channel")?,
    );
    let role = exempt_role(gid, saved);
    if saved["had"].as_bool().unwrap_or(false) {
        let bits = |k: &str| Permissions::from_bits_retain(saved[k].as_u64().unwrap_or(0));
        put_role(app, gid, channel, role, bits("allow"), bits("deny")).await
    } else {
        crate::verify::given::mark_own(gid, channel.get());
        app.http
            .delete_channel_permission(channel)
            .role(Id::new(role))
            .await?;
        Ok(())
    }
}

/// While `start` still runs, its `panic_end` job stays at least this far ahead, so a panic shorter than
/// the category calls cannot end in the middle of the start and leave its lockdown without an end job.
const START_MARGIN_SECS: i64 = 60;

/// Rewrites the pending `panic_end` job's payload in place (one statement, so it is never missing),
/// and moves it to at least [`START_MARGIN_SECS`] from now.
async fn end_payload(app: &App, gid: Id<GuildMarker>, payload: Value) -> Result<()> {
    let n = sqlx::query(
        "UPDATE scheduled_jobs SET payload = $2, run_at = greatest(run_at, now() + make_interval(secs => $3))
         WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(gid.get() as i64)
    .bind(payload)
    .bind(START_MARGIN_SECS as f64)
    .execute(&app.pool)
    .await?
    .rows_affected();
    anyhow::ensure!(n == 1, "panic_end job missing");
    Ok(())
}

pub async fn start(app: &App, gid: Id<GuildMarker>, cfg: &PanicCfg) -> Result<()> {
    // The check and the insert are one step: two trips at once must not start it twice, or the
    // second saves the already exempted overwrites and the categories stay open after panic ends.
    let run = PanicRun {
        lock_mod_commands: cfg.lock_mod_commands,
    };
    if app
        .panic_runs
        .lock()
        .unwrap()
        .insert(gid.get(), run)
        .is_some()
    {
        return Ok(());
    }
    let ends = crate::app::now_secs() + cfg.duration_secs as i64;
    // The dashboard reads the mod command lock from this job, so it exists before the category calls.
    // No unlock until the end: nothing is locked before the full payload is saved.
    let payload = |exempted: &[Value], unlock: bool| json!({"lock_mod_commands": cfg.lock_mod_commands, "unlock": unlock, "exempt": exempted, "locked": false, "locked_server": false});
    if let Err(e) = jobs::replace(
        &app.pool,
        gid.get() as i64,
        "panic_end",
        payload(&[], false),
        ends.max(crate::app::now_secs() + START_MARGIN_SECS),
    )
    .await
    {
        app.panic_runs.lock().unwrap().remove(&gid.get());
        return Err(e);
    }
    let mut exempted = vec![];
    let mut saved = Ok(());
    // Only a lock panic makes takes anything a category needs back.
    let categories = if cfg.lock_on_trigger {
        cfg.whitelisted_categories.as_slice()
    } else {
        &[]
    };
    let taken = if categories.is_empty() {
        vec![]
    } else {
        match lock_targets(app, gid).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    ?e,
                    "panic: reading roles for the category exemptions failed"
                );
                vec![]
            }
        }
    };
    let quarantine_role = match app.cfg(gid.get()).await {
        Ok(c) => c.antinuke.quarantine_role.map(|r| r.get()),
        Err(_) => None,
    };
    'categories: for &c in categories {
        let todo = match exemptions(app, gid, c, &taken, quarantine_role).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(?e, channel = c.get(), "panic: reading a category failed");
                continue;
            }
        };
        for one in todo {
            // Saved before the change, so a crash or a lost answer still leaves a job that puts
            // the overwrite back; putting back one that never changed does no harm.
            exempted.push(one.saved.clone());
            saved = end_payload(app, gid, payload(&exempted, false)).await;
            if saved.is_err() {
                break 'categories;
            }
            if let Err(e) = put_role(app, gid, one.category, one.role, one.allow, one.deny).await {
                tracing::warn!(?e, channel = c.get(), "panic: exempting category failed");
            }
        }
    }
    if saved.is_ok() {
        saved = end_payload(app, gid, payload(&exempted, cfg.unlock_when_ending)).await;
    }
    if let Err(e) = saved {
        // Without its end job panic mode would never end: mod commands stay locked and the
        // categories open. Undo the start so the next trip can try again, and drop the job, or the
        // dashboard and a restart would still read mod commands as locked.
        for one in &exempted {
            if let Err(e) = unexempt(app, gid, one).await {
                tracing::warn!(?e, "panic: putting a category back failed");
            }
        }
        let drop_job =
            sqlx::query("DELETE FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'panic_end'")
                .bind(gid.get() as i64)
                .execute(&app.pool)
                .await;
        if let Err(e) = drop_job {
            tracing::warn!(?e, "panic: removing the panic_end job failed");
        }
        app.panic_runs.lock().unwrap().remove(&gid.get());
        return Err(e);
    }
    crate::reply::alert_owner(app, gid, "Panic mode started").await;
    for step in panic_plan(cfg.lock_on_trigger, cfg.lock_mod_commands) {
        match step {
            // A failed step is logged, so the report below is still sent.
            // Panic's end lifts only a lock this call saved, also one an error left half applied;
            // a lock from before panic, or one staff make after a failed lock, stays.
            // Two locks, so one staff already made (say a server lock) leaves the other to panic.
            // The role lock takes dangerous perms, the server lock takes writing from main roles.
            PanicStep::LockServer => {
                for (kind, key) in [(LockKind::Role, ROLE_LOCK), (LockKind::Server, SERVER_LOCK)] {
                    // Marked first: a crash between the lock and the mark must not leave a lock
                    // that panic's end never lifts (everyone muted). Cleared when no lock was saved.
                    own_lock(app, gid, key, true).await;
                    let mut saved = false;
                    let r = lockdown::lock_saving(app, gid, kind, None, &mut saved).await;
                    if let Err(e) = r {
                        tracing::warn!(?e, key, "panic: locking the server failed");
                    }
                    if !saved {
                        own_lock(app, gid, key, false).await;
                        // A lock staff made first covers writing, so the categories close with it.
                        if key == SERVER_LOCK {
                            drop_exemptions(app, gid, &exempted).await;
                        }
                    }
                }
            }
            PanicStep::PingWarnedRoles => {
                let log = Log::new("Panic Mode Triggered! [Anti Nuke]", ALERT)
                    .text(&format!(
                        "{PANIC} **An abnormal behaviour has been detected!**"
                    ))
                    .text(&format!(
                        "{ARROW} TommyBot will lockdown the server and run a scan."
                    ));
                if let Err(e) =
                    send_log_pinging(app, gid.get(), LogKind::Panic, log, &cfg.warned_roles).await
                {
                    tracing::warn!(?e, "panic: pinging the warned roles failed");
                }
            }
            // Mod commands are gated by `panic_runs`, and the involved accounts were quarantined
            // by the verdicts that tripped panic mode.
            PanicStep::LockModCommands | PanicStep::QuarantineInvolved => {}
            PanicStep::LoadLatestBackup => {
                let b = app.cfg(gid.get()).await?.backups.clone();
                let backup = if !(b.enabled && b.restore) {
                    Backup::Disabled
                } else {
                    match backup::restore_latest(app, gid).await {
                        Ok(Some(sum)) => Backup::Loaded(sum),
                        Ok(None) => Backup::None,
                        Err(e) => Backup::Failed(e.to_string()),
                    }
                };
                let case: Option<(String, Value)> = sqlx::query_as(
                    "SELECT case_id, data FROM nuke_incidents WHERE guild_id = $1 ORDER BY created_at DESC LIMIT 1",
                )
                .bind(gid.get() as i64)
                .fetch_optional(&app.pool)
                .await
                .unwrap_or_default();
                let sent = send_log(
                    app,
                    gid.get(),
                    LogKind::Panic,
                    report(case.as_ref(), &backup),
                );
                if let Err(e) = sent.await {
                    tracing::warn!(?e, "panic report log failed");
                }
            }
            PanicStep::UnlockServer => {}
        }
    }
    let log = Log::new("Panic Mode Lockdown", ALERT)
        .text(&format!("{YES} **The server has been locked down.**"));
    // Panic mode runs: a failed log must not end the nuke run that started it.
    if let Err(e) = send_log(app, gid.get(), LogKind::Panic, log).await {
        tracing::warn!(?e, "panic lockdown log failed");
    }
    Ok(())
}

/// What panic mode's backup step did.
pub(crate) enum Backup {
    /// Backups or "Restore after a nuke" are off.
    Disabled,
    None,
    Loaded(backup::Summary),
    Failed(String),
}

/// The green "Panic Mode Report:" embed, sent once per panic: the latest nuke case, who was and
/// was not quarantined, and what the backup step did.
pub(crate) fn report(case: Option<&(String, Value)>, b: &Backup) -> Log {
    let mut log = Log::new("Panic Mode Report:", DONE);
    if let Some((id, data)) = case {
        let ids = |k: &str| -> Vec<u64> {
            data[k]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .collect()
        };
        let (who, failed) = (ids("quarantined"), ids("failed"));
        log = log
            .text(&format!("{ARROW} **CASE:** `{id}`"))
            .text(&format!(
                "{QUARANTINED} **Members Quarantined:** {} {YES} | {} {NO}",
                who.len(),
                failed.len()
            ));
        for (u, mark) in who
            .iter()
            .map(|u| (u, QUARANTINED))
            .chain(failed.iter().map(|u| (u, NO)))
        {
            let frags = data["fragments"][u.to_string()].as_u64().unwrap_or(0);
            log = log.text(&format!(
                "{ARROW} {} [{frags} fragments] {mark}",
                embed::user(*u)
            ));
        }
    }
    let log = log.section("Backup System Report:");
    let sum = match b {
        Backup::Disabled => return log.text(&format!("{NO} **Backup System is disabled!**")),
        Backup::None => return log.text(&format!("{NO} **There is no backup to load.**")),
        Backup::Failed(e) => {
            return log
                .text(&format!(
                    "{NO} **Backup System failed to load the recent backup!**"
                ))
                .line("Error", e)
        }
        Backup::Loaded(sum) => sum,
    };
    let col = |c: backup::Counts| {
        format!(
            "Deleted: `{}`\nRecovered: `{}`\nEdited: `{}`",
            c.deleted, c.created, c.updated
        )
    };
    log.text(&format!(
        "{YES} **Backup System loaded the recent backup successfully!**"
    ))
    .inline("Roles", &col(sum.roles))
    .inline("Channels", &col(sum.channels))
    .inline("Categories", &col(sum.categories))
    .inline("Webhooks", &format!("Deleted: `{}`", sum.webhooks_deleted))
    .inline(
        "Settings",
        // Restores never touch TommyBot's own settings.
        &format!(
            "{ARROW} Server: {}\n{ARROW} TommyBot: {NO}",
            if sum.settings { YES } else { NO }
        ),
    )
}

/// `panic_end` payload key: panic's own role lock.
pub(crate) const ROLE_LOCK: &str = "locked";
/// `panic_end` payload key: panic's own server (writing) lock.
pub(crate) const SERVER_LOCK: &str = "locked_server";

/// The main roles (or @everyone) with the bits panic's server lock would take from them now.
async fn lock_targets(app: &App, gid: Id<GuildMarker>) -> Result<Vec<(u64, u64)>> {
    let cfg = app.cfg(gid.get()).await?;
    let main: Vec<u64> = cfg.lockdown.main_roles.iter().map(|r| r.get()).collect();
    let roles = app.http.roles(gid).await?.models().await?;
    Ok(taken(&roles, &main, gid.get(), cfg.lockdown_prefs.voice))
}

/// Puts the categories back at once when panic's server lock was not made, and empties the job's
/// list. Only logged on failure; the end puts back what is still listed.
async fn drop_exemptions(app: &App, gid: Id<GuildMarker>, exempted: &[Value]) {
    if exempted.is_empty() {
        return;
    }
    let mut all = true;
    for one in exempted {
        if let Err(e) = unexempt(app, gid, one).await {
            tracing::warn!(?e, "panic: putting a category back failed");
            all = false;
        }
    }
    if !all {
        return;
    }
    let r = sqlx::query(
        "UPDATE scheduled_jobs SET payload = payload || jsonb_build_object('exempt', '[]'::jsonb)
         WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(gid.get() as i64)
    .execute(&app.pool)
    .await;
    if let Err(e) = r {
        tracing::warn!(?e, "panic: emptying the exemptions failed");
    }
}

/// Records in the `panic_end` job whether the lock under `key` ([`ROLE_LOCK`], [`SERVER_LOCK`]) is
/// panic's own, so its end lifts only that one. Only logged on failure.
pub(crate) async fn own_lock(app: &App, gid: Id<GuildMarker>, key: &str, locked: bool) {
    let r = sqlx::query(
        "UPDATE scheduled_jobs SET payload = payload || jsonb_build_object($2::text, $3::bool)
         WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(gid.get() as i64)
    .bind(key)
    .bind(locked)
    .execute(&app.pool)
    .await;
    if let Err(e) = r {
        tracing::warn!(?e, "panic: saving whose lock it is failed");
    }
}

/// Ends panic mode for a guild; runs from the `panic_end` job, whose payload says what to put back.
pub async fn end(app: &App, gid: Id<GuildMarker>, payload: &Value) -> Result<()> {
    app.panic_runs.lock().unwrap().remove(&gid.get());
    // The job as it is now: a staff unlock after the scheduler read it cleared panic's claim.
    let now: Option<Value> = sqlx::query_scalar(
        "SELECT payload FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(gid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let payload = now.as_ref().unwrap_or(payload);
    for saved in payload["exempt"].as_array().into_iter().flatten() {
        if let Err(e) = unexempt(app, gid, saved).await {
            tracing::warn!(?e, "panic: restoring category overwrite failed");
        }
    }
    // Jobs from before `locked` existed unlock the role lock as they did, and had no server lock.
    let role = payload[ROLE_LOCK].as_bool().unwrap_or(true);
    let server = payload[SERVER_LOCK].as_bool().unwrap_or(false);
    for step in panic_end_plan((role || server) && payload["unlock"].as_bool().unwrap_or(false)) {
        if step == PanicStep::UnlockServer {
            // Both are tried, so a failed role unlock does not leave everyone muted.
            let r = match role {
                true => lockdown::unlock(app, gid, Some(&LockKind::Role))
                    .await
                    .map(drop),
                false => Ok(()),
            };
            if server {
                lockdown::unlock(app, gid, Some(&LockKind::Server)).await?;
            }
            r?;
        }
    }
    send_log(app, gid.get(), LogKind::Panic, end_log(payload)).await
}

/// Names who ended panic early from the dashboard (D74); a timed end has nobody.
fn end_log(payload: &Value) -> Log {
    let log = Log::new("Panic Mode Ended", DONE).text(&format!("{YES} **Panic mode has ended.**"));
    match payload["ended_by"]
        .as_str()
        .and_then(|u| u.parse::<u64>().ok())
    {
        Some(u) => log.user("Ended by", u),
        None => log,
    }
}

/// Re-arms panic mode and join raids after a restart: the pending `panic_end` and `raid_end` jobs
/// say which guilds are still in them.
pub async fn restore(app: &App) -> Result<()> {
    for (guild, payload) in jobs::pending(&app.pool, "raid_end").await? {
        if let Some(until) = payload["until_ms"].as_i64() {
            let started = payload["started_ms"].as_i64().unwrap_or(until);
            let d = tommybot_automod::join::RaidDetector::restored(
                started,
                until,
                payload["code"].as_str().unwrap_or_default().into(),
            );
            app.raids.lock().unwrap().insert(guild as u64, d);
        }
    }
    for (guild, payload) in jobs::pending(&app.pool, "panic_end").await? {
        app.panic_runs.lock().unwrap().insert(
            guild as u64,
            PanicRun {
                lock_mod_commands: payload["lock_mod_commands"].as_bool().unwrap_or(false),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_early_end_names_who_ended_it() {
        let by = format!("{:?}", end_log(&json!({"ended_by": "42"})).build());
        assert!(by.contains("<@42>"), "{by}");
        let timed = format!("{:?}", end_log(&json!({})).build());
        assert!(!timed.contains("Ended by"), "{timed}");
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres. The per-category re-save changes the
    /// one pending job in place, and refuses when there is none (start then undoes itself).
    #[tokio::test]
    async fn end_payload_rewrites_the_pending_job_in_place() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        );
        let gid = Id::new(9_890_000_000 + std::process::id() as u64);
        let g = gid.get() as i64;
        assert!(end_payload(&app, gid, json!({})).await.is_err());
        jobs::replace(
            &app.pool,
            g,
            "panic_end",
            json!({"exempt": []}),
            crate::app::now_secs() + 3600,
        )
        .await
        .unwrap();
        let one = json!({"unlock": false, "exempt": [{"channel": 5, "had": false}]});
        end_payload(&app, gid, one.clone()).await.unwrap();
        assert_eq!(
            jobs::pending(&app.pool, "panic_end")
                .await
                .unwrap()
                .into_iter()
                .filter(|(id, _)| *id == g)
                .map(|(_, p)| p)
                .collect::<Vec<_>>(),
            [one]
        );
        // A job already due (a panic shorter than the start) moves out of the scheduler's reach.
        sqlx::query("UPDATE scheduled_jobs SET run_at = now() WHERE guild_id = $1")
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
        end_payload(&app, gid, json!({})).await.unwrap();
        let ahead: f64 = sqlx::query_scalar(
            "SELECT extract(epoch from run_at - now())::float8 FROM scheduled_jobs WHERE guild_id = $1",
        )
        .bind(g)
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert!(ahead > 55.0, "{ahead}");
        sqlx::query("DELETE FROM scheduled_jobs WHERE guild_id = $1")
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
    }

    #[test]
    fn the_lock_takes_writing_from_main_roles_and_a_category_gets_only_that_back() {
        let role = |id: u64, bits: u64| -> twilight_model::guild::Role {
            serde_json::from_value(json!({"id": id.to_string(), "name": "r", "color": 0, "colors": {"primary_color": 0},
                "hoist": false, "managed": false, "mentionable": false, "permissions": bits.to_string(), "position": 0, "flags": 0}))
            .unwrap()
        };
        let (send, view, react) = (1u64 << 11, 1u64 << 10, 1u64 << 6);
        let roles = [
            role(9, view | send | react | (1 << 17)),
            role(5, view | send),
            role(6, view),
        ];
        // No main roles: @everyone (the guild id) is the one locked.
        assert_eq!(taken(&roles, &[], 9, false), [(9, send | react)]);
        // Main roles set: only those, and a role with nothing to lose is skipped.
        assert_eq!(taken(&roles, &[5, 6], 9, false), [(5, send)]);
        // Already allowed, denied on the role or denied to @everyone: never given.
        assert_eq!(give_back(send | react, (0, 0), 0), send | react);
        assert_eq!(give_back(send | react, (0, react), 0), send);
        assert_eq!(give_back(send | react, (send, 0), react), 0);
    }

    fn text(l: Log) -> String {
        serde_json::to_string(&l.build()).unwrap()
    }

    #[test]
    fn report_counts_both_sides_and_names_every_backup_outcome() {
        let case = (
            "9g5udu".to_string(),
            json!({"quarantined": [11], "failed": [22], "fragments": {"11": 8, "22": 1}}),
        );
        let t = text(report(Some(&case), &Backup::Disabled));
        assert!(t.contains("**CASE:** `9g5udu`"), "{t}");
        assert!(
            t.contains(&format!("**Members Quarantined:** 1 {YES} | 1 {NO}")),
            "{t}"
        );
        assert!(
            t.contains("[8 fragments]") && t.contains("[1 fragments]"),
            "{t}"
        );
        assert!(t.contains("Backup System is disabled!"), "{t}");
        assert!(text(report(Some(&case), &Backup::None)).contains("There is no backup to load."));
        let failed = text(report(None, &Backup::Failed("boom".into())));
        assert!(failed.contains("failed to load") && failed.contains("boom"));
        assert!(!failed.contains("CASE"));
        let loaded = text(report(
            Some(&case),
            &Backup::Loaded(backup::Summary::default()),
        ));
        assert!(
            loaded.contains("loaded the recent backup successfully"),
            "{loaded}"
        );
        // An older case saved before the failed list existed reads as 0 failed.
        let old = ("abc123".to_string(), json!({"quarantined": [11]}));
        assert!(text(report(Some(&old), &Backup::None)).contains(&format!("1 {YES} | 0 {NO}")));
    }
}
