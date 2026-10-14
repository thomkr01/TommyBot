//! Prefix mod commands: permit check -> action -> case (+ undo data). ban/kick/timeout/warn.
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;
use tommybot_automod::lockdown::{kind_from, LockKind};
use tommybot_core::{cases, jobs, permits, sections::MiscCfg, warn};
use twilight_http::Client;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, MessageFlags,
    },
    gateway::payload::incoming::MessageCreate,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
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
    lockdown, modcmds,
    prefix::{Flags, Parsed},
    reply,
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

pub(crate) async fn target_above_bot(
    app: &App,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
) -> Result<bool> {
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
pub(crate) async fn gated(
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
    actor: (Id<UserMarker>, &permits::Level),
    kind: ModKind,
    target: Id<UserMarker>,
    reason: Option<&str>,
    duration_s: Option<i64>,
    points: i32,
) -> Result<String> {
    apply_dm(
        app,
        gid,
        owner,
        actor,
        kind,
        target,
        reason,
        duration_s,
        points,
        Flags::default(),
    )
    .await
}

/// [`apply`] with the prefix flags: `dm` tells the target first, `soft` turns a ban into a softban, `compact` shortens the reply.
#[allow(clippy::too_many_arguments)]
pub async fn apply_dm(
    app: &App,
    gid: Id<GuildMarker>,
    owner: Id<UserMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    kind: ModKind,
    target: Id<UserMarker>,
    reason: Option<&str>,
    duration_s: Option<i64>,
    points: i32,
    flags: Flags,
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
    let soft = flags.soft && kind == ModKind::Ban;
    if matches!(kind, ModKind::Ban | ModKind::Timeout) && !soft {
        appeals::dm_link(app, &cfg.appeals, gid, target).await; // before the ban: a banned user can't be DMed
    }
    if flags.dm || cfg.misc.always_dm {
        let who = (!cfg.misc.hide_staff_in_dms).then_some(actor_id.get());
        reply::dm(app, target, &reply::dm_text(perm, reason, who)).await;
    }
    let (inverse, points) = match kind {
        ModKind::Ban if soft => {
            // A softban exists to purge messages, so purge a day when the server default is none.
            app.http
                .create_ban(gid, target)
                .delete_message_seconds(cfg.misc.purge_secs().max(86_400))
                .await?;
            app.http.delete_ban(gid, target).await?;
            (None, 0)
        }
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
            kind: if soft { "softban" } else { perm },
            user_id: t as i64,
            mod_id: actor_id.get() as i64,
            reason,
            points,
            duration_s,
            inverse,
        },
    )
    .await?;
    let mut text = reply::case_text(
        cfg.misc.compact_replies || flags.compact,
        (case_no.into(), if soft { "softban" } else { perm }, t),
        reason,
        actor_id.get(),
    );
    if kind == ModKind::Warn {
        if let Some(extra) = apply_warn_actions(app, &cfg.misc, gid, target, points).await? {
            text += &format!("\n{extra}");
        }
    }
    Ok(text)
}

