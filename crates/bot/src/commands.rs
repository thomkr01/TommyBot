//! Prefix mod commands: permit check -> action -> case (+ undo data). ban/kick/timeout/warn.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tommybot_automod::lockdown::{kind_from, LockKind};
use tommybot_core::{cases, jobs, permits, sections::MiscCfg, warn};
use tommybot_logging::{
    embed::{self, duration, ARROW, ORANGE, RED},
    Log, LogKind,
};
use twilight_http::{request::AuditLogReason, Client};
use twilight_model::{
    channel::{message::AllowedMentions, ChannelType},
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
    confirm::{ask_confirm, Action, Pending},
    events::send_log,
    hierarchy::top_position,
    lockdown, modcmds,
    prefix::{Flags, Parsed},
    reply,
    slash::BackupCmd,
    targets,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
    let top = crate::http_err::member(&app.http, gid, target)
        .await?
        .map(|m| top_position(&info.roles, &m.roles));
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

/// Readable reason for a refused permit check.
pub(crate) fn deny_text(perm: &str, target: u64, d: &permits::Denied) -> String {
    let why = match d {
        permits::Denied::NoPermit => format!("you lack the {perm} permit"),
        permits::Denied::TargetProtected => "they are immune".into(),
        permits::Denied::TargetAboveBot => "their top role is above the bot".into(),
        permits::Denied::SelfTarget => "you cannot target yourself".into(),
    };
    format!("Cannot {perm} <@{target}>: {why}")
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
        None,
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
    proof: Option<&str>,
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
        return Ok(deny_text(perm, target.get(), &d));
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, perm).await? {
        return Ok(msg);
    }
    let t = target.get();
    let cfg = app.cfg(gid.get()).await?;
    let soft = flags.soft && kind == ModKind::Ban;
    // One DM before the action: a banned user can't be DMed afterwards.
    let note = appeals::note(&cfg.appeals, &cfg.misc.ban_appeal_message, gid)
        .filter(|_| matches!(kind, ModKind::Ban | ModKind::Timeout) && !soft);
    let told = (flags.dm || cfg.misc.always_dm).then(|| {
        let who = (!cfg.misc.hide_staff_in_dms).then_some(actor_id.get());
        reply::dm_text(perm, reason, who)
    });
    if let Some(text) = [told, note]
        .into_iter()
        .flatten()
        .reduce(|a, b| format!("{a}\n{b}"))
    {
        reply::dm(app, target, &text).await;
    }
    let audit = audit_reason(reason, actor_id);
    let (inverse, points) = match kind {
        ModKind::Ban if soft => {
            // A softban exists to purge messages, so purge a day when the server default is none.
            app.http
                .create_ban(gid, target)
                .delete_message_seconds(cfg.misc.purge_secs().max(86_400))
                .reason(&audit)
                .await?;
            app.http.delete_ban(gid, target).reason(&audit).await?;
            (None, 0)
        }
        ModKind::Ban => {
            // Queued first: a restart right after the ban must not make it permanent. If the ban
            // fails, the unban later finds no ban and is dropped.
            if let Some(d) = duration_s {
                let job = json!({"user":t});
                jobs::enqueue(&app.pool, gid.get() as i64, "unban", job, now_secs() + d).await?;
            }
            app.http
                .create_ban(gid, target)
                .delete_message_seconds(cfg.misc.purge_secs())
                .reason(&audit)
                .await?;
            (Some(json!({"op":"unban","user":t})), 0)
        }
        ModKind::Kick => {
            app.http
                .remove_guild_member(gid, target)
                .reason(&audit)
                .await?;
            (None, 0)
        }
        ModKind::Timeout => {
            timeout_until(
                app,
                gid,
                target,
                now_secs() + duration_s.unwrap_or(cfg.misc.timeout_secs() as i64),
                &audit,
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
            proof,
            inverse,
        },
    )
    .await?;
    let name = if soft { "softban" } else { perm };
    mod_log(
        app,
        gid,
        (case_no, name),
        (actor_id, target),
        duration_s,
        reason,
    )
    .await;
    let mut text = reply::case_text(
        cfg.misc.compact_replies || flags.compact,
        (case_no.into(), if soft { "softban" } else { perm }, t),
        reason,
        actor_id.get(),
    );
    if kind == ModKind::Warn {
        if let Some((extra, _)) = apply_warn_actions(app, &cfg.misc, gid, target, points).await? {
            text += format!("\n{extra}").as_str();
        }
    }
    Ok(text)
}

/// The bot stays silent for quarantined members and in a voice channel's chat (purge excepted).
pub async fn ignored(
    app: &App,
    msg: &MessageCreate,
    gid: Id<GuildMarker>,
    rest: &str,
) -> Result<bool> {
    let held: Option<(i64,)> =
        sqlx::query_as("SELECT 1::bigint FROM quarantines WHERE guild_id=$1 AND user_id=$2")
            .bind(gid.get() as i64)
            .bind(msg.author.id.get() as i64)
            .fetch_optional(&app.pool)
            .await?;
    if held.is_some() {
        return Ok(true);
    }
    if crate::prefix::parse(rest).is_some_and(|p| p.command == "purge") {
        return Ok(false);
    }
    let Ok(ch) = app.http.channel(msg.channel_id).await else {
        return Ok(false);
    };
    Ok(matches!(
        ch.model().await?.kind,
        ChannelType::GuildVoice | ChannelType::GuildStageVoice
    ))
}

/// Every command name [`run`] answers; the invoking message is deleted for these when `delete_invocation` is on.
const KNOWN: &[&str] = &[
    "ban",
    "softban",
    "kick",
    "warn",
    "timeout",
    "verifysetup",
    "lock",
    "unlock",
    "unban",
    "hackban",
    "untimeout",
    "unwarn",
    "quarantine",
    "unquarantine",
    "purge",
    "slowmode",
    "cases",
    "case",
    "verify",
    "sanitize",
    "note",
    "notes",
    "editc",
    "editreason",
    "closecase",
    "caseoff",
    "opencase",
    "caseon",
    "clearwarns",
    "warns",
    "modcases",
];

/// Prefix entry: resolves names, then runs [`apply`] per target and posts each reply.
pub async fn run(app: &App, msg: &MessageCreate, mut p: Parsed) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    p.flags.soft |= p.command == "softban";
    if KNOWN.contains(&p.command.as_str()) {
        let cfg = app.cfg(gid.get()).await?;
        if cfg.misc.delete_invocation {
            let owner = app.guild_info(gid).await?.owner;
            if app.level(gid, owner, msg.author.id).await? != permits::Level::None {
                let r = app.http.delete_message(msg.channel_id, msg.id).await;
                crate::http_err::best_effort(r, "delete command message");
            }
        }
    }
    let kind = match p.command.as_str() {
        "ban" | "softban" | "hackban" => ModKind::Ban,
        "kick" => ModKind::Kick,
        "warn" => ModKind::Warn,
        "timeout" => ModKind::Timeout,
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
            if p.kind.as_deref() == Some("update") {
                let text = p.input.clone().unwrap_or_else(|| p.names.join(", "));
                let text =
                    lockdown::update(app, gid, msg.channel_id, (msg.author.id, &actor), &text)
                        .await?;
                return say(app, msg, &text).await;
            }
            // `lock role`, `lock server`, `lock channel`...: one-off type instead of the dashboard default.
            // Channel mentions lock (or unlock) those channels instead of this one.
            let channels = match p.channels.as_slice() {
                [] => vec![msg.channel_id.get()],
                c => c.to_vec(),
            };
            let name = p.kind.as_ref().or(p.names.first()).map(String::as_str);
            // `-blind` turns a server lock (or the default) into a blind one.
            let name = match (p.flags.blind, name) {
                (true, None | Some("server" | "sc")) => Some("blind"),
                (_, n) => n,
            };
            let name = name.or((!p.channels.is_empty()).then_some("c"));
            let mut kinds = vec![];
            for c in &channels {
                match name.map(|n| kind_from(n, Some(*c))) {
                    Some(Some(k)) if !kinds.contains(&k) => kinds.push(k),
                    Some(Some(_)) => {}
                    Some(None) => return say(app, msg, lockdown::LOCK_TYPES).await,
                    None => {}
                }
            }
            let unlock = p.command == "unlock";
            if name.is_none() {
                if !unlock {
                    // A bare `lock` lists the active locks instead of locking.
                    let e = lockdown::overview(app, gid).await?;
                    return say_embeds(app, msg, "", &[e]).await;
                }
                // A bare `unlock` in a locked channel lifts that channel only.
                let here = LockKind::Channel(msg.channel_id.get());
                if lockdown::active(app, gid).await?.contains(&here) {
                    kinds.push(here);
                }
            }
            let kinds: Vec<Option<LockKind>> = match kinds.is_empty() {
                true => vec![None],
                false => kinds.into_iter().map(Some).collect(),
            };
            let compact = cfg_compact(app, gid, p.flags).await?;
            // Server-wide locks take a while: show a loading reply, then edit in the result.
            let pending = match kinds.as_slice() {
                [k] if !compact && !matches!(k, Some(LockKind::Channel(_))) => {
                    Some(send_reply(app, msg, "", &[lockdown::loading(k.as_ref(), unlock)]).await?)
                }
                _ => None,
            };
            let mut out = vec![];
            for kind in kinds {
                let actor = (msg.author.id, &actor);
                out.push(lockdown::command(app, gid, actor, unlock, kind, p.duration_s).await?);
            }
            if compact {
                let text: Vec<String> = out.into_iter().map(|r| r.text).collect();
                return say(app, msg, &text.join("\n")).await;
            }
            // shortcut: Discord shows at most 10 embeds, so more than 10 channels show the first 10.
            let embeds: Vec<_> = out.into_iter().take(10).map(|r| r.embed).collect();
            return match pending {
                Some(m) => {
                    app.http
                        .update_message(msg.channel_id, m.id)
                        .embeds(Some(&embeds))
                        .await?;
                    Ok(())
                }
                None => say_embeds(app, msg, "", &embeds).await,
            };
        }
        "unban" | "untimeout" | "unwarn" | "quarantine" | "unquarantine" | "purge" | "slowmode"
        | "cases" | "case" | "verify" | "sanitize" | "note" | "notes" | "editc" | "editreason"
        | "closecase" | "caseoff" | "opencase" | "caseon" | "clearwarns" | "warns" | "modcases" => {
            return modcmds::run_prefix(app, msg, p).await
        }
        "backups" => return backup_prefix(app, msg, gid, &p).await,
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
    let reason = p.reason.clone();
    let job = Pending::new(
        gid,
        msg.author.id,
        Action::Mod {
            kind,
            targets,
            reason: p.reason,
            duration_s: p.duration_s,
            flags: p.flags,
            proof: None,
        },
    );
    if job.needs_confirm(cfg.misc.confirm) {
        return ask_confirm(app, msg, job).await;
    }
    let texts = job.run(app, owner, cfg.misc.warn_base_points).await?;
    let compact = cfg.misc.compact_replies || p.flags.compact;
    if !compact {
        let e = reply::result_embeds(kind.name(), &texts, reason.as_deref(), msg.author.id.get());
        return say_embeds(app, msg, "", &e).await;
    }
    let text = reply::summary(
        kind.name(),
        compact,
        &texts,
        reason.as_deref(),
        msg.author.id.get(),
    );
    say(app, msg, &text).await
}

