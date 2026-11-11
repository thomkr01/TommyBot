//! Runs automod lockdown plans against Discord and restores them from the stored snapshot.
use anyhow::Result;
use tommybot_automod::{
    discord_api::MAX_INVITE_PAUSE_MS,
    lockdown::{plan, unlock_ops, JoinLock, LockKind, Op, Plan, RoleInfo, Snapshot, SILENCE},
    store,
};
use tommybot_logging::emoji::LOCKDOWN;
use twilight_model::{
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    events::send_log,
};
use std::time::{Duration, Instant};
use tommybot_logging::{
    embed::{self, ALERT, ARROW, BLUE, DONE, GREEN, NO, RED, YES},
    emoji::{ANNOUNCE, LOCK, MODERATOR, RUNNING, UNLOCK},
    Log, LogKind,
};
use twilight_model::channel::message::Embed;

/// Runs one op; returns the channel it created, if any.
async fn exec(app: &App, gid: Id<GuildMarker>, op: &Op, snap: &Snapshot) -> Result<Option<u64>> {
    match op {
        Op::SetRolePerms { role, perms } => {
            let perms = Permissions::from_bits_truncate(*perms);
            app.http
                .update_role(gid, Id::new(*role))
                .permissions(perms)
                .await?;
        }
        Op::DenyChannelSend { channel, role } => {
            // Other bits of an existing overwrite stay; unlock puts the old one back.
            let (allow, deny) = snap
                .prior
                .iter()
                .find(|p| p.0 == *channel && p.1 == *role)
                .map_or((0, 0), |p| (p.2, p.3));
            let o = PermissionOverwrite {
                id: Id::new(*role),
                kind: PermissionOverwriteType::Role,
                allow: Some(Permissions::from_bits_truncate(allow & !SILENCE)),
                deny: Some(Permissions::from_bits_truncate(deny | SILENCE)),
            };
            app.http
                .update_channel_permission(Id::new(*channel), &o)
                .await?;
        }
        Op::CreateLockedChannel => {
            // @everyone may read it but not write, so members see why the server went quiet.
            let everyone = twilight_model::channel::permission_overwrite::PermissionOverwrite {
                id: gid.cast(),
                kind: twilight_model::channel::permission_overwrite::PermissionOverwriteType::Role,
                allow: Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
                deny: Permissions::SEND_MESSAGES,
            };
            let ch = app
                .http
                .create_guild_channel(gid, "server-locked")
                .permission_overwrites(&[everyone])
                .await?
                .model()
                .await?;
            app.http
                .create_message(ch.id)
                .embeds(&[locked_embed(&[])])
                .await?;
            return Ok(Some(ch.id.get()));
        }
        // No-op: the saved snapshot records the join lock and `join_lock` reads it from there.
        Op::EnableJoinLock(_) => {}
        Op::PauseInvites => {
            let now = now_secs() * 1000;
            app.api
                .pause_invites(gid.get(), Some((now, now + MAX_INVITE_PAUSE_MS)))
                .await?;
        }
    }
    Ok(None)
}

/// Locks the guild unless an active lock already covers the same part (its snapshot would save
/// locked perms).
pub async fn lock(app: &App, gid: Id<GuildMarker>, kind: LockKind) -> Result<bool> {
    lock_for(app, gid, kind, None).await
}

/// [`lock`] that lifts itself after `secs`. The timer is queued before any lock step runs, so a
/// lock that a restart cut off and [`resume`] finished still ends on time.
pub async fn lock_for(
    app: &App,
    gid: Id<GuildMarker>,
    kind: LockKind,
    secs: Option<i64>,
) -> Result<bool> {
    if lock_thread(app, gid, &kind, true).await? {
        return Ok(true);
    }
    let active = store::locks(&app.pool, gid.get()).await?;
    if active
        .iter()
        .filter_map(|s| s.kind.as_ref())
        .any(|k| k.overlaps(&kind))
    {
        return Ok(false);
    }
    let plan = plan_now(app, gid, &kind).await?;
    // Saved first so a failure halfway can still be unlocked exactly, or finished after a restart.
    store::save_lock(&app.pool, gid.get(), &plan.snapshot).await?;
    if let Some(d) = secs {
        let job = serde_json::json!({ "lock": kind });
        tommybot_core::jobs::enqueue(&app.pool, gid.get() as i64, "unlock", job, now_secs() + d)
            .await?;
    }
    apply(app, gid, plan).await?;
    announce(
        app,
        gid,
        Log::new("Server Locked", ALERT)
            .text(&format!("{YES} **The server has been locked down.**")),
    )
    .await;
    Ok(true)
}

