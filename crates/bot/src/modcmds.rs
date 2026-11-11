//! Moderation commands beyond ban/kick/timeout/warn: unban, untimeout, unwarn, hackban,
//! quarantine, purge, slowmode, verify, sanitize and case viewing/editing. One implementation, prefix and slash parsers.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_antinuke::Inverse;
use tommybot_core::{
    cases,
    notes::{self, NoteAction, NoteLog},
    permits,
};
use tommybot_logging::{
    embed::{self, ORANGE},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    channel::message::{AllowedMentions, Component, Embed},
    gateway::payload::incoming::MessageCreate,
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    antinuke,
    app::App,
    commands::{self, gated, target_above_bot},
    confirm::{self, Action, Pending},
    events::send_log,
    prefix::{Flags, Parsed},
    purge,
    reply::{self, Applied, DmOutcome},
    settings, targets,
};

/// The clicked message of a purge menu: From Here purges it and the newer ones below it, Until Here
/// the older ones above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Anchor {
    From(u64),
    Until(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModCmd {
    Unban {
        user: u64,
    },
    Hackban {
        user: u64,
    },
    Untimeout {
        user: u64,
    },
    Unwarn {
        case: i32,
    },
    Quarantine {
        user: u64,
    },
    Unquarantine {
        user: u64,
    },
    Purge {
        count: usize,
        user: Option<u64>,
        /// Start at this message (message context menus) instead of the newest.
        from: Option<Anchor>,
        #[serde(default)]
        filters: Vec<crate::purge::Filter>,
    },
    Slowmode {
        secs: u16,
        /// Empty means the current channel.
        #[serde(default)]
        channels: Vec<u64>,
    },
    CasesView {
        case: Option<i32>,
        user: Option<u64>,
        /// `modcases`: cases this moderator made.
        by: Option<u64>,
        /// `?t TYPE`: one case kind.
        kind: Option<String>,
    },
    CasesEdit {
        case: i32,
        reason: String,
    },
    /// `closecase` / `opencase`: switch a case off or back on.
    CaseSet {
        case: i32,
        active: bool,
    },
    ClearWarns {
        user: u64,
    },
    Verify {
        user: u64,
    },
    Sanitize {
        user: u64,
    },
    NoteAdd {
        user: u64,
        text: String,
    },
    /// Every filter is optional; none lists the latest notes.
    NoteList {
        user: Option<u64>,
        id: Option<String>,
        by: Option<u64>,
    },
    NoteRemove {
        id: String,
        /// Only if the note is about this member.
        user: Option<u64>,
    },
    NoteModify {
        id: String,
        text: String,
    },
    /// Without filters: every note of the server.
    NoteSweep {
        user: Option<u64>,
        by: Option<u64>,
    },
}

/// Numbers below this are case numbers / counts, not snowflakes.
pub(crate) const SMALL: u64 = 1_000_000;
pub(crate) const MAX_SLOWMODE: u64 = 21_600;
const NOTE_MIN: usize = 5;
const NOTE_MAX: usize = 500;

impl ModCmd {
    /// `cases MEMBER` with no other filter pages through all of the member's cases.
    pub fn paged_user(&self) -> Option<u64> {
        match *self {
            ModCmd::CasesView {
                case: None,
                user: Some(u),
                by: None,
                kind: None,
            } => Some(u),
            _ => None,
        }
    }

    /// Permit name and rate-limit action. Unban/hackban share `ban`, untimeout shares `timeout`.
    fn perm(&self) -> &'static str {
        match self {
            Self::Unban { .. } | Self::Hackban { .. } => "ban",
            Self::Untimeout { .. } => "timeout",
            Self::Unwarn { .. } | Self::ClearWarns { .. } => "warn",
            Self::Quarantine { .. } | Self::Unquarantine { .. } => "quarantine",
            Self::Purge { .. } => "purge",
            Self::NoteList { .. } => "notes_view",
            Self::NoteAdd { .. }
            | Self::NoteModify { .. }
            | Self::NoteRemove { .. }
            | Self::NoteSweep { .. } => "notes_manage",
            Self::Slowmode { .. } => "slowmode",
            Self::CasesView { .. } | Self::CasesEdit { .. } | Self::CaseSet { .. } => "cases",
            Self::Verify { .. } => "verify",
            Self::Sanitize { .. } => "sanitize",
        }
    }

    /// Commands that lose data or are hard to undo ask for a Confirm click first (`MiscCfg.confirm`).
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Self::Purge { .. } | Self::Hackban { .. } | Self::NoteSweep { .. }
        )
    }

    /// Whether to wait for a Confirm click: a sweep of every note in the server always asks,
    /// other destructive commands only while `misc.confirm` is on.
    pub fn asks_confirm(&self, confirm_on: bool) -> bool {
        matches!(
            self,
            Self::NoteSweep {
                user: None,
                by: None
            }
        ) || (confirm_on && self.destructive())
    }

    /// Completes "Really ...?" for the confirm prompt.
    pub fn describe(&self) -> String {
        match self {
            Self::Purge {
                count,
                user: Some(u),
                ..
            } => format!("purge {count} messages of <@{u}>"),
            Self::Purge { count, .. } => format!("purge {count} messages"),
            Self::Hackban { user } => format!("ban <@{user}>"),
            Self::NoteSweep { user: Some(u), .. } => format!("remove all notes of <@{u}>"),
            Self::NoteSweep { by: Some(m), .. } => format!("remove all notes written by <@{m}>"),
            Self::NoteSweep { .. } => "remove all notes".into(),
            _ => self.perm().into(),
        }
    }

    /// The result Details flags this command implies: a hackban shows Hack Ban ✅.
    pub(crate) fn flags(&self) -> Flags {
        Flags {
            hack: matches!(self, Self::Hackban { .. }),
            ..Flags::default()
        }
    }

    /// Quarantine, unquarantine and unban ask Yes/No when a member was typed as an id or a name.
    pub fn asks_when_unsure(&self) -> bool {
        matches!(
            self,
            Self::Quarantine { .. } | Self::Unquarantine { .. } | Self::Unban { .. }
        )
    }

    /// These take several members; the prefix runs one copy per member.
    pub(crate) fn with_user(&self, u: u64) -> Option<Self> {
        Some(match self {
            Self::Unban { .. } => Self::Unban { user: u },
            Self::Untimeout { .. } => Self::Untimeout { user: u },
            Self::Quarantine { .. } => Self::Quarantine { user: u },
            Self::Unquarantine { .. } => Self::Unquarantine { user: u },
            Self::Verify { .. } => Self::Verify { user: u },
            Self::Sanitize { .. } => Self::Sanitize { user: u },
            _ => return None,
        })
    }

    /// Key of the result embed (`reply::result_embeds`); None keeps plain text (views, notes, case edits).
    pub(crate) fn result_kind(&self) -> Option<&'static str> {
        Some(match self {
            Self::Hackban { .. } => "ban",
            Self::Unban { .. } => "unban",
            Self::Untimeout { .. } => "untimeout",
            Self::Quarantine { .. } => "quarantine",
            Self::Unquarantine { .. } => "unquarantine",
            Self::Verify { .. } => "verify",
            Self::Sanitize { .. } => "sanitize",
            Self::Purge { .. } => "purge",
            Self::Slowmode { .. } => "slowmode",
            Self::NoteAdd { .. } | Self::NoteList { id: Some(_), .. } => "note",
            _ => return None,
        })
    }

    /// The member the permit/hierarchy check applies to.
    pub(crate) fn target(&self) -> Option<u64> {
        match *self {
            Self::Unban { user }
            | Self::Hackban { user }
            | Self::Untimeout { user }
            | Self::ClearWarns { user }
            | Self::Quarantine { user }
            | Self::Unquarantine { user }
            | Self::Sanitize { user }
            | Self::NoteAdd { user, .. }
            | Self::NoteList {
                user: Some(user), ..
            }
            | Self::NoteRemove {
                user: Some(user), ..
            }
            | Self::NoteSweep {
                user: Some(user), ..
            } => Some(user),
            _ => None,
        }
    }
}

