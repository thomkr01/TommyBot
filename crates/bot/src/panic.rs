//! Panic mode (spec 4.2): runs the antinuke crate's plans when several accounts get quarantined at once.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tommybot_antinuke::{panic_end_plan, panic_plan, PanicStep};
use tommybot_automod::lockdown::LockKind;
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
    app.heat_raiders
        .lock()
        .unwrap()
        .insert(gid.get(), (raiders, until));
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

async fn put_everyone(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    allow: Permissions,
    deny: Permissions,
) -> Result<()> {
    let o = PermissionOverwrite {
        id: Id::new(gid.get()),
        kind: PermissionOverwriteType::Role,
        allow: Some(allow),
        deny: Some(deny),
    };
    app.http.update_channel_permission(channel, &o).await?;
    Ok(())
}

/// Lets @everyone keep sending in a whitelisted category while the server is locked. Returns the
/// overwrite it replaced, for [`unexempt`].
async fn exempt(app: &App, gid: Id<GuildMarker>, category: Id<ChannelMarker>) -> Result<Value> {
    let ch = app.http.channel(category).await?.model().await?;
    let old = ch
        .permission_overwrites
        .unwrap_or_default()
        .into_iter()
        .find(|o| o.kind == ModelOverwriteType::Role && o.id.get() == gid.get());
    let (allow, deny) = old
        .as_ref()
        .map_or((Permissions::empty(), Permissions::empty()), |o| {
            (o.allow, o.deny)
        });
    put_everyone(
        app,
        gid,
        category,
        allow | Permissions::SEND_MESSAGES,
        deny - Permissions::SEND_MESSAGES,
    )
    .await?;
    Ok(json!({
        "channel": category.get(), "allow": allow.bits(), "deny": deny.bits(), "had": old.is_some()
    }))
}

async fn unexempt(app: &App, gid: Id<GuildMarker>, saved: &Value) -> Result<()> {
    let channel = Id::new(
        saved["channel"]
            .as_u64()
            .context("saved overwrite has no channel")?,
    );
    if saved["had"].as_bool().unwrap_or(false) {
        let bits = |k: &str| Permissions::from_bits_truncate(saved[k].as_u64().unwrap_or(0));
        put_everyone(app, gid, channel, bits("allow"), bits("deny")).await
    } else {
        app.http
            .delete_channel_permission(channel)
            .role(Id::new(gid.get()))
            .await?;
        Ok(())
    }
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
    let mut exempted = vec![];
    for &c in &cfg.whitelisted_categories {
        match exempt(app, gid, c).await {
            Ok(saved) => exempted.push(saved),
            Err(e) => tracing::warn!(?e, channel = c.get(), "panic: exempting category failed"),
        }
    }
    let ends = crate::app::now_secs() + cfg.duration_secs as i64;
    let payload = json!({
        "lock_mod_commands": cfg.lock_mod_commands, "unlock": cfg.unlock_when_ending,
        "exempt": &exempted, "locked": false
    });
    if let Err(e) = jobs::replace(&app.pool, gid.get() as i64, "panic_end", payload, ends).await {
        // Without its end job panic mode would never end: mod commands stay locked and the
        // categories open. Undo the start so the next trip can try again.
        for saved in &exempted {
            if let Err(e) = unexempt(app, gid, saved).await {
                tracing::warn!(?e, "panic: putting a category back failed");
            }
        }
        app.panic_runs.lock().unwrap().remove(&gid.get());
        return Err(e);
    }
    crate::reply::alert_owner(app, gid, "Panic mode started").await;
    for step in panic_plan(cfg.lock_on_trigger, cfg.lock_mod_commands) {
        match step {
            // A failed step is logged, so the report below is still sent.
            // False: a lock from before panic covers it, and panic's end must leave it. An error
            // can come after the lock was saved, so the end still lifts that one.
            PanicStep::LockServer => match lockdown::lock(app, gid, LockKind::Role).await {
                Ok(false) => {}
                r => {
                    if let Err(e) = r {
                        tracing::warn!(?e, "panic: locking the server failed");
                    }
                    own_lock(app, gid, true).await;
                }
            },
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

/// Records in the `panic_end` job whether the server lock is panic's own, so its end lifts only
/// that one. Only logged on failure.
pub(crate) async fn own_lock(app: &App, gid: Id<GuildMarker>, locked: bool) {
    let r = sqlx::query(
        "UPDATE scheduled_jobs SET payload = payload || jsonb_build_object('locked', $2::bool)
         WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(gid.get() as i64)
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
    for saved in payload["exempt"].as_array().into_iter().flatten() {
        if let Err(e) = unexempt(app, gid, saved).await {
            tracing::warn!(?e, "panic: restoring category overwrite failed");
        }
    }
    // Jobs from before `locked` existed unlock as they did.
    let ours = payload["locked"].as_bool().unwrap_or(true);
    for step in panic_end_plan(ours && payload["unlock"].as_bool().unwrap_or(false)) {
        if step == PanicStep::UnlockServer {
            lockdown::unlock(app, gid, Some(&LockKind::Role)).await?;
        }
    }
    send_log(
        app,
        gid.get(),
        LogKind::Panic,
        Log::new("Panic Mode Ended", DONE).text(&format!("{YES} **Panic mode has ended.**")),
    )
    .await
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