/// The lock plan for the guild as it is now.
async fn plan_now(app: &App, gid: Id<GuildMarker>, kind: &LockKind) -> Result<Plan> {
    let cfg = app.cfg(gid.get()).await?;
    let roles = crate::hierarchy::ranked(gid, app.http.roles(gid).await?.models().await?);
    let mine = app
        .http
        .guild_member(gid, app.me)
        .await?
        .model()
        .await?
        .roles;
    let bot_position = roles
        .iter()
        .filter(|r| mine.contains(&r.id))
        .map(|r| r.position as i32)
        .max()
        .unwrap_or(0);
    let info: Vec<RoleInfo> = roles
        .iter()
        .map(|r| RoleInfo {
            id: r.id.get(),
            perms: r.permissions.bits(),
            position: r.position as i32,
            managed: r.managed,
        })
        .collect();
    let main_roles: Vec<u64> = cfg.lockdown.main_roles.iter().map(|r| r.get()).collect();
    let mut plan = plan(
        kind,
        gid.get(),
        &main_roles,
        &info,
        bot_position,
        cfg.lockdown_prefs.voice,
    );
    if let LockKind::Channel(c) = *kind {
        let ch = app.http.channel(Id::new(c)).await?.model().await?;
        plan.snapshot.prior = ch
            .permission_overwrites
            .unwrap_or_default()
            .iter()
            .filter(|o| plan.snapshot.channel_denies.contains(&(c, o.id.get())))
            .map(|o| (c, o.id.get(), o.allow.bits(), o.deny.bits()))
            .collect();
    }
    if !cfg.lockdown_prefs.pause_invites {
        plan.ops.retain(|o| *o != Op::PauseInvites);
        plan.snapshot.invites_paused = false;
    }
    Ok(plan)
}

/// Runs the ops of a saved lock, then marks it done. Every op is safe to run twice.
async fn apply(app: &App, gid: Id<GuildMarker>, mut plan: Plan) -> Result<()> {
    for op in &plan.ops {
        if *op == Op::CreateLockedChannel && plan.snapshot.locked_channel.is_some() {
            continue;
        }
        if let Some(created) = exec(app, gid, op, &plan.snapshot).await? {
            plan.snapshot.locked_channel = Some(created);
            store::save_lock(&app.pool, gid.get(), &plan.snapshot).await?;
        }
    }
    Ok(store::lock_done(&app.pool, gid.get(), &plan.snapshot).await?)
}

/// Finishes the locks and unlocks a restart cut off. A lock keeps its saved snapshot (the live
/// perms may already be half locked) and runs its ops again.
pub async fn resume(app: &App) -> Result<()> {
    for (guild, snap, unlocking) in store::interrupted(&app.pool).await? {
        let gid = Id::new(guild);
        let r = async {
            match (unlocking, &snap.kind) {
                (true, kind) => unlock(app, gid, kind.as_ref()).await.map(drop),
                (false, Some(k)) => {
                    let ops = plan_now(app, gid, k).await?.ops;
                    apply(
                        app,
                        gid,
                        Plan {
                            ops,
                            snapshot: snap.clone(),
                        },
                    )
                    .await
                }
                (false, None) => Ok(store::lock_done(&app.pool, guild, &snap).await?),
            }
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(?e, guild, unlocking, "resuming an interrupted lock failed");
        }
    }
    Ok(())
}

