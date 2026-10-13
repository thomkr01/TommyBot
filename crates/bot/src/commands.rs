//! Prefix mod commands: permit check -> action -> case (+ undo data). ban/kick/timeout/warn.
use anyhow::Result;
use serde_json::json;
use tommybot_core::{cases, jobs, permits, warn};
use twilight_http::Client;
use twilight_model::{
    gateway::payload::incoming::MessageCreate,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

use crate::{
    antinuke,
    app::{now_secs, App},
    appeals, backup,
    hierarchy::top_position,
    lockdown,
    prefix::Parsed,
    slash::BackupCmd,
    targets,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModKind {
    Ban,
    Kick,
    Timeout,
    Warn,
}

impl ModKind {
    /// Doubles as the permit name, the rate-limit action and the case kind.
    pub fn name(self) -> &'static str {
        match self {
            ModKind::Ban => "ban",
            ModKind::Kick => "kick",
            ModKind::Timeout => "timeout",
            ModKind::Warn => "warn",
        }
    }
}

/// The target outranks the bot when its top role is not below the bot's (the guild owner always does).
fn above_bot(target_top: Option<i64>, bot_top: i64, target_is_owner: bool) -> bool {
    target_is_owner || target_top.is_some_and(|t| t >= bot_top)
}

async fn target_above_bot(app: &App, gid: Id<GuildMarker>, target: Id<UserMarker>) -> Result<bool> {
    let info = app.guild_info(gid).await?;
    if target == info.owner {
        return Ok(true);
    }
    let top = match app.http.guild_member(gid, target).await {
        Ok(m) => Some(top_position(&info.roles, &m.model().await?.roles)),
        Err(_) => None, // not in the server
    };
    Ok(above_bot(top, info.bot_top, false))
}

/// Refuses while panic mode locks mod commands; on a rate-limit breach the actor is quarantined.
async fn gated(
    app: &App,
    gid: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
    actor: &permits::Level,
    action: &str,
) -> Result<Option<String>> {
    if app.mod_commands_locked(gid.get()) && !actor.immune() {
        return Ok(Some("Mod commands are locked during panic mode".into()));
    }
    if app.command_allowed(actor_id, actor, action) {
        return Ok(None);
    }
    antinuke::quarantine_actor(app, gid, actor_id, "command rate limit exceeded").await?;
    Ok(Some(format!(
        "Command rate limit hit for `{action}`: nothing done, you have been quarantined"
    )))
}

/// Permit check -> hierarchy -> rate limit -> action -> case. Returns the reply text; Err only for
/// infrastructure failures. `points` only matters for warns.
#[allow(clippy::too_many_arguments)]
pub async fn apply(
    app: &App,
    gid: Id<GuildMarker>,
    owner: Id<UserMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    kind: ModKind,
    target: Id<UserMarker>,
    reason: Option<&str>,
    duration_s: Option<i64>,
    points: i32,
) -> Result<String> {
    let perm = kind.name();
    let target_level = app.level(gid, owner, target).await?;
    let above = kind != ModKind::Warn && target_above_bot(app, gid, target).await?;
    if let Err(d) = permits::check(
        actor,
        perm,
        actor_id.get() as i64,
        &target_level,
        target.get() as i64,
        above,
    ) {
        return Ok(format!("Cannot {perm} <@{target}>: {d:?}"));
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, perm).await? {
        return Ok(msg);
    }
    let t = target.get();
    let cfg = app.cfg(gid.get()).await?;
    if matches!(kind, ModKind::Ban | ModKind::Timeout) {
        appeals::dm_link(app, &cfg.appeals, gid, target).await; // before the ban: a banned user can't be DMed
    }
    let (inverse, points) = match kind {
        ModKind::Ban => {
            app.http
                .create_ban(gid, target)
                .delete_message_seconds(cfg.misc.purge_secs())
                .await?;
            if let Some(d) = duration_s {
                let job = json!({"user":t});
                jobs::enqueue(&app.pool, gid.get() as i64, "unban", job, now_secs() + d).await?;
            }
            (Some(json!({"op":"unban","user":t})), 0)
        }
        ModKind::Kick => {
            app.http.remove_guild_member(gid, target).await?;
            (None, 0)
        }
        ModKind::Timeout => {
            timeout_until(
                app,
                gid,
                target,
                now_secs() + duration_s.unwrap_or(cfg.misc.timeout_secs() as i64),
            )
            .await?;
            (Some(json!({"op":"untimeout","user":t})), 0)
        }
        ModKind::Warn => (None, points),
    };
    let case_no = cases::create(
        &app.pool,
        cases::NewCase {
            guild_id: gid.get() as i64,
            kind: perm,
            user_id: t as i64,
            mod_id: actor_id.get() as i64,
            reason,
            points,
            duration_s,
            inverse,
        },
    )
    .await?;
    let mut text = format!("Case #{case_no}: {perm} <@{t}>");
    if kind == ModKind::Warn && cfg.misc.auto_warn_actions {
        if let Some(extra) = apply_warn_thresholds(app, gid.get() as i64, t as i64, points).await? {
            text += &format!("\n{extra}");
        }
    }
    Ok(text)
}

/// Prefix entry: resolves names, then runs [`apply`] per target and posts each reply.
pub async fn run(app: &App, msg: &MessageCreate, p: Parsed) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    let kind = match p.command.as_str() {
        "ban" => ModKind::Ban,
        "kick" => ModKind::Kick,
        "warn" => ModKind::Warn,
        "timeout" | "mute" => ModKind::Timeout,
        "lock" | "unlock" => {
            let owner = app.guild_info(gid).await?.owner;
            let actor = app.level(gid, owner, msg.author.id).await?;
            let text = lockdown_cmd(app, gid, &actor, p.command == "unlock", p.duration_s).await?;
            return say(app, msg, &text).await;
        }
        _ => return Ok(()),
    };
    let owner = app.guild_info(gid).await?.owner;
    let cfg = app.cfg(gid.get()).await?;
    if cfg.misc.ignore_no_permit
        && app.level(gid, owner, msg.author.id).await? == permits::Level::None
    {
        return Ok(());
    }
    let mut targets: Vec<Id<UserMarker>> = p.targets.iter().map(|t| Id::new(*t)).collect();
    for n in &p.names {
        match targets::resolve(app, gid, n).await? {
            Ok(id) => targets.push(id),
            Err(e) => say(app, msg, &e).await?,
        }
    }
    for t in targets {
        // Fresh level each time: a rate-limit quarantine mid-list must stop the rest.
        let actor = app.level(gid, owner, msg.author.id).await?;
        let text = match apply(
            app,
            gid,
            owner,
            (msg.author.id, &actor),
            kind,
            t,
            p.reason.as_deref(),
            p.duration_s,
            cfg.misc.warn_base_points,
        )
        .await
        {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!(?e, "mod command failed");
                format!("Failed to {} <@{t}>, see logs", kind.name())
            }
        };
        say(app, msg, &text).await?;
    }
    Ok(())
}