async fn cfg_compact(app: &App, gid: Id<GuildMarker>, flags: Flags) -> Result<bool> {
    Ok(flags.compact || app.cfg(gid.get()).await?.misc.compact_replies)
}

/// Discord's audit log caps reasons at 512 characters.
pub(crate) fn audit_reason(reason: Option<&str>, actor: Id<UserMarker>) -> String {
    embed::clip(
        &format!(
            "{} | Moderator: {actor}",
            reason.unwrap_or("No reason provided")
        ),
        500,
    )
}

/// Mod log: no title, Case / Type / Moderator columns, then Target, Duration and Reason.
pub(crate) async fn mod_log(
    app: &App,
    gid: Id<GuildMarker>,
    (case_no, name): (i32, &str),
    (actor, target): (Id<UserMarker>, Id<UserMarker>),
    duration_s: Option<i64>,
    reason: Option<&str>,
) {
    let log = mod_log_embed(
        (case_no, name),
        (actor.get(), target.get()),
        duration_s,
        reason,
    );
    if let Err(e) = send_log(app, gid.get(), LogKind::ModCommand, log).await {
        tracing::warn!(?e, "mod log failed");
    }
}

pub(crate) fn mod_log_embed(
    (case_no, name): (i32, &str),
    (actor, target): (u64, u64),
    duration_s: Option<i64>,
    reason: Option<&str>,
) -> Log {
    let mut log = Log::new("", case_color(name))
        .inline("Case:", &format!("`{case_no}`"))
        .inline("Type:", &format!("`{}`", case_title(name)))
        .inline("Moderator:", &embed::user(actor))
        .field("Target:", &format!("{ARROW} {}", embed::user(target)));
    if let Some(d) = duration_s {
        log = log.field("Duration:", &format!("`{}`", duration(d)));
    }
    log.field("Reason:", reason.unwrap_or("No reason provided"))
}