/// Threads cannot have permission overwrites, so a channel lock there locks the thread itself.
/// Returns false when `kind` is not a thread's channel lock.
async fn lock_thread(
    app: &App,
    gid: Id<GuildMarker>,
    kind: &LockKind,
    locked: bool,
) -> Result<bool> {
    let LockKind::Channel(c) = *kind else {
        return Ok(false);
    };
    let c = Id::new(c);
    let ch = app.http.channel(c).await?.model().await?;
    // A typed `<#id>` can point anywhere; never touch a channel of another server.
    anyhow::ensure!(
        ch.guild_id == Some(gid),
        "<#{c}> is not a channel of this server"
    );
    if !ch.kind.is_thread() {
        return Ok(false);
    }
    app.http.update_thread(c).locked(locked).await?;
    Ok(true)
}

/// Kinds of the guild's active locks, oldest first.
pub async fn active(app: &App, gid: Id<GuildMarker>) -> Result<Vec<LockKind>> {
    Ok(store::locks(&app.pool, gid.get())
        .await?
        .into_iter()
        .filter_map(|s| s.kind)
        .collect())
}

/// Join lock of the guild's stored snapshot, if it has one.
pub async fn join_lock(app: &App, gid: Id<GuildMarker>) -> Result<Option<JoinLock>> {
    load_join_lock(&app.pool, gid.get()).await
}

async fn load_join_lock(pool: &sqlx::PgPool, guild: u64) -> Result<Option<JoinLock>> {
    let locks = store::locks(pool, guild).await?;
    Ok(locks
        .iter()
        .find_map(|s| s.kind.as_ref().and_then(join_lock_of)))
}

fn join_lock_of(kind: &LockKind) -> Option<JoinLock> {
    kind.covers().2
}

async fn announce(app: &App, gid: Id<GuildMarker>, log: Log) {
    let on = app
        .cfg(gid.get())
        .await
        .is_ok_and(|c| c.lockdown_prefs.updates);
    if on {
        if let Err(e) = send_log(app, gid.get(), LogKind::ModCommand, log).await {
            tracing::warn!(?e, "lockdown update log failed");
        }
    }
}

/// Lifts the active locks that overlap `which`, or all of them; returns how many were lifted.
pub async fn unlock(app: &App, gid: Id<GuildMarker>, which: Option<&LockKind>) -> Result<usize> {
    if let Some(k) = which {
        if lock_thread(app, gid, k, false).await? {
            return Ok(1);
        }
    }
    let (mut lifted, mut failed) = (0, 0);
    for snap in store::locks(&app.pool, gid.get()).await? {
        let hit = match (which, &snap.kind) {
            (Some(w), Some(k)) => w.overlaps(k),
            (Some(_), None) => false,
            (None, _) => true,
        };
        if !hit {
            continue;
        }
        // Read before the snapshot is taken, so a failed read leaves the lock to retry.
        let current: Vec<(u64, u64)> = app
            .http
            .roles(gid)
            .await?
            .models()
            .await?
            .iter()
            .map(|r| (r.id.get(), r.permissions.bits()))
            .collect();
        if !store::take_lock(&app.pool, gid.get(), &snap).await? {
            continue;
        }
        // The lock is claimed now, so one failed step must not leave the rest locked. A deleted
        // channel or role has nothing to restore.
        let mut step = |r: Result<()>| match r {
            Err(e) if !crate::http_err::gone_any(&e) => {
                tracing::warn!(?e, "unlock step failed");
                failed += 1;
            }
            _ => {}
        };
        for op in unlock_ops(&snap, &current) {
            step(exec(app, gid, &op, &snap).await.map(drop));
        }
        if snap.invites_paused {
            step(app.api.pause_invites(gid.get(), None).await);
        }
        for &(channel, role) in &snap.channel_denies {
            let r = match snap.prior.iter().find(|p| p.0 == channel && p.1 == role) {
                Some(&(_, _, allow, deny)) => {
                    let o = PermissionOverwrite {
                        id: Id::new(role),
                        kind: PermissionOverwriteType::Role,
                        allow: Some(Permissions::from_bits_truncate(allow)),
                        deny: Some(Permissions::from_bits_truncate(deny)),
                    };
                    app.http
                        .update_channel_permission(Id::new(channel), &o)
                        .await
                }
                None => {
                    app.http
                        .delete_channel_permission(Id::new(channel))
                        .role(Id::new(role))
                        .await
                }
            };
            step(r.map(drop).map_err(Into::into));
        }
        if let Some(c) = snap.locked_channel {
            crate::http_err::best_effort(
                app.http.delete_channel(Id::new(c)).await,
                "delete lock channel",
            );
        }
        store::drop_lock(&app.pool, gid.get(), &snap).await?;
        lifted += 1;
    }
    if failed > 0 {
        let log = Log::new("Unlock Incomplete", ALERT)
            .text(&format!("`{failed}` permission changes could not be undone. Check the bot's role and permissions, then fix them by hand."));
        if let Err(e) = send_log(app, gid.get(), LogKind::Server, log).await {
            tracing::warn!(?e, "could not log the incomplete unlock");
        }
    }
    if lifted > 0 {
        if active(app, gid).await?.is_empty() {
            sqlx::query("DELETE FROM lock_updates WHERE guild_id=$1")
                .bind(gid.get() as i64)
                .execute(&app.pool)
                .await?;
        }
        announce(
            app,
            gid,
            Log::new("Server Unlocked", DONE)
                .text(&format!("{YES} **The server has been unlocked.**")),
        )
        .await;
    }
    Ok(lifted)
}