async fn apply_warn_thresholds(
    app: &App,
    gid: i64,
    uid: i64,
    added: i32,
) -> Result<Option<String>> {
    let rows: Vec<(i32, String, Option<i64>)> = sqlx::query_as(
        "SELECT points, action, duration_s FROM warn_thresholds WHERE guild_id = $1",
    )
    .bind(gid)
    .fetch_all(&app.pool)
    .await?;
    let ts: Vec<warn::Threshold> = rows
        .into_iter()
        .filter_map(|(points, a, d)| {
            let action = match a.as_str() {
                "timeout" => warn::Action::Timeout { secs: d? },
                "kick" => warn::Action::Kick,
                "ban" => warn::Action::Ban,
                _ => return None,
            };
            Some(warn::Threshold { points, action })
        })
        .collect();
    let after = cases::warn_points(&app.pool, gid, uid).await?;
    let Some(t) = warn::crossed(&ts, after - added, after) else {
        return Ok(None);
    };
    let (g, u) = (Id::new(gid as u64), Id::<UserMarker>::new(uid as u64));
    match t.action {
        warn::Action::Ban => {
            app.http.create_ban(g, u).await?;
        }
        warn::Action::Kick => {
            app.http.remove_guild_member(g, u).await?;
        }
        warn::Action::Timeout { secs } => timeout(&app.http, g, u, secs).await?,
    }
    cases::reset_warns(&app.pool, gid, uid).await?;
    Ok(Some(format!(
        "<@{uid}> reached {} warn points: {:?}",
        t.points, t.action
    )))
}

/// Discord rejects timeouts longer than 28 days.
const MAX_TIMEOUT_S: i64 = 28 * 86_400;

/// Seconds to apply now, and whether a renewal must follow.
fn timeout_chunk(remaining: i64) -> (i64, bool) {
    (remaining.min(MAX_TIMEOUT_S), remaining > MAX_TIMEOUT_S)
}

/// Single capped timeout for callers that cannot renew (warn thresholds, automod).
pub async fn timeout(
    http: &Client,
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
    secs: i64,
) -> Result<()> {
    let until = Timestamp::from_secs(now_secs() + timeout_chunk(secs).0)?;
    http.update_guild_member(g, u)
        .communication_disabled_until(Some(until))
        .await?;
    Ok(())
}