/// Text and embeds for a finished command: result embeds, or count lines when compact.
pub(crate) fn reply_parts(
    kind: Option<&str>,
    compact: bool,
    done: &[Applied],
    shown: (Flags, Option<i64>),
    reason: Option<&str>,
    actor: u64,
) -> (String, Vec<Embed>) {
    let joined = || {
        done.iter()
            .map(|a| a.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    match kind {
        // A note card shows the note's text, so it goes to the moderator only (see `run_prefix`).
        Some("note") => match done.first() {
            Some(a) if !compact && a.text.starts_with("Note: ") => {
                (String::new(), vec![crate::modview::note_embed(&a.text)])
            }
            _ => (joined(), vec![]),
        },
        Some(k) if !compact => (
            String::new(),
            reply::result_embeds(k, done, shown, reason, actor),
        ),
        Some(k) => (reply::summary(k, true, done, reason, actor), vec![]),
        None => (joined(), vec![]),
    }
}

/// `resolved` is a member found by name when no id/mention was typed.
pub fn from_prefix(p: &Parsed, resolved: Option<u64>) -> Result<ModCmd, String> {
    let small = p.targets.iter().copied().find(|t| *t < SMALL);
    let user = p.targets.iter().copied().find(|t| *t >= SMALL).or(resolved);
    let need_user = || user.ok_or_else(|| "Give a user mention, id or name".to_string());
    let case = || {
        small
            .map(|n| n as i32)
            .ok_or_else(|| "Give a case number".to_string())
    };
    Ok(match p.command.as_str() {
        "unban" => ModCmd::Unban { user: need_user()? },
        "hackban" => ModCmd::Hackban { user: need_user()? },
        "untimeout" | "unmute" => ModCmd::Untimeout { user: need_user()? },
        "unwarn" => ModCmd::Unwarn { case: case()? },
        "clearwarns" => ModCmd::ClearWarns { user: need_user()? },
        "closecase" | "caseoff" | "opencase" | "caseon" => ModCmd::CaseSet {
            case: case()?,
            active: matches!(p.command.as_str(), "opencase" | "caseon"),
        },
        // The words after the case number are the reason, no `?r` needed.
        "editc" | "editreason" => ModCmd::CasesEdit {
            case: case()?,
            reason: p
                .reason
                .clone()
                .or_else(|| Some(p.names.join(" ")).filter(|r| !r.is_empty()))
                .ok_or("Give the new reason")?,
        },
        "quarantine" => ModCmd::Quarantine { user: need_user()? },
        "unquarantine" => ModCmd::Unquarantine { user: need_user()? },
        "verify" => ModCmd::Verify { user: need_user()? },
        "sanitize" | "dehoist" => ModCmd::Sanitize { user: need_user()? },
        "purge" | "clear" => ModCmd::Purge {
            count: small
                .filter(|n| (1..=purge::MAX as u64).contains(n))
                .ok_or("Give a message count from 1 to 1000")? as usize,
            user,
            from: None,
            filters: p
                .kind
                .as_deref()
                .map(|k| purge::Filter::parse(k, p.input.as_deref()))
                .transpose()?
                .into_iter()
                .chain(p.roles.iter().map(|r| purge::Filter::Role(*r)))
                .collect(),
        },
        "slowmode" => {
            let channels: Vec<u64> = p
                .channels
                .iter()
                .copied()
                .chain(p.targets.iter().copied().filter(|t| *t >= SMALL))
                .collect();
            let off = p.names.iter().any(|n| n == "off") || !channels.is_empty();
            let secs = p
                .duration_s
                .map(|d| d as u64)
                .or(small)
                .or(off.then_some(0));
            ModCmd::Slowmode {
                secs: secs
                    .filter(|s| *s <= MAX_SLOWMODE)
                    .ok_or("Give a delay up to 6h (e.g. 10s) or `off`")?
                    as u16,
                channels,
            }
        }
        "note" | "notes" => {
            let (sub, text) = (p.names.first().map(String::as_str), p.reason.clone());
            match sub {
                Some("add") => ModCmd::NoteAdd {
                    user: need_user()?,
                    text: text.ok_or("Give the note with ?r")?,
                },
                Some("remove") => ModCmd::NoteRemove {
                    id: p
                        .names
                        .get(1)
                        .cloned()
                        .or(small.map(|n| n.to_string()))
                        .ok_or("Give a note id")?,
                    user,
                },
                Some("sweep") => ModCmd::NoteSweep {
                    user: Some(need_user()?),
                    by: None,
                },
                _ => ModCmd::NoteList {
                    user,
                    id: None,
                    by: None,
                },
            }
        }
        "cases" | "case" if p.names.first().is_some_and(|n| n == "modify") => ModCmd::CasesEdit {
            case: case()?,
            reason: p.reason.clone().ok_or("Give a new reason with ?r")?,
        },
        // No case and no user lists the guild's latest cases.
        "cases" | "case" => ModCmd::CasesView {
            case: small.map(|n| n as i32),
            user,
            by: None,
            kind: crate::modview::case_kind(p.kind.as_deref())?,
        },
        "warns" => ModCmd::CasesView {
            case: None,
            user: Some(need_user()?),
            by: None,
            kind: Some("warn".into()),
        },
        "modcases" => ModCmd::CasesView {
            case: None,
            user: None,
            by: Some(need_user()?),
            kind: crate::modview::case_kind(p.kind.as_deref())?,
        },
        _ => return Err("Unknown command".into()),
    })
}

/// Prefix entry for the commands above; the caller only routes known names here.
pub async fn run_prefix(app: &App, msg: &MessageCreate, p: Parsed) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, msg.author.id).await?;
    if level == permits::Level::None && app.cfg(gid.get()).await?.misc.ignore_no_permit {
        return Ok(());
    }
    // `unban 123 spam links`: the words after an id are the reason, not names.
    let mut p = p;
    if p.command == "unban" && !p.targets.is_empty() && p.reason.is_none() {
        p.reason = Some(p.names.join(" ")).filter(|r| !r.is_empty());
        p.names.clear();
    }
    let multi = matches!(
        p.command.as_str(),
        "unban" | "untimeout" | "quarantine" | "unquarantine" | "verify" | "sanitize"
    );
    let name_word = |n: &&String| {
        !matches!(
            n.as_str(),
            "modify" | "view" | "off" | "add" | "remove" | "sweep" | "list"
        )
    };
    // `editc 3 new reason`: the words are the reason, not a member.
    let free_text = matches!(p.command.as_str(), "editc" | "editreason");
    let mut users: Vec<u64> = p.targets.iter().copied().filter(|t| *t >= SMALL).collect();
    let mut failed = vec![];
    // `note remove q413p9`: the id is not a member name.
    let note_id = matches!(p.command.as_str(), "note" | "notes")
        && p.names.first().is_some_and(|n| n == "remove");
    let lookups: Vec<&String> = match (multi, free_text || note_id, users.is_empty()) {
        (true, ..) => p.names.iter().filter(name_word).collect(),
        (false, false, true) if p.targets.iter().all(|t| *t < SMALL) => {
            p.names.iter().find(name_word).into_iter().collect()
        }
        _ => vec![],
    };
    for n in lookups {
        let found = if p.command == "unban" {
            targets::resolve_banned(app, gid, n).await?
        } else {
            targets::resolve(app, gid, n).await?
        };
        match found {
            Ok(id) => users.push(id.get()),
            Err(e) if multi => failed.push(e),
            Err(e) => return commands::say(app, msg, &e).await,
        }
    }
    let cmd = match from_prefix(&p, users.first().copied()) {
        Ok(c) => c,
        Err(e) => {
            let text = if failed.is_empty() {
                e
            } else {
                failed.join("\n")
            };
            return commands::say(app, msg, &text).await;
        }
    };
    let kind = cmd.result_kind();
    let shown = (
        Flags {
            hack: cmd.flags().hack,
            ..p.flags
        },
        None,
    );
    let private = matches!(cmd, ModCmd::NoteAdd { .. } | ModCmd::NoteList { .. });
    let paged = cmd.paged_user();
    let action = Action::Ext {
        cmd,
        channel: msg.channel_id,
        reason: p.reason.clone(),
        proof: None,
        users,
        unsure: p.unsure(),
        dm: p.flags.dm,
    };
    let job = Pending::new(gid, msg.author.id, action);
    if job.needs_confirm(app.cfg(gid.get()).await?.misc.confirm) {
        if !failed.is_empty() {
            commands::say(app, msg, &failed.join("\n")).await?;
        }
        return confirm::ask_confirm(app, msg, job).await;
    }
    let mut texts = job.run(app, owner, 0).await?;
    texts.extend(failed.into_iter().map(Applied::from));
    let compact = app.cfg(gid.get()).await?.misc.compact_replies || p.flags.compact;
    let (text, embeds) = reply_parts(
        kind,
        compact,
        &texts,
        shown,
        p.reason.as_deref(),
        msg.author.id.get(),
    );
    if private {
        // Notes are for moderators only: the channel never sees their text.
        let sent = async {
            let ch = app
                .http
                .create_private_channel(msg.author.id)
                .await?
                .model()
                .await?;
            app.http
                .create_message(ch.id)
                .content(&text)
                .embeds(&embeds)
                .allowed_mentions(Some(&AllowedMentions::default()))
                .await?;
            anyhow::Ok(())
        };
        let notice = match sent.await {
            Ok(()) => "Sent to your DMs.",
            Err(_) => "Could not DM you: open your DMs to read notes.",
        };
        return commands::say(app, msg, notice).await;
    }
    if let Some((e, row)) = cases_pager(app, gid, msg.author.id.get(), paged, &text).await? {
        return settings::send(app, msg, "", Some(e), vec![row]).await;
    }
    commands::say_embeds(app, msg, &text, &embeds).await
}