/// Prefix entry: resolves names, then runs [`apply`] per target and posts each reply.
pub async fn run(app: &App, msg: &MessageCreate, mut p: Parsed) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    p.flags.soft |= p.command == "softban";
    let kind = match p.command.as_str() {
        "ban" | "softban" => ModKind::Ban,
        "kick" => ModKind::Kick,
        "warn" => ModKind::Warn,
        "timeout" | "mute" => ModKind::Timeout,
        "verifysetup" => {
            if !app.is_immune(gid, msg.author.id).await? {
                return Ok(());
            }
            let text = crate::verify::setup(app, gid).await?;
            return say(app, msg, &text).await;
        }
        "lock" | "unlock" => {
            let owner = app.guild_info(gid).await?.owner;
            let actor = app.level(gid, owner, msg.author.id).await?;
            // `lock role`, `lock server`, `lock channel`...: one-off type instead of the dashboard default.
            let kind = match p.names.first() {
                Some(n) => match kind_from(n, Some(msg.channel_id.get())) {
                    Some(k) => Some(k),
                    None => return say(app, msg, LOCK_TYPES).await,
                },
                None => None,
            };
            let unlock = p.command == "unlock";
            let text = lockdown_cmd(app, gid, &actor, unlock, kind, p.duration_s).await?;
            return say(app, msg, &text).await;
        }
        "unban" | "hackban" | "untimeout" | "unmute" | "unwarn" | "quarantine" | "unquarantine"
        | "purge" | "clear" | "slowmode" | "cases" | "case" | "verify" | "sanitize" | "dehoist"
        | "note" | "notes" => return modcmds::run_prefix(app, msg, p).await,
        _ => return Ok(()),
    };
    let owner = app.guild_info(gid).await?.owner;
    let cfg = app.cfg(gid.get()).await?;
    if cfg.misc.ignore_no_permit
        && app.level(gid, owner, msg.author.id).await? == permits::Level::None
    {
        return Ok(());
    }
    if cfg.misc.delete_invocation
        && app.level(gid, owner, msg.author.id).await? != permits::Level::None
    {
        app.http.delete_message(msg.channel_id, msg.id).await.ok();
    }
    let mut targets: Vec<Id<UserMarker>> = p.targets.iter().map(|t| Id::new(*t)).collect();
    for n in &p.names {
        match targets::resolve(app, gid, n).await? {
            Ok(id) => targets.push(id),
            Err(e) => say(app, msg, &e).await?,
        }
    }
    let job = Pending {
        gid,
        actor: msg.author.id,
        kind,
        targets,
        reason: p.reason,
        duration_s: p.duration_s,
        flags: p.flags,
        at: Instant::now(),
    };
    if cfg.misc.confirm && kind != ModKind::Warn && !job.targets.is_empty() {
        return ask_confirm(app, msg, job).await;
    }
    for text in job.run(app, owner, cfg.misc.warn_base_points).await? {
        say(app, msg, &text).await?;
    }
    Ok(())
}

/// A prefix command waiting for its Confirm click. Lost on restart: the staff member just types it again.
pub struct Pending {
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    kind: ModKind,
    targets: Vec<Id<UserMarker>>,
    reason: Option<String>,
    duration_s: Option<i64>,
    flags: Flags,
    at: Instant,
}

const CONFIRM_TTL: Duration = Duration::from_secs(60);

impl Pending {
    /// One reply per target.
    async fn run(&self, app: &App, owner: Id<UserMarker>, points: i32) -> Result<Vec<String>> {
        let mut out = vec![];
        for &t in &self.targets {
            // Fresh level each time: a rate-limit quarantine mid-list must stop the rest.
            let actor = app.level(self.gid, owner, self.actor).await?;
            let text = match apply_dm(
                app,
                self.gid,
                owner,
                (self.actor, &actor),
                self.kind,
                t,
                self.reason.as_deref(),
                self.duration_s,
                points,
                self.flags,
            )
            .await
            {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(?e, "mod command failed");
                    format!("Failed to {} <@{t}>, see logs", self.kind.name())
                }
            };
            out.push(text);
        }
        Ok(out)
    }
}

fn confirm_button(label: &str, id: String, style: ButtonStyle) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

async fn ask_confirm(app: &App, msg: &MessageCreate, job: Pending) -> Result<()> {
    let what = if job.flags.soft && job.kind == ModKind::Ban {
        "softban"
    } else {
        job.kind.name()
    };
    let who: Vec<String> = job.targets.iter().map(|t| format!("<@{t}>")).collect();
    let text = format!("Really {what} {}?", who.join(", "));
    let nonce = msg.id.get();
    let row = Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            confirm_button(
                "Confirm",
                format!("{CONFIRM}{nonce}:y"),
                ButtonStyle::Danger,
            ),
            confirm_button(
                "Cancel",
                format!("{CONFIRM}{nonce}:n"),
                ButtonStyle::Secondary,
            ),
        ],
    });
    {
        let mut pending = app.pending.lock().unwrap();
        pending.retain(|_, p| p.at.elapsed() < CONFIRM_TTL);
        pending.insert(nonce, job);
    }
    app.http
        .create_message(msg.channel_id)
        .content(&text)
        .components(&[row])
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