/// `lock MESSAGE ?t update`: adds the message as UPDATE n to the blind lock's notice, or, without a
/// blind lock, posts the last five updates in `channel`.
pub async fn update(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    (actor_id, actor): (Id<UserMarker>, &tommybot_core::permits::Level),
    text: &str,
) -> Result<String> {
    if !actor.has("lock_updates") {
        return Ok("Cannot post lock updates: no permit".into());
    }
    if let Some(why) = crate::commands::gated(app, gid, actor_id, actor, "lock_updates").await? {
        return Ok(why);
    }
    let locks = store::locks(&app.pool, gid.get()).await?;
    if locks.is_empty() {
        return Ok("Nothing is locked".into());
    }
    let g = gid.get() as i64;
    sqlx::query("INSERT INTO lock_updates (guild_id, author_id, message) VALUES ($1,$2,$3)")
        .bind(g)
        .bind(actor_id.get() as i64)
        .bind(embed::clip(text, 500))
        .execute(&app.pool)
        .await?;
    if let Some(ch) = locks.iter().find_map(|s| s.locked_channel) {
        refresh_notice(app, gid, Id::new(ch)).await?;
        return Ok("Lock update added to the lock notice".into());
    }
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT message, EXTRACT(EPOCH FROM created_at)::bigint FROM lock_updates
         WHERE guild_id=$1 ORDER BY id DESC LIMIT 5",
    )
    .bind(g)
    .fetch_all(&app.pool)
    .await?;
    let mut log = Log::new("Lockdown Updates", ALERT);
    for (m, at) in rows.iter().rev() {
        log = log.text(&format!("{ARROW} <t:{at}:R> {m}"));
    }
    let r = app
        .http
        .create_message(channel)
        .embeds(&[log.build()])
        .await;
    crate::http_err::best_effort(r, "post lock update");
    Ok("Lock update posted".into())
}