/// The first page and its buttons when a `cases MEMBER` reply spans several pages.
pub(crate) async fn cases_pager(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: u64,
    user: Option<u64>,
    text: &str,
) -> Result<Option<(Embed, Component)>> {
    // A first page that starts with a case line was allowed; denials are plain text.
    let (Some(u), true) = (user, text.starts_with("`#")) else {
        return Ok(None);
    };
    let (e, pages) = crate::modview::cases_page(app, gid.get() as i64, u, 0).await?;
    Ok((pages > 1).then(|| {
        (
            e,
            settings::page_row(invoker, &format!("cases.{u}"), 0, pages),
        )
    }))
}

/// A reply that is no case: a refusal, a listing or a note.
fn plain(text: impl Into<String>) -> Result<Applied> {
    Ok(Applied::from(text.into()))
}

/// Permit check -> hierarchy -> rate limit -> action -> case. `dm` is the `?dm` flag: quarantine
/// and the removals DM the member when it or the server's DM toggle says so.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: &App,
    gid: Id<GuildMarker>,
    owner: Id<UserMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    channel: Id<ChannelMarker>,
    cmd: ModCmd,
    (reason, proof): (Option<&str>, Option<&str>),
    dm: bool,
) -> Result<Applied> {
    let perm = cmd.perm();
    let target = cmd.target();
    if let Some(t) = cmd.target() {
        let target = Id::new(t);
        let mut level = app.level(gid, owner, target).await?;
        // Quarantine took the member's roles, so judge a release by the roles it gives back.
        if matches!(cmd, ModCmd::Unquarantine { .. } | ModCmd::Verify { .. }) {
            let saved: Option<Vec<i64>> = sqlx::query_scalar(
                "SELECT saved_roles FROM quarantines WHERE guild_id=$1 AND user_id=$2",
            )
            .bind(gid.get() as i64)
            .bind(t as i64)
            .fetch_optional(&app.pool)
            .await?;
            if let Some(roles) = saved {
                let held = permits::resolve(
                    &app.pool,
                    gid.get() as i64,
                    owner.get() as i64,
                    t as i64,
                    &roles,
                )
                .await?;
                // Staff caught by anti-nuke or a rate limit go free only through an owner or TA.
                if held != permits::Level::None && !actor.immune() {
                    return plain(format!("Cannot release <@{t}>: their roles hold a permit, so only an owner or trusted admin can"));
                }
                if held.immune() {
                    level = held;
                }
            }
        }
        let above = target_above_bot(app, gid, target).await?;
        let actor_n = actor_id.get() as i64;
        if let Err(d) = permits::check(actor, perm, actor_n, &level, t as i64, above) {
            return plain(commands::deny_text(perm, t, &d));
        }
    } else if !actor.has(perm) {
        return plain(format!("Cannot use `{perm}`: no permit"));
    }
    // A case has its member as the target: nobody changes their own, or an owner's or TA's.
    if let ModCmd::Unwarn { case } | ModCmd::CasesEdit { case, .. } | ModCmd::CaseSet { case, .. } =
        &cmd
    {
        let member: Option<i64> =
            sqlx::query_scalar("SELECT user_id FROM cases WHERE guild_id=$1 AND case_no=$2")
                .bind(gid.get() as i64)
                .bind(case)
                .fetch_optional(&app.pool)
                .await?;
        if let Some(m) = member.filter(|m| *m > 0) {
            if m as u64 == actor_id.get() {
                return plain("You cannot change your own case");
            }
            if !actor.immune() && app.level(gid, owner, Id::new(m as u64)).await?.immune() {
                return plain(format!("Cannot change case #{case}: <@{m}> is immune"));
            }
        }
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, perm).await? {
        return plain(msg);
    }
    let g = gid.get() as i64;
    let reasonless = reason.is_none();
    let audit = commands::audit_reason(reason, actor_id);
    let cfg = app.cfg(gid.get()).await?;
    // The DM for quarantine and the removals, sent by `reply::dm_and_act` around the action.
    let notice = |kind, user: u64| {
        let n = reply::Notice {
            kind,
            reason,
            duration_s: None,
            moderator: (!cfg.misc.hide_staff_in_dms).then_some(actor_id),
            appeal: None,
        };
        reply::wants_dm(dm, &cfg.misc).then(|| reply::tell(app, gid, Id::new(user), n))
    };
    let mut told = DmOutcome::Off;
    // (case kind, case user, inverse, reason prefix, reply)
    let (kind, uid, inverse, note, reply): (&str, u64, Option<Value>, Option<String>, String) =
        match cmd {
            ModCmd::CasesView {
                case,
                user,
                by,
                kind,
            } => {
                return crate::modview::view_cases(app, g, case, user, by, kind, 0)
                    .await
                    .map(Applied::from)
            }
            ModCmd::NoteAdd { user, text } => {
                if !(NOTE_MIN..=NOTE_MAX).contains(&text.chars().count()) {
                    return plain(format!(
                        "A note must be {NOTE_MIN} to {NOTE_MAX} characters long"
                    ));
                }
                let (id,): (String,) = sqlx::query_as(
                    "INSERT INTO notes (guild_id, user_id, author_id, text) VALUES ($1,$2,$3,$4) RETURNING short_id",
                )
                .bind(g)
                .bind(user as i64)
                .bind(actor_id.get() as i64)
                .bind(&text)
                .fetch_one(&app.pool)
                .await?;
                let log = NoteLog {
                    action: NoteAction::Add,
                    id: Some(id.clone()),
                    user: Some(user),
                    by: actor_id.get(),
                    text: Some(text.clone()),
                    ..NoteLog::default()
                };
                crate::modview::send_note_log(app, gid.get(), &log)
                    .await
                    .ok();
                return plain(crate::modview::note_text(&id, user, actor_id.get(), &text));
            }
            ModCmd::NoteModify { id, text } => {
                if !(NOTE_MIN..=NOTE_MAX).contains(&text.chars().count()) {
                    return plain(format!(
                        "A note must be {NOTE_MIN} to {NOTE_MAX} characters long"
                    ));
                }
                let actor = actor_id.get();
                let Some((user, old)) = notes::edit(&app.pool, g, &id, &text, actor as i64).await?
                else {
                    return plain(format!("No note `{id}`"));
                };
                if old == text {
                    return plain(format!("Note `{id}` already says that"));
                }
                let log = NoteLog {
                    action: NoteAction::Modify,
                    id: Some(id.clone()),
                    user: Some(user as u64),
                    by: actor,
                    text: Some(text),
                    old: Some(old),
                    ..NoteLog::default()
                };
                crate::modview::send_note_log(app, gid.get(), &log)
                    .await
                    .ok();
                return plain(format!("Note `{id}` updated"));
            }
            ModCmd::NoteList { user, id, by } => {
                return crate::modview::view_notes(app, g, user, id, by)
                    .await
                    .map(Applied::from)
            }
            ModCmd::NoteRemove { id, user } => {
                let gone: Option<(i64, String)> = sqlx::query_as(
                    "DELETE FROM notes WHERE guild_id=$1 AND short_id=$2 AND ($3::bigint IS NULL OR user_id=$3) RETURNING user_id, text",
                )
                .bind(g)
                .bind(&id)
                .bind(user.map(|u| u as i64))
                .fetch_optional(&app.pool)
                .await?;
                let Some((user, text)) = gone else {
                    return plain(format!("No note `{id}`"));
                };
                let log = NoteLog {
                    action: NoteAction::Remove,
                    id: Some(id.clone()),
                    user: Some(user as u64),
                    by: actor_id.get(),
                    text: Some(text),
                    ..NoteLog::default()
                };
                crate::modview::send_note_log(app, gid.get(), &log)
                    .await
                    .ok();
                return plain(format!("Removed note `{id}`"));
            }
            ModCmd::NoteSweep { user, by } => {
                let n = sqlx::query(
                    "DELETE FROM notes WHERE guild_id=$1 AND ($2::bigint IS NULL OR user_id=$2) AND ($3::bigint IS NULL OR author_id=$3)",
                )
                .bind(g)
                .bind(user.map(|u| u as i64))
                .bind(by.map(|u| u as i64))
                .execute(&app.pool)
                .await?
                .rows_affected();
                let log = NoteLog {
                    action: NoteAction::Sweep,
                    user,
                    author: by,
                    by: actor_id.get(),
                    count: Some(n),
                    ..NoteLog::default()
                };
                crate::modview::send_note_log(app, gid.get(), &log)
                    .await
                    .ok();
                return plain(format!("Removed {n} notes"));
            }
            ModCmd::CasesEdit { case, reason } => {
                let target: Option<i64> = sqlx::query_scalar(
                    "UPDATE cases SET reason=$3 WHERE guild_id=$1 AND case_no=$2 RETURNING user_id",
                )
                .bind(g)
                .bind(case)
                .bind(&reason)
                .fetch_optional(&app.pool)
                .await?;
                let done = target.is_some() as u64;
                stat(app, g, actor_id.get(), "case_edit", target, Some(case)).await;
                if done > 0 {
                    let log = Log::new("Case Updated", ORANGE)
                        .line("Case", &format!("`{case}`"))
                        .line("Moderator", &embed::user(actor_id.get()))
                        .line("New Reason", &reason);
                    send_log(app, gid.get(), LogKind::ModCommand, log)
                        .await
                        .ok();
                }
                return plain(match done {
                    0 => format!("No case #{case}"),
                    _ => format!("Case #{case} reason updated"),
                });
            }
            ModCmd::CaseSet { case, active } => {
                let target: Option<i64> = sqlx::query_scalar(
                    "UPDATE cases SET active=$3 WHERE guild_id=$1 AND case_no=$2 RETURNING user_id",
                )
                .bind(g)
                .bind(case)
                .bind(active)
                .fetch_optional(&app.pool)
                .await?;
                let done = target.is_some() as u64;
                stat(app, g, actor_id.get(), "case_edit", target, Some(case)).await;
                return plain(match (done, active) {
                    (0, _) => format!("No case #{case}"),
                    (_, true) => format!("Case #{case} is active again"),
                    _ => format!("Case #{case} closed"),
                });
            }
            ModCmd::ClearWarns { user } => {
                cases::reset_warns(&app.pool, g, user as i64).await?;
                stat(
                    app,
                    g,
                    actor_id.get(),
                    "warn_clear",
                    Some(user as i64),
                    None,
                )
                .await;
                return plain(format!("Cleared the warns of <@{user}>"));
            }
            ModCmd::Unban { user } => {
                let act = async {
                    Ok(app
                        .http
                        .delete_ban(gid, Id::new(user))
                        .reason(&audit)
                        .await?)
                };
                let (d, r) =
                    reply::dm_and_act(reply::dm_first("unban"), notice("unban", user), act).await;
                (told, _) = (d, r?);
                ("unban", user, None, None, format!("Unbanned <@{user}>"))
            }
            ModCmd::Hackban { user } => {
                let secs = cfg.misc.purge_secs();
                app.http
                    .create_ban(gid, Id::new(user))
                    .delete_message_seconds(secs)
                    .reason(&audit)
                    .await?;
                let inv = json!({"op":"unban","user":user});
                (
                    "ban",
                    user,
                    Some(inv),
                    None,
                    format!("Banned <@{user}> by id"),
                )
            }
            ModCmd::Untimeout { user } => {
                let act = async {
                    Ok(app
                        .http
                        .update_guild_member(gid, Id::new(user))
                        .communication_disabled_until(None)
                        .reason(&audit)
                        .await?)
                };
                let (d, r) =
                    reply::dm_and_act(reply::dm_first("untimeout"), notice("untimeout", user), act)
                        .await;
                (told, _) = (d, r?);
                let text = format!("Removed timeout of <@{user}>");
                ("untimeout", user, None, None, text)
            }
            ModCmd::Unwarn { case } => {
                let row: Option<(i64,)> = sqlx::query_as(
                    "UPDATE cases SET active=false WHERE guild_id=$1 AND case_no=$2 AND kind='warn' AND active RETURNING user_id",
                )
                .bind(g)
                .bind(case)
                .fetch_optional(&app.pool)
                .await?;
                let Some((uid,)) = row else {
                    return plain(format!("No active warn case #{case}"));
                };
                let text = format!("Deactivated warn #{case} of <@{uid}>");
                (
                    "unwarn",
                    uid as u64,
                    None,
                    Some(format!("warn #{case}")),
                    text,
                )
            }
            ModCmd::Quarantine { user } => {
                let act = antinuke::quarantine(app, gid, Id::new(user));
                let (d, r) = reply::dm_and_act(
                    reply::dm_first("quarantine"),
                    notice("quarantine", user),
                    act,
                )
                .await;
                (told, _) = (d, r?);
                let inv = json!({"op":"unquarantine","user":user});
                let text = format!("Quarantined <@{user}>");
                ("quarantine", user, Some(inv), None, text)
            }
            ModCmd::Unquarantine { user } => {
                let held: Option<(i64,)> = sqlx::query_as(
                    "SELECT 1::bigint FROM quarantines WHERE guild_id=$1 AND user_id=$2",
                )
                .bind(g)
                .bind(user as i64)
                .fetch_optional(&app.pool)
                .await?;
                if held.is_none() {
                    return plain(format!("<@{user}> is not quarantined"));
                }
                let inv = Inverse::Unquarantine { user };
                let act = antinuke::run_inverse(app, gid, &inv);
                let (d, r) = reply::dm_and_act(
                    reply::dm_first("unquarantine"),
                    notice("unquarantine", user),
                    act,
                )
                .await;
                (told, _) = (d, r?);
                let text = format!("Released <@{user}> from quarantine");
                ("unquarantine", user, None, None, text)
            }
            ModCmd::Verify { user } => {
                // Verify also ends a quarantine.
                let held: Option<(i64,)> = sqlx::query_as(
                    "SELECT 1::bigint FROM quarantines WHERE guild_id=$1 AND user_id=$2",
                )
                .bind(g)
                .bind(user as i64)
                .fetch_optional(&app.pool)
                .await?;
                if held.is_some() {
                    antinuke::run_inverse(app, gid, &Inverse::Unquarantine { user }).await?;
                }
                if cfg.verification.roles.is_empty() && held.is_none() {
                    return plain("Set the verification roles on the dashboard first");
                }
                crate::verify::grant(app, gid, Id::new(user), &cfg.verification).await?;
                ("verify", user, None, None, format!("Verified <@{user}>"))
            }
            ModCmd::Sanitize { user } => {
                let m = app
                    .http
                    .guild_member(gid, Id::new(user))
                    .await?
                    .model()
                    .await?;
                let name = m.nick.or(m.user.global_name).unwrap_or(m.user.name);
                let clean = crate::modview::sanitized(&name);
                if clean == name {
                    return plain(format!("<@{user}> already has a clean name"));
                }
                app.http
                    .update_guild_member(gid, Id::new(user))
                    .nick(Some(&clean))
                    .reason(&audit)
                    .await?;
                let note = format!("`{name}` -> `{clean}`");
                let text = format!("Renamed <@{user}> to `{clean}`");
                ("sanitize", user, None, Some(note), text)
            }
            ModCmd::Purge {
                count,
                user,
                from,
                filters,
            } => {
                let n = purge::run(app, gid, channel, count, user, &filters, from).await?;
                if n == 0 {
                    return plain("No deletable messages found (over 14 days old are skipped)");
                }
                let who = user.unwrap_or(actor_id.get());
                let note = format!("{n} messages in <#{channel}>");
                (
                    "purge",
                    who,
                    None,
                    Some(note),
                    format!("Deleted {n} messages"),
                )
            }
            ModCmd::Slowmode { secs, channels } => {
                let channels = match channels.as_slice() {
                    [] => vec![channel.get()],
                    c => c.to_vec(),
                };
                for c in &channels {
                    let ch = app.http.channel(Id::new(*c)).await?.model().await?;
                    // A raw id can name another server's channel; the bot must not act there.
                    anyhow::ensure!(
                        ch.guild_id == Some(gid),
                        "<#{c}> is not a channel of this server"
                    );
                }
                for c in &channels {
                    app.http
                        .update_channel(Id::new(*c))
                        .rate_limit_per_user(secs)
                        .reason(&audit)
                        .await?;
                    // Staff's value stays: a running auto slowmode must not revert it.
                    if let Err(e) = crate::slowmode::staff_set(app, *c, secs).await {
                        tracing::warn!(?e, channel = c, "ending the auto slowmode failed");
                    }
                }
                let list: Vec<_> = channels.iter().map(|c| format!("<#{c}>")).collect();
                let note = format!("{secs}s in {}", list.join(" "));
                (
                    "slowmode",
                    actor_id.get(),
                    None,
                    Some(note),
                    format!("Slowmode set to {secs}s"),
                )
            }
        };
    let reason = match (note, reason) {
        (Some(n), Some(r)) => Some(format!("{n}: {r}")),
        (n, r) => n.or(r.map(String::from)),
    };
    let case_no = cases::create(
        &app.pool,
        cases::NewCase {
            guild_id: g,
            kind,
            user_id: uid as i64,
            mod_id: actor_id.get() as i64,
            reason: reason.as_deref(),
            points: 0,
            duration_s: None,
            proof,
            inverse,
        },
    )
    .await?;
    commands::mod_log(
        app,
        gid,
        (case_no, kind),
        (actor_id, Id::new(uid)),
        None,
        reason.as_deref(),
    )
    .await;
    let hint = if reasonless {
        crate::reply::add_reason_hint(case_no.into())
    } else {
        String::new()
    };
    Ok(Applied {
        target,
        case: Some(case_no),
        text: format!("Case #{case_no}: {reply}{hint}"),
        dm: told,
    })
}

#[cfg(test)]
#[path = "modcmds_tests.rs"]
mod tests;

/// A command that rewrites case history counts in moderator stats, like the dashboard's edits.
/// Nothing is recorded when the case did not exist; a failed write is only logged.
async fn stat(app: &App, g: i64, by: u64, kind: &str, target: Option<i64>, case_no: Option<i32>) {
    if target.is_none() {
        return;
    }
    let r = async {
        cases::record(
            &mut *app.pool.acquire().await?,
            cases::ModAction {
                guild_id: g,
                mod_id: by as i64,
                kind,
                source: "bot",
                target_id: target,
                case_no,
                audit_id: None,
            },
        )
        .await
    }
    .await;
    if let Err(e) = r {
        tracing::warn!(?e, kind, "recording a case change for mod stats failed");
    }
}
