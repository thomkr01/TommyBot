//! Prefix mod commands: permit check -> action -> case (+ undo data). ban/kick/timeout/warn.
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tommybot_automod::lockdown::kind_from;
use tommybot_core::{cases, jobs, permits, sections::MiscCfg, warn};
use tommybot_logging::{
    embed::{self, duration, ARROW, ORANGE, RED},
    Log, LogKind,
};
use twilight_http::{request::AuditLogReason, Client};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, MessageFlags,
    },
    gateway::payload::incoming::MessageCreate,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

use crate::{
    antinuke,
    app::{now_secs, App},
    appeals, backup,
    events::send_log,
    hierarchy::top_position,
    lockdown,
    modcmds::{self, ModCmd},
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
        return Ok(deny_text(perm, target.get(), &d));
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, perm).await? {
        return Ok(msg);
    }
    let t = target.get();
    let cfg = app.cfg(gid.get()).await?;
    let soft = flags.soft && kind == ModKind::Ban;
    // One DM before the action: a banned user can't be DMed afterwards.
    let note = appeals::note(&cfg.appeals, gid)
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
    // Wick also writes the case into Discord's audit log, which caps reasons at 512 characters.
    let audit = embed::clip(
        &format!(
            "{} | Moderator: {actor_id}",
            reason.unwrap_or("No reason provided")
        ),
        500,
    );
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
            app.http
                .create_ban(gid, target)
                .delete_message_seconds(cfg.misc.purge_secs())
                .reason(&audit)
                .await?;
            if let Some(d) = duration_s {
                let job = json!({"user":t});
                jobs::enqueue(&app.pool, gid.get() as i64, "unban", job, now_secs() + d).await?;
            }
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
            inverse,
        },
    )
    .await?;
    let name = if soft { "softban" } else { perm };
    // Wick's mod log: no title, Case / Type / Moderator columns, then Target and Reason.
    let mut log = Log::new("", case_color(name))
        .inline("Case:", &format!("`{case_no}`"))
        .inline("Type:", &format!("`{}`", case_title(name)))
        .inline("Moderator:", &embed::user(actor_id.get()))
        .field("Target:", &format!("{ARROW} {}", embed::user(t)));
    if let Some(d) = duration_s {
        log = log.field("Duration:", &format!("`{}`", duration(d)));
    }
    let log = log.field("Reason:", reason.unwrap_or("No reason provided"));
    if let Err(e) = send_log(app, gid.get(), LogKind::ModCommand, log).await {
        tracing::warn!(?e, "mod log failed");
    }
    let mut text = reply::case_text(
        cfg.misc.compact_replies || flags.compact,
        (case_no.into(), if soft { "softban" } else { perm }, t),
        reason,
        actor_id.get(),
    );
    if kind == ModKind::Warn {
        if let Some(extra) = apply_warn_actions(app, &cfg.misc, gid, target, points).await? {
            text += format!("\n{extra}").as_str();
        }
    }
    Ok(text)
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
                app.http.delete_message(msg.channel_id, msg.id).await.ok();
            }
        }
    }
    let kind = match p.command.as_str() {
        "ban" | "softban" => ModKind::Ban,
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
            // `lock role`, `lock server`, `lock channel`...: one-off type instead of the dashboard default.
            // Channel mentions lock (or unlock) those channels instead of this one.
            let channels = match p.channels.as_slice() {
                [] => vec![msg.channel_id.get()],
                c => c.to_vec(),
            };
            let name = p.kind.as_ref().or(p.names.first()).map(String::as_str);
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
            let mut out = vec![];
            for kind in kinds
                .into_iter()
                .map(Some)
                .chain(name.is_none().then_some(None))
            {
                out.push(lockdown::command(app, gid, &actor, unlock, kind, p.duration_s).await?);
            }
            return say(app, msg, &out.join("\n")).await;
        }
        "unban" | "hackban" | "untimeout" | "unwarn" | "quarantine" | "unquarantine" | "purge"
        | "slowmode" | "cases" | "case" | "verify" | "sanitize" | "note" | "notes" | "editc"
        | "editreason" | "closecase" | "caseoff" | "opencase" | "caseon" | "clearwarns"
        | "warns" | "modcases" => return modcmds::run_prefix(app, msg, p).await,
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
        },
    );
    if cfg.misc.confirm && job.needs_confirm() {
        return ask_confirm(app, msg, job).await;
    }
    let texts = job.run(app, owner, cfg.misc.warn_base_points).await?;
    let compact = cfg.misc.compact_replies || p.flags.compact;
    let text = reply::summary(
        kind.name(),
        compact,
        &texts,
        reason.as_deref(),
        msg.author.id.get(),
    );
    say(app, msg, &text).await
}

/// What a [`Pending`] does once confirmed.
#[derive(Serialize, Deserialize)]
pub enum Action {
    Mod {
        kind: ModKind,
        targets: Vec<Id<UserMarker>>,
        reason: Option<String>,
        duration_s: Option<i64>,
        flags: Flags,
    },
    Ext {
        cmd: ModCmd,
        channel: Id<ChannelMarker>,
        reason: Option<String>,
    },
}

/// A command waiting for its Confirm click, stored in `pending_confirms` so it survives a restart.
pub struct Pending {
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    action: Action,
}

const CONFIRM_TTL: Duration = Duration::from_secs(60);

impl Pending {
    pub fn new(gid: Id<GuildMarker>, actor: Id<UserMarker>, action: Action) -> Self {
        Self { gid, actor, action }
    }

    /// Warns and non-destructive commands run straight away.
    pub fn needs_confirm(&self) -> bool {
        match &self.action {
            Action::Mod { kind, targets, .. } => *kind != ModKind::Warn && !targets.is_empty(),
            Action::Ext { cmd, .. } => cmd.destructive(),
        }
    }