/// Rewrites the blind lock's notice in `ch` with every stored update as UPDATE n.
async fn refresh_notice(app: &App, gid: Id<GuildMarker>, ch: Id<ChannelMarker>) -> Result<()> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT author_id, message FROM lock_updates WHERE guild_id=$1 ORDER BY id")
            .bind(gid.get() as i64)
            .fetch_all(&app.pool)
            .await?;
    let e = locked_embed(&rows);
    // The channel only holds the notice: nobody else may write there.
    let notice = app
        .http
        .channel_messages(ch)
        .after(Id::new(1))
        .limit(1)
        .await?
        .models()
        .await?;
    match notice.first() {
        Some(m) => {
            app.http
                .update_message(ch, m.id)
                .embeds(Some(std::slice::from_ref(&e)))
                .await?;
        }
        None => {
            app.http
                .create_message(ch)
                .embeds(std::slice::from_ref(&e))
                .await?;
        }
    }
    Ok(())
}

/// Dashboard "Send an update" during a blind lock (job `lock_update`): stores it and refreshes the notice.
pub async fn dashboard_update(app: &App, gid: Id<GuildMarker>, by: u64, text: &str) -> Result<()> {
    let locks = store::locks(&app.pool, gid.get()).await?;
    let Some(ch) = locks.iter().find_map(|s| s.locked_channel) else {
        return Ok(());
    };
    sqlx::query("INSERT INTO lock_updates (guild_id, author_id, message) VALUES ($1,$2,$3)")
        .bind(gid.get() as i64)
        .bind(by as i64)
        .bind(embed::clip(text, 500))
        .execute(&app.pool)
        .await?;
    refresh_notice(app, gid, Id::new(ch)).await
}

/// The red notice in `#server-locked`, with every update (author id, text) numbered, newest last.
/// Only the last six fit, so the numbers keep counting past the dropped ones.
pub fn locked_embed(updates: &[(i64, String)]) -> Embed {
    let mut log = Log::new(&format!("{LOCKDOWN} The server is locked"), RED)
        .text(&format!("{ANNOUNCE} Staff have locked this server down."))
        .text(&format!(
            "{ARROW} You cannot see or talk in the channels until the lockdown ends."
        ))
        .text(&format!(
            "{ARROW} **Hang tight while staff sort everything out.**"
        ));
    let skip = updates.len().saturating_sub(6);
    for (n, (author, text)) in updates.iter().enumerate().skip(skip) {
        log = log.text(&format!(
            "\n📌 **UPDATE {}** (<@{author}>):\n{ARROW} {text}",
            n + 1
        ));
    }
    log.build()
}