const CONFIRM: &str = "confirm:";

/// Handles the Confirm / Cancel buttons. Returns false for other interactions.
pub async fn handle_confirm(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some((nonce, yes)) = c
        .custom_id
        .strip_prefix(CONFIRM)
        .and_then(|r| r.split_once(':'))
        .and_then(|(n, a)| Some((n.parse::<u64>().ok()?, a == "y")))
    else {
        return Ok(false);
    };
    let client = app.http.interaction(Id::new(app.me.get()));
    let respond = |kind, content: Option<&str>, flags| InteractionResponse {
        kind,
        data: Some(InteractionResponseData {
            content: content.map(Into::into),
            flags,
            ..Default::default()
        }),
    };
    let taken = {
        let mut pending = app.pending.lock().unwrap();
        match pending.get(&nonce) {
            Some(p) if Some(p.actor) != i.author_id() => {
                Err("Only the staff member who typed the command can answer.")
            }
            Some(p) if p.at.elapsed() >= CONFIRM_TTL => {
                pending.remove(&nonce);
                Err("This confirmation expired, type the command again.")
            }
            Some(_) => Ok(pending.remove(&nonce).unwrap()),
            None => Err("This confirmation expired, type the command again."),
        }
    };
    let job = match taken {
        Ok(j) => j,
        Err(e) => {
            let r = respond(
                InteractionResponseType::ChannelMessageWithSource,
                Some(e),
                Some(MessageFlags::EPHEMERAL),
            );
            client.create_response(i.id, &i.token, &r).await?;
            return Ok(true);
        }
    };
    // Several HTTP calls can exceed the 3s answer window, so acknowledge first.
    let ack = respond(InteractionResponseType::DeferredUpdateMessage, None, None);
    client.create_response(i.id, &i.token, &ack).await?;
    let text = if yes {
        let owner = app.guild_info(job.gid).await?.owner;
        let points = app.cfg(job.gid.get()).await?.misc.warn_base_points;
        let mut t = job.run(app, owner, points).await?.join("\n");
        t.truncate(t.floor_char_boundary(2000));
        t
    } else {
        "Cancelled.".into()
    };
    client
        .update_response(&i.token)
        .content(Some(&text))
        .components(Some(&[]))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(true)
}

pub(crate) async fn apply_warn_actions(
    app: &App,
    misc: &MiscCfg,
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
    added: i32,
) -> Result<Option<String>> {
    if !misc.auto_warn_actions {
        return Ok(None);
    }
    let (gid, uid) = (g.get() as i64, u.get() as i64);
    let after = cases::warn_points(&app.pool, gid, uid).await?;
    let Some(t) = warn::crossed(&misc.warn_actions, after - added, after) else {
        return Ok(None);
    };
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

pub(crate) async fn say(app: &App, msg: &MessageCreate, text: &str) -> Result<()> {
    let sent = app
        .http
        .create_message(msg.channel_id)
        .content(text)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?
        .model()
        .await?;
    if let Some(gid) = msg.guild_id {
        if let Some(secs) = app.cfg(gid.get()).await?.misc.delete_replies_secs {
            reply::delete_later(app, gid, msg.channel_id, sent.id, secs).await?;
        }
    }
    Ok(())
}

pub const LOCK_TYPES: &str = "Lock type: server, blind, role, channel, kick, ban or all";

/// Lock or unlock the server; shared by the prefix and slash commands. `kind` overrides the dashboard's lock type.
pub async fn lockdown_cmd(
    app: &App,
    gid: Id<GuildMarker>,
    actor: &permits::Level,
    unlock: bool,
    kind: Option<LockKind>,
    duration_s: Option<i64>,
) -> Result<String> {
    if !actor.has("lockdown") {
        return Ok("Cannot lock: no permit".into());
    }
    if unlock {
        lockdown::unlock(app, gid).await?;
        return Ok("Server unlocked".into());
    }
    let kind = match kind {
        Some(k) => k,
        None => app.cfg(gid.get()).await?.lockdown_prefs.kind(),
    };
    let done = lockdown::lock(app, gid, kind).await?;
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