    fn question(&self) -> String {
        match &self.action {
            Action::Mod {
                kind,
                targets,
                flags,
                ..
            } => {
                let what = if flags.soft && *kind == ModKind::Ban {
                    "softban"
                } else {
                    kind.name()
                };
                let who: Vec<String> = targets.iter().map(|t| format!("<@{t}>")).collect();
                format!("Really {what} {}?", who.join(", "))
            }
            Action::Ext { cmd, .. } => format!("Really {}?", cmd.describe()),
        }
    }

    /// One reply per target.
    async fn run(&self, app: &App, owner: Id<UserMarker>, points: i32) -> Result<Vec<String>> {
        let (kind, targets, reason, duration_s, flags) = match &self.action {
            Action::Mod {
                kind,
                targets,
                reason,
                duration_s,
                flags,
            } => (*kind, targets, reason, *duration_s, *flags),
            Action::Ext {
                cmd,
                channel,
                reason,
            } => {
                let actor = app.level(self.gid, owner, self.actor).await?;
                let text = modcmds::run(
                    app,
                    self.gid,
                    owner,
                    (self.actor, &actor),
                    *channel,
                    cmd.clone(),
                    reason.as_deref(),
                )
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(?e, "mod command failed");
                    format!("Command failed: {}", why(&e))
                });
                return Ok(vec![text]);
            }
        };
        let mut out = vec![];
        for &t in targets {
            // Fresh level each time: a rate-limit quarantine mid-list must stop the rest.
            let actor = app.level(self.gid, owner, self.actor).await?;
            let text = match apply_dm(
                app,
                self.gid,
                owner,
                (self.actor, &actor),
                kind,
                t,
                reason.as_deref(),
                duration_s,
                points,
                flags,
            )
            .await
            {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(?e, "mod command failed");
                    format!("Failed to {} <@{t}>: {}", kind.name(), why(&e))
                }
            };
            out.push(text);
        }
        Ok(out)
    }
}

/// Discord's reason for a failed mod action, for the reply.
pub fn why(e: &anyhow::Error) -> String {
    use twilight_http::{api_error::ApiError, error::ErrorType};
    match e.downcast_ref::<twilight_http::Error>().map(|e| e.kind()) {
        Some(ErrorType::Response {
            error: ApiError::General(g),
            ..
        }) => match g.code {
            50013 => {
                "missing permissions (their top role is above mine, or they have Administrator)"
                    .into()
            }
            _ => g.message.clone(),
        },
        _ => "see logs".into(),
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

/// Stores `job` for its buttons and returns the question with the Confirm / Cancel row.
/// `nonce` is the command message or interaction id.
pub async fn stash_confirm(app: &App, nonce: u64, job: Pending) -> Result<(String, Component)> {
    let row = Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            confirm_button(
                "Confirm",
                format!("{CONFIRM}{nonce}:{}:y", job.actor),
                ButtonStyle::Danger,
            ),
            confirm_button(
                "Cancel",
                format!("{CONFIRM}{nonce}:{}:n", job.actor),
                ButtonStyle::Secondary,
            ),
        ],
    });
    // Expired rows are dropped on the way in, so the table stays small without a sweeper.
    sqlx::query(
        "WITH gone AS (DELETE FROM pending_confirms WHERE created_at < now() - make_interval(secs => $4))
         INSERT INTO pending_confirms (nonce, guild_id, action) VALUES ($1, $2, $3)",
    )
    .bind(nonce as i64)
    .bind(job.gid.get() as i64)
    .bind(serde_json::to_value(&job.action)?)
    .bind(CONFIRM_TTL.as_secs_f64())
    .execute(&app.pool)
    .await?;
    Ok((job.question(), row))
}

pub async fn ask_confirm(app: &App, msg: &MessageCreate, job: Pending) -> Result<()> {
    let (text, row) = stash_confirm(app, msg.id.get(), job).await?;
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
    let Some((nonce, actor, yes)) = c.custom_id.strip_prefix(CONFIRM).and_then(|r| {
        let mut it = r.splitn(3, ':');
        let (n, actor, a) = (it.next()?, it.next()?, it.next()?);
        Some((n.parse::<i64>().ok()?, actor.parse::<u64>().ok()?, a == "y"))
    }) else {
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
    const EXPIRED: &str = "This confirmation expired, type the command again.";
    let taken = if i.author_id().map(Id::get) != Some(actor) {
        Err("Only the staff member who typed the command can answer.")
    } else {
        let row: Option<(i64, serde_json::Value, bool)> = sqlx::query_as(
            "DELETE FROM pending_confirms WHERE nonce = $1
             RETURNING guild_id, action, created_at < now() - make_interval(secs => $2)",
        )
        .bind(nonce)
        .bind(CONFIRM_TTL.as_secs_f64())
        .fetch_optional(&app.pool)
        .await?;
        match row {
            Some((gid, action, false)) => Ok(Pending::new(
                Id::new(gid as u64),
                Id::new(actor),
                serde_json::from_value(action)?,
            )),
            _ => Err(EXPIRED),
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
    use super::{above_bot, timeout_chunk, Action, Flags, Id, ModCmd, ModKind, MAX_TIMEOUT_S};

    #[test]
    fn pending_action_survives_the_database_round_trip() {
        let flags = Flags {
            dm: true,
            soft: true,
            compact: false,
        };
        let a = Action::Mod {
            kind: ModKind::Ban,
            targets: vec![Id::new(42)],
            reason: Some("spam".into()),
            duration_s: Some(60),
            flags,
        };
        let Action::Mod { kind, targets, .. } =
            serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap()
        else {
            panic!("variant changed")
        };
        assert_eq!((kind, targets), (ModKind::Ban, vec![Id::new(42)]));
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