fn lock_name(k: &LockKind) -> String {
    match k {
        LockKind::Server => "channels".into(),
        LockKind::Blind => "channels (blind)".into(),
        LockKind::Channel(c) => format!("<#{c}>"),
        LockKind::Role => "roles".into(),
        LockKind::Join(j) => format!("joins ({j:?})").to_lowercase(),
        LockKind::All => "everything".into(),
        LockKind::Mixed {
            channels,
            roles,
            joins,
        } => [
            (*channels, "channels"),
            (*roles, "roles"),
            (*joins, "joins"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(" + "),
    }
}

pub const LOCK_TYPES: &str =
    "Lock type: server (sc), blind, role (r), channel (c), kick (k), ban (b) or all (s)";

/// A lock command's answer: `text` for compact replies, `embed` otherwise.
pub struct Reply {
    pub text: String,
    pub embed: Embed,
}

fn fail(lift: bool, text: String) -> Reply {
    let title = if lift {
        "Unlock Result:"
    } else {
        "Lock Result:"
    };
    let embed = Log::new(title, BLUE)
        .text(&format!("{ARROW} {text}"))
        .build();
    Reply { text, embed }
}

/// Shown while a server-wide lock or unlock runs; replaced by the result.
pub fn loading(kind: Option<&LockKind>, lift: bool) -> Embed {
    let name = kind.map_or("all".into(), type_name);
    let (title, verb) = if lift {
        ("TommyBot Unlocking:", "Disabling")
    } else {
        ("TommyBot Lockdown:", "Enabling")
    };
    Log::new(title, BLUE)
        .text(&format!("{RUNNING} {verb} `{name}` lockdown!"))
        .build()
}

/// `channels (blind)` -> `Channels (blind)`, as the Type line shows it.
fn type_name(k: &LockKind) -> String {
    let n = lock_name(k);
    let mut c = n.chars();
    c.next()
        .map_or(n.clone(), |f| f.to_uppercase().chain(c).collect())
}

/// The finished lock or unlock, with how long it took.
fn finished(
    kind: Option<&LockKind>,
    name: String,
    lift: bool,
    took: Duration,
    actor: Id<UserMarker>,
) -> Reply {
    if let Some(LockKind::Channel(c)) = kind {
        let (color, icon, done, body) = if lift {
            (
                GREEN,
                UNLOCK,
                "unlocked",
                "Everyone now has access to this channel.",
            )
        } else {
            (
                RED,
                LOCK,
                "locked",
                "You will gain access again once the lockdown is lifted.",
            )
        };
        let embed = Log::new("", color)
            .text(&format!("{icon} **<#{c}> has been {done}!**"))
            .text(body)
            .build();
        return Reply {
            text: format!("<#{c}> has been {done}"),
            embed,
        };
    }
    let ms = took.as_secs_f64() * 1000.0;
    let (title, icon, line, ok, verb) = if lift {
        (
            "TommyBot Unlocking:",
            UNLOCK,
            "has been lifted",
            "Unlocking",
            "unlocking",
        )
    } else {
        (
            "TommyBot Lockdown:",
            LOCK,
            "has been successfully enforced",
            "Lockdown",
            "locking down",
        )
    };
    let embed = Log::new(title, GREEN)
        .text(&format!(
            "{icon} `{name}` lockdown mode {line} in `{ms:.0} ms`."
        ))
        .text("")
        .text(&format!("{YES} **{ok} successful!**"))
        .text(&format!(
            "{ARROW} *TommyBot has finished {verb} in `{ms:.2} ms`.*"
        ))
        .text(&format!("{MODERATOR} **Staff:** <@{actor}> `{actor}`"))
        .line("Type", &name)
        .build();
    Reply {
        text: format!("{ok} successful: {name} ({ms:.0} ms)"),
        embed,
    }
}

/// A bare prefix `lock`: the active locks, or how to add one.
pub async fn overview(app: &App, gid: Id<GuildMarker>) -> Result<Embed> {
    let active = active(app, gid).await?;
    let mut log = Log::new("Active lockdown modes:", RED);
    if active.is_empty() {
        let prefix = app.cfg(gid.get()).await?.misc.prefix.clone();
        log = log
            .text(&format!(
                "{NO} There are no active lockdown modes currently!"
            ))
            .text("")
            .text(&format!("{ANNOUNCE} **Add a lockdown:**"))
            .text(&format!(
                "You can add a lockdown by doing `{prefix}lock ?t [TYPE]`"
            ));
    }
    for k in &active {
        log = log.text(&format!("{LOCK} {}", type_name(k)));
    }
    Ok(log.build())
}

/// The first lock permit `kind` needs that `actor` lacks.
fn missing(actor: &tommybot_core::permits::Level, kind: &LockKind) -> Option<&'static str> {
    let (channels, roles, joins) = kind.covers();
    [
        (
            channels || matches!(kind, LockKind::Channel(_)),
            "lock_channels",
        ),
        (roles, "lock_staff_roles"),
        (joins.is_some(), "lock_joins"),
    ]
    .into_iter()
    .find(|(on, p)| *on && !actor.has(p))
    .map(|(_, p)| p)
}

/// Lock or unlock the server; shared by the prefix and slash commands. `kind` overrides the dashboard's lock type.
pub async fn command(
    app: &App,
    gid: Id<GuildMarker>,
    (actor_id, actor): (Id<UserMarker>, &tommybot_core::permits::Level),
    lift: bool,
    kind: Option<LockKind>,
    duration_s: Option<i64>,
) -> Result<Reply> {
    if !actor.has("lockdown") {
        return Ok(fail(lift, "Cannot lock: no permit".into()));
    }
    if let Some(why) = crate::commands::gated(app, gid, actor_id, actor, "lockdown").await? {
        return Ok(fail(lift, why));
    }
    let start = Instant::now();
    if lift {
        // Lifting a lock needs the same permit as setting it.
        let lifts = match &kind {
            Some(k) => vec![k.clone()],
            None => active(app, gid).await?,
        };
        if let Some(p) = lifts.iter().find_map(|k| missing(actor, k)) {
            return Ok(fail(
                lift,
                format!("Cannot unlock: you lack the {p} permit"),
            ));
        }
        // Named before the locks are gone, for the Type line.
        let name = match &kind {
            Some(k) => type_name(k),
            None => active(app, gid)
                .await?
                .iter()
                .map(type_name)
                .collect::<Vec<_>>()
                .join(" + "),
        };
        return Ok(match unlock(app, gid, kind.as_ref()).await? {
            0 => fail(lift, "Nothing to unlock".into()),
            _ => finished(kind.as_ref(), name, lift, start.elapsed(), actor_id),
        });
    }
    let kind = match kind {
        Some(k) => k,
        None => app.cfg(gid.get()).await?.lockdown_prefs.kind(),
    };
    let name = lock_name(&kind);
    if let Some(p) = missing(actor, &kind) {
        return Ok(fail(
            lift,
            format!("Cannot lock {name}: you lack the {p} permit"),
        ));
    }
    Ok(if lock_for(app, gid, kind.clone(), duration_s).await? {
        finished(
            Some(&kind),
            type_name(&kind),
            lift,
            start.elapsed(),
            actor_id,
        )
    } else {
        fail(
            lift,
            format!("Cannot lock {name}: an active lock already covers it"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_replies_read_like_the_panels() {
        assert_eq!(type_name(&LockKind::Server), "Channels");
        let r = finished(
            Some(&LockKind::Channel(5)),
            String::new(),
            true,
            Duration::ZERO,
            Id::new(1),
        );
        assert_eq!(r.text, "<#5> has been unlocked");
        let r = finished(
            Some(&LockKind::Role),
            "Roles".into(),
            false,
            Duration::from_micros(390_110),
            Id::new(1),
        );
        assert_eq!(r.text, "Lockdown successful: Roles (390 ms)");
        assert!(r.embed.description.unwrap().contains("`390.11 ms`"));
    }

    #[test]
    fn lock_notice_numbers_updates_past_the_dropped_ones() {
        let e = locked_embed(&[]);
        assert_eq!(e.color, Some(RED));
        assert!(!e.description.unwrap().contains("UPDATE"));
        let ups: Vec<(i64, String)> = (1..=8).map(|n| (7, format!("note {n}"))).collect();
        let d = locked_embed(&ups).description.unwrap();
        assert!(
            !d.contains("UPDATE 2**")
                && d.contains("**UPDATE 3** (<@7>):")
                && d.contains("UPDATE 8")
        );
        assert!(d.contains("note 8"));
    }

    #[test]
    fn join_lock_maps_from_kind() {
        assert_eq!(
            join_lock_of(&LockKind::Join(JoinLock::Kick)),
            Some(JoinLock::Kick)
        );
        assert_eq!(join_lock_of(&LockKind::All), Some(JoinLock::Ban));
        assert_eq!(join_lock_of(&LockKind::Server), None);
        assert_eq!(join_lock_of(&LockKind::Channel(1)), None);
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn join_lock_reads_saved_snapshot() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let gid = 4_000_000_000_000_000_000 + std::process::id() as u64;
        let p = plan(&LockKind::Join(JoinLock::Kick), gid, &[], &[], 0, false);
        store::save_lock(&pool, gid, &p.snapshot).await.unwrap();
        assert_eq!(
            load_join_lock(&pool, gid).await.unwrap(),
            Some(JoinLock::Kick)
        );
        store::take_lock(&pool, gid, &p.snapshot).await.unwrap();
        assert_eq!(load_join_lock(&pool, gid).await.unwrap(), None);
        store::drop_lock(&pool, gid, &p.snapshot).await.unwrap();
    }
}