fn case_title(kind: &str) -> String {
    let mut c = kind.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn case_color(kind: &str) -> u32 {
    match kind {
        "ban" | "softban" => RED,
        _ => ORANGE,
    }
}

pub(crate) async fn apply_warn_actions(
    app: &App,
    misc: &MiscCfg,
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
    added: i32,
) -> Result<Option<(String, bool)>> {
    if !misc.auto_warn_actions {
        return Ok(None);
    }
    let (gid, uid) = (g.get() as i64, u.get() as i64);
    let after = cases::warn_points(&app.pool, gid, uid).await?;
    let Some(t) = warn::crossed(&misc.warn_actions, after - added, after) else {
        return Ok(warn::next(&misc.warn_actions, after).map(|n| {
            (
                format!("`{}` points away from `{:?}`", n.points - after, n.action),
                false,
            )
        }));
    };
    let why = format!("Reached {} warn points", t.points);
    let (kind, secs) = match t.action {
        warn::Action::Ban => {
            app.http.create_ban(g, u).reason(&why).await?;
            ("ban", None)
        }
        warn::Action::Kick => {
            app.http.remove_guild_member(g, u).reason(&why).await?;
            ("kick", None)
        }
        warn::Action::Timeout { secs } => {
            let secs = match secs {
                0 => misc.timeout_secs() as i64,
                s => s,
            };
            timeout(&app.http, g, u, secs, &why).await?;
            ("timeout", Some(secs))
        }
    };
    let case_no = cases::create(
        &app.pool,
        cases::NewCase {
            guild_id: gid,
            kind,
            user_id: uid,
            mod_id: app.me.get() as i64,
            reason: Some(&why),
            points: 0,
            duration_s: secs,
            proof: None,
            inverse: None,
        },
    )
    .await?;
    mod_log(
        app,
        g,
        (case_no, kind),
        (app.me.cast(), u),
        secs,
        Some(&why),
    )
    .await;
    // The bot clears the warns once the punishment is applied.
    cases::reset_warns(&app.pool, gid, uid).await?;
    Ok(Some((
        format!("<@{uid}> reached {} warn points: {:?}", t.points, t.action),
        kind == "timeout",
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
    reason: &str,
) -> Result<()> {
    let until = Timestamp::from_secs(now_secs() + timeout_chunk(secs).0)?;
    http.update_guild_member(g, u)
        .communication_disabled_until(Some(until))
        .reason(reason)
        .await?;
    Ok(())
}

/// Applies the next 28-day chunk of a timeout ending at `until` and queues a renewal when more remains.
pub async fn timeout_until(
    app: &App,
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
    until: i64,
    reason: &str,
) -> Result<()> {
    let now = now_secs();
    let (chunk, more) = timeout_chunk(until - now);
    if chunk <= 0 {
        return Ok(());
    }
    app.http
        .update_guild_member(g, u)
        .communication_disabled_until(Some(Timestamp::from_secs(now + chunk)?))
        .reason(reason)
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
    say_embeds(app, msg, text, &[]).await
}

pub(crate) async fn say_embeds(
    app: &App,
    msg: &MessageCreate,
    text: &str,
    embeds: &[twilight_model::channel::message::Embed],
) -> Result<()> {
    send_reply(app, msg, text, embeds).await.map(drop)
}

/// Sends a reply (deleted later if the server wants that) and returns it.
async fn send_reply(
    app: &App,
    msg: &MessageCreate,
    text: &str,
    embeds: &[twilight_model::channel::message::Embed],
) -> Result<twilight_model::channel::Message> {
    let sent = app
        .http
        .create_message(msg.channel_id)
        .content(text)
        .embeds(embeds)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?
        .model()
        .await?;
    if let Some(gid) = msg.guild_id {
        if let Some(secs) = app.cfg(gid.get()).await?.misc.delete_replies_secs {
            reply::delete_later(app, gid, msg.channel_id, sent.id, secs).await?;
        }
    }
    Ok(sent)
}

/// `t!backups ID|take|load ID|delete ID|clear` and its aliases.
async fn backup_prefix(
    app: &App,
    msg: &MessageCreate,
    gid: Id<GuildMarker>,
    p: &Parsed,
) -> Result<()> {
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, msg.author.id).await?;
    if actor == permits::Level::None {
        return Ok(());
    }
    let mut args = msg.content.split_whitespace().skip(1);
    // `bload dh0mfu`-style aliases carry the subcommand in `p.kind`.
    let sub = p
        .kind
        .clone()
        .or_else(|| args.next().map(str::to_lowercase));
    let id = args.next().map(str::to_owned);
    let cmd = match (sub.as_deref().unwrap_or("list"), id) {
        ("take" | "create", _) => BackupCmd::Create,
        ("list", _) => BackupCmd::List,
        ("clear", _) => BackupCmd::Clear,
        ("load", Some(id)) => BackupCmd::Load {
            id,
            confirm: msg.content.contains("?confirm"),
        },
        ("delete", Some(id)) => BackupCmd::Delete { id },
        (key, None) if key != "load" && key != "delete" => BackupCmd::Info { id: key.into() },
        _ => {
            return say(
                app,
                msg,
                "Usage: `backups [ID | take | load ID | delete ID | clear]`",
            )
            .await
        }
    };
    let text = backup_cmd(app, gid, (msg.author.id, &actor), cmd).await?;
    say(app, msg, &text).await
}

/// `/backups create|list|load|delete|purge` and `t!backups ID|take|load|delete|clear`. `load`
/// previews unless `confirm` is set.
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
        BackupCmd::Create => match backup::save(app, gid, Some(actor_id.get())).await {
            Ok(code) => format!("Saved backup `{code}`"),
            Err(e) => e.to_string(),
        },
        BackupCmd::List => {
            let rows = backup::list(app, gid).await?;
            if rows.is_empty() {
                "No backups yet".into()
            } else {
                rows.iter()
                    .map(|(code, at, r, c, by)| {
                        let by = by.map_or("TommyBot".into(), |u| format!("<@{u}>"));
                        format!("`{code}` {by} <t:{at}:R>: {r} roles, {c} channels")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        BackupCmd::Info { id } => backup::info(app, gid, &id).await?,
        BackupCmd::Load { id, confirm } => backup::load(app, gid, &id, confirm).await?,
        BackupCmd::Delete { id } => match backup::delete(app, gid, Some(&id)).await? {
            true => format!("Deleted backup `{id}`"),
            false => format!("No backup `{id}`"),
        },
        BackupCmd::Clear => {
            backup::delete(app, gid, None).await?;
            "Deleted all backups".into()
        }
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
    let r = antinuke::revert_actor(app, gid, target, now_secs() - since_s, None).await?;
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
    use super::{above_bot, timeout_chunk, Flags, Id, ModKind, MAX_TIMEOUT_S};
    use crate::confirm::Action;
    use crate::modcmds::ModCmd;

    #[test]
    fn pending_action_survives_the_database_round_trip() {
        let flags = Flags {
            dm: true,
            soft: true,
            compact: false,
            ..Default::default()
        };
        let a = Action::Mod {
            kind: ModKind::Ban,
            targets: vec![Id::new(42)],
            reason: Some("spam".into()),
            duration_s: Some(60),
            flags,
            proof: Some("https://x/y.png".into()),
        };
        let Action::Mod {
            kind,
            targets,
            proof,
            ..
        } = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap()
        else {
            panic!("variant changed")
        };
        assert_eq!((kind, targets), (ModKind::Ban, vec![Id::new(42)]));
        assert_eq!(proof.as_deref(), Some("https://x/y.png"));
        let purge = ModCmd::Purge {
            count: 5,
            user: None,
            from: Some(crate::modcmds::Anchor::Until(9)),
            filters: vec![],
        };
        let e = Action::Ext {
            cmd: purge.clone(),
            channel: Id::new(7),
            reason: None,
            proof: None,
        };
        let Action::Ext { cmd, channel, .. } =
            serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap()
        else {
            panic!("variant changed")
        };
        assert_eq!((cmd, channel), (purge, Id::new(7)));
    }

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