/// Applies the next 28-day chunk of a timeout ending at `until` and queues a renewal when more remains.
pub async fn timeout_until(
    app: &App,
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
    until: i64,
) -> Result<()> {
    let now = now_secs();
    let (chunk, more) = timeout_chunk(until - now);
    if chunk <= 0 {
        return Ok(());
    }
    app.http
        .update_guild_member(g, u)
        .communication_disabled_until(Some(Timestamp::from_secs(now + chunk)?))
        .await?;
    if more {
        // Renewed a day early so scheduler lag cannot open a gap.
        let job = json!({"user":u.get(),"until_secs":until});
        jobs::enqueue(
            &app.pool,
            g.get() as i64,
            "timeout_renew",
            job,
            now + chunk - 86_400,
        )
        .await?;
    }
    Ok(())
}

async fn say(app: &App, msg: &MessageCreate, text: &str) -> Result<()> {
    app.http
        .create_message(msg.channel_id)
        .content(text)
        .await?;
    Ok(())
}

/// Lock or unlock the server; shared by the prefix and slash commands.
pub async fn lockdown_cmd(
    app: &App,
    gid: Id<GuildMarker>,
    actor: &permits::Level,
    unlock: bool,
    duration_s: Option<i64>,
) -> Result<String> {
    if !actor.has("lockdown") {
        return Ok("Cannot lock: no permit".into());
    }
    if unlock {
        lockdown::unlock(app, gid).await?;
        return Ok("Server unlocked".into());
    }
    let done = lockdown::lock(app, gid, tommybot_automod::lockdown::LockKind::Server).await?;
    if done {
        if let Some(d) = duration_s {
            jobs::enqueue(
                &app.pool,
                gid.get() as i64,
                "unlock",
                json!({}),
                now_secs() + d,
            )
            .await?;
        }
    }
    Ok(if done {
        "Server locked"
    } else {
        "Already locked"
    }
    .into())
}

/// `/backup create|list|load|delete`. `load` previews unless `confirm` is set.
pub async fn backup_cmd(
    app: &App,
    gid: Id<GuildMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    cmd: BackupCmd,
) -> Result<String> {
    if !actor.has("backups") {
        return Ok("Cannot manage backups: no permit".into());
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, "backups").await? {
        return Ok(msg);
    }
    Ok(match cmd {
        BackupCmd::Create => format!("Saved backup #{}", backup::save(app, gid).await?),
        BackupCmd::List => {
            let rows = backup::list(app, gid).await?;
            if rows.is_empty() {
                "No backups yet".into()
            } else {
                rows.iter()
                    .map(|(id, at, r, c)| format!("`#{id}` <t:{at}:R>: {r} roles, {c} channels"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        BackupCmd::Load { id, confirm } => backup::load(app, gid, id, confirm).await?,
        BackupCmd::Delete { id } => match backup::delete(app, gid, id).await? {
            true => format!("Deleted backup #{id}"),
            false => format!("No backup #{id}"),
        },
    })
}

/// `/revert`: undo what `target` did in the last `since_s` seconds.
pub async fn revert_cmd(
    app: &App,
    gid: Id<GuildMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    target: Id<UserMarker>,
    since_s: i64,
) -> Result<String> {
    if !actor.has("revert") {
        return Ok("Cannot revert: no permit".into());
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, "revert").await? {
        return Ok(msg);
    }
    let r = antinuke::revert_actor(app, gid, target, now_secs() - since_s).await?;
    let reinvite: Vec<String> = r.reinvite.iter().map(|u| format!("<@{u}>")).collect();
    Ok(format!(
        "Reverted {}/{} actions by <@{target}> and recreated {} deleted. Re-invite: {}",
        r.ops - r.failed,
        r.ops,
        r.recreated,
        if reinvite.is_empty() {
            "none".into()
        } else {
            reinvite.join(" ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::{above_bot, timeout_chunk, MAX_TIMEOUT_S};

    #[test]
    fn timeout_chunking() {
        assert_eq!(timeout_chunk(600), (600, false));
        assert_eq!(timeout_chunk(MAX_TIMEOUT_S), (MAX_TIMEOUT_S, false));
        assert_eq!(timeout_chunk(MAX_TIMEOUT_S + 1), (MAX_TIMEOUT_S, true));
    }

    #[test]
    fn hierarchy() {
        assert!(above_bot(Some(5), 5, false)); // equal is refused
        assert!(above_bot(Some(9), 5, false));
        assert!(!above_bot(Some(4), 5, false));
        assert!(!above_bot(None, 5, false)); // not in the server
        assert!(above_bot(None, 5, true)); // owner
    }
}
