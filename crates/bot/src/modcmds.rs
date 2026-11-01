//! Moderation commands beyond ban/kick/timeout/warn: unban, untimeout, unwarn, hackban,
//! quarantine, purge, slowmode, verify, sanitize and case viewing/editing. One implementation, prefix and slash parsers.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_antinuke::Inverse;
use tommybot_core::{cases, permits};
use tommybot_logging::{
    embed::{self, ORANGE},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    channel::message::Embed,
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
    prefix::Parsed,
    purge, reply, targets,
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
        id: Option<i64>,
        by: Option<u64>,
    },
    NoteRemove {
        id: i64,
        /// Only if the note is about this member.
        user: Option<u64>,
    },
    NoteModify {
        id: i64,
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

    /// What a DM about this command calls it.
    pub(crate) fn dm_name(&self) -> &'static str {
        match self {
            Self::Untimeout { .. } => "timeout removal",
            Self::Unquarantine { .. } => "quarantine removal",
            _ => self.perm(),
        }
    }

    /// Commands that lose data or are hard to undo ask for a Confirm click first (`MiscCfg.confirm`).
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Self::Purge { .. } | Self::Hackban { .. } | Self::NoteSweep { .. }
        )
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

    /// Wick takes several members for these; the prefix runs one copy per member.
    fn with_user(&self, u: u64) -> Option<Self> {
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
            | Self::Sanitize { user } => Some(user),
            _ => None,
        }
    }
}

/// Text and embeds for a finished command: result embeds, or Wick's count lines when compact.
pub(crate) fn reply_parts(
    kind: Option<&str>,
    compact: bool,
    texts: &[String],
    reason: Option<&str>,
    actor: u64,
) -> (String, Vec<Embed>) {
    match kind {
        Some(k) if !compact => (String::new(), reply::result_embeds(k, texts, reason, actor)),
        Some(k) => (reply::summary(k, true, texts, reason, actor), vec![]),
        None => (texts.join("\n"), vec![]),
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
                    id: case()? as i64,
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
    let lookups: Vec<&String> = match (multi, free_text, users.is_empty()) {
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
    if app.cfg(gid.get()).await?.misc.confirm && cmd.destructive() {
        let action = Action::Ext {
            cmd,
            channel: msg.channel_id,
            reason: p.reason,
            proof: None,
        };
        return confirm::ask_confirm(app, msg, Pending::new(gid, msg.author.id, action)).await;
    }
    let kind = cmd.result_kind();
    let cmds: Vec<ModCmd> = users.iter().filter_map(|u| cmd.with_user(*u)).collect();
    let cmds = if cmds.is_empty() { vec![cmd] } else { cmds };
    let mut texts = vec![];
    for cmd in cmds {
        // Fresh level each time: a rate-limit quarantine mid-list must stop the rest.
        let level = app.level(gid, owner, msg.author.id).await?;
        texts.push(
            run(
                app,
                gid,
                owner,
                (msg.author.id, &level),
                msg.channel_id,
                cmd,
                p.reason.as_deref(),
                None,
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(?e, "mod command failed");
                format!("Command failed: {}", crate::confirm::why(&e))
            }),
        );
    }
    texts.extend(failed);
    let compact = app.cfg(gid.get()).await?.misc.compact_replies || p.flags.compact;
    let (text, embeds) = reply_parts(
        kind,
        compact,
        &texts,
        p.reason.as_deref(),
        msg.author.id.get(),
    );
    commands::say_embeds(app, msg, &text, &embeds).await
}

/// Permit check -> hierarchy -> rate limit -> action -> case. Returns the reply text.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: &App,
    gid: Id<GuildMarker>,
    owner: Id<UserMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    channel: Id<ChannelMarker>,
    cmd: ModCmd,
    reason: Option<&str>,
    proof: Option<&str>,
) -> Result<String> {
    let perm = cmd.perm();
    if let Some(t) = cmd.target() {
        let target = Id::new(t);
        let level = app.level(gid, owner, target).await?;
        let above = target_above_bot(app, gid, target).await?;
        let actor_n = actor_id.get() as i64;
        if let Err(d) = permits::check(actor, perm, actor_n, &level, t as i64, above) {
            return Ok(commands::deny_text(perm, t, &d));
        }
    } else if !actor.has(perm) {
        return Ok(format!("Cannot use `{perm}`: no permit"));
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, perm).await? {
        return Ok(msg);
    }
    let g = gid.get() as i64;
    let reasonless = reason.is_none();
    let audit = commands::audit_reason(reason, actor_id);
    // (case kind, case user, inverse, reason prefix, reply)
    let (kind, uid, inverse, note, reply): (&str, u64, Option<Value>, Option<String>, String) =
        match cmd {
            ModCmd::CasesView {
                case,
                user,
                by,
                kind,
            } => return crate::modview::view_cases(app, g, case, user, by, kind).await,
            ModCmd::NoteAdd { user, text } => {
                if !(NOTE_MIN..=NOTE_MAX).contains(&text.chars().count()) {
                    return Ok(format!(
                        "A note must be {NOTE_MIN} to {NOTE_MAX} characters long"
                    ));
                }
                let (id,): (i64,) = sqlx::query_as(
                    "INSERT INTO notes (guild_id, user_id, author_id, text) VALUES ($1,$2,$3,$4) RETURNING id",
                )
                .bind(g)
                .bind(user as i64)
                .bind(actor_id.get() as i64)
                .bind(text)
                .fetch_one(&app.pool)
                .await?;
                return Ok(format!("Note #{id} added for <@{user}>"));
            }
            ModCmd::NoteModify { id, text } => {
                if !(NOTE_MIN..=NOTE_MAX).contains(&text.chars().count()) {
                    return Ok(format!(
                        "A note must be {NOTE_MIN} to {NOTE_MAX} characters long"
                    ));
                }
                let n = sqlx::query("UPDATE notes SET text=$3 WHERE guild_id=$1 AND id=$2")
                    .bind(g)
                    .bind(id)
                    .bind(text)
                    .execute(&app.pool)
                    .await?
                    .rows_affected();
                return Ok(match n {
                    0 => format!("No note #{id}"),
                    _ => format!("Note #{id} updated"),
                });
            }
            ModCmd::NoteList { user, id, by } => {
                return crate::modview::view_notes(app, g, user, id, by).await
            }
            ModCmd::NoteRemove { id, user } => {
                let n = sqlx::query(
                    "DELETE FROM notes WHERE guild_id=$1 AND id=$2 AND ($3::bigint IS NULL OR user_id=$3)",
                )
                .bind(g)
                .bind(id)
                .bind(user.map(|u| u as i64))
                    .execute(&app.pool)
                    .await?
                    .rows_affected();
                return Ok(match n {
                    0 => format!("No note #{id}"),
                    _ => format!("Removed note #{id}"),
                });
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
                return Ok(format!("Removed {n} notes"));
            }
            ModCmd::CasesEdit { case, reason } => {
                let done =
                    sqlx::query("UPDATE cases SET reason=$3 WHERE guild_id=$1 AND case_no=$2")
                        .bind(g)
                        .bind(case)
                        .bind(&reason)
                        .execute(&app.pool)
                        .await?
                        .rows_affected();
                if done > 0 {
                    let log = Log::new("Case Updated", ORANGE)
                        .line("Case", &format!("`{case}`"))
                        .line("Moderator", &embed::user(actor_id.get()))
                        .line("New Reason", &reason);
                    send_log(app, gid.get(), LogKind::ModCommand, log)
                        .await
                        .ok();
                }
                return Ok(match done {
                    0 => format!("No case #{case}"),
                    _ => format!("Case #{case} reason updated"),
                });
            }
            ModCmd::CaseSet { case, active } => {
                let done =
                    sqlx::query("UPDATE cases SET active=$3 WHERE guild_id=$1 AND case_no=$2")
                        .bind(g)
                        .bind(case)
                        .bind(active)
                        .execute(&app.pool)
                        .await?
                        .rows_affected();
                return Ok(match (done, active) {
                    (0, _) => format!("No case #{case}"),
                    (_, true) => format!("Case #{case} is active again"),
                    _ => format!("Case #{case} closed"),
                });
            }
            ModCmd::ClearWarns { user } => {
                cases::reset_warns(&app.pool, g, user as i64).await?;
                return Ok(format!("Cleared the warns of <@{user}>"));
            }
            ModCmd::Unban { user } => {
                app.http
                    .delete_ban(gid, Id::new(user))
                    .reason(&audit)
                    .await?;
                ("unban", user, None, None, format!("Unbanned <@{user}>"))
            }
            ModCmd::Hackban { user } => {
                let secs = app.cfg(gid.get()).await?.misc.purge_secs();
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
                app.http
                    .update_guild_member(gid, Id::new(user))
                    .communication_disabled_until(None)
                    .reason(&audit)
                    .await?;
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
                    return Ok(format!("No active warn case #{case}"));
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
                antinuke::quarantine(app, gid, Id::new(user)).await?;
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
                    return Ok(format!("<@{user}> is not quarantined"));
                }
                antinuke::run_inverse(app, gid, &Inverse::Unquarantine { user }).await?;
                let text = format!("Released <@{user}> from quarantine");
                ("unquarantine", user, None, None, text)
            }
            ModCmd::Verify { user } => {
                let cfg = app.cfg(gid.get()).await?;
                // Wick's verify also ends a quarantine.
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
                    return Ok("Set the verification roles on the dashboard first".into());
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
                    return Ok(format!("<@{user}> already has a clean name"));
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
                    return Ok("No deletable messages found (over 14 days old are skipped)".into());
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
                    app.http
                        .update_channel(Id::new(*c))
                        .rate_limit_per_user(secs)
                        .reason(&audit)
                        .await?;
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
    Ok(format!("Case #{case_no}: {reply}{hint}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefix::parse;

    fn pf(s: &str, resolved: Option<u64>) -> Result<ModCmd, String> {
        from_prefix(&parse(s).unwrap(), resolved)
    }

    #[test]
    fn prefix_parsing() {
        let big = 123_456_789_012_345_678u64;
        assert_eq!(
            pf(&format!("unban {big}"), None),
            Ok(ModCmd::Unban { user: big })
        );
        assert!(pf("unban", None).is_err());
        assert_eq!(
            pf("untimeout bob", Some(5_000_000)),
            Ok(ModCmd::Untimeout { user: 5_000_000 })
        );
        assert_eq!(pf("unwarn 7", None), Ok(ModCmd::Unwarn { case: 7 }));
        assert_eq!(
            pf(&format!("purge 50 {big}"), None),
            Ok(ModCmd::Purge {
                count: 50,
                user: Some(big),
                from: None,
                filters: vec![]
            })
        );
        assert!(pf("purge 5000", None).is_err());
        assert_eq!(
            pf("slowmode 10s", None),
            Ok(ModCmd::Slowmode {
                secs: 10,
                channels: vec![]
            })
        );
        assert_eq!(
            pf("slowmode off", None),
            Ok(ModCmd::Slowmode {
                secs: 0,
                channels: vec![]
            })
        );
        assert!(pf("slowmode 7h", None).is_err());
        assert_eq!(
            pf("sm <#5000000> <#6000000> 1h", None),
            Ok(ModCmd::Slowmode {
                secs: 3600,
                channels: vec![5_000_000, 6_000_000]
            })
        );
        assert_eq!(
            pf("cases modify 3 ?r typo", None),
            Ok(ModCmd::CasesEdit {
                case: 3,
                reason: "typo".into()
            })
        );
        assert_eq!(
            pf("cases 3", None),
            Ok(ModCmd::CasesView {
                case: Some(3),
                user: None,
                by: None,
                kind: None
            })
        );
        assert_eq!(
            pf("cases", None),
            Ok(ModCmd::CasesView {
                case: None,
                user: None,
                by: None,
                kind: None
            })
        );
        assert_eq!(
            pf("editc 3 fixed typo", None),
            Ok(ModCmd::CasesEdit {
                case: 3,
                reason: "fixed typo".into()
            })
        );
        assert!(pf("editreason 3", None).is_err());
        assert_eq!(
            pf("caseoff 4", None),
            Ok(ModCmd::CaseSet {
                case: 4,
                active: false
            })
        );
        assert_eq!(
            pf("opencase 4", None),
            Ok(ModCmd::CaseSet {
                case: 4,
                active: true
            })
        );
        assert_eq!(
            pf("clearwarns 5000000", None),
            Ok(ModCmd::ClearWarns { user: 5_000_000 })
        );
    }

    #[test]
    fn multi_target_and_purge_aliases() {
        let cmd = ModCmd::Unban { user: 1 };
        assert_eq!(cmd.with_user(2), Some(ModCmd::Unban { user: 2 }));
        assert_eq!(
            ModCmd::Slowmode {
                secs: 1,
                channels: vec![]
            }
            .with_user(2),
            None
        );
        let big = 123_456_789_012_345_678u64;
        let c = pf(&format!("bc 20 {big}"), None).unwrap();
        let ModCmd::Purge { filters, .. } = c else {
            panic!()
        };
        assert_eq!(filters, vec![purge::Filter::Bots]);
        assert_eq!(
            purge::Filter::parse("-sus", None),
            Ok(purge::Filter::Suspicious)
        );
        assert_eq!(purge::Filter::parse("@", None), Ok(purge::Filter::Mentions));
        assert_eq!(
            purge::Filter::parse("=", Some("Hi")),
            Ok(purge::Filter::Equals("hi".into()))
        );
    }

    #[test]
    fn note_prefix() {
        let big = 123_456_789_012_345_678u64;
        assert_eq!(
            pf(&format!("note add {big} ?r rude in vc"), None),
            Ok(ModCmd::NoteAdd {
                user: big,
                text: "rude in vc".into()
            })
        );
        assert!(pf(&format!("note add {big}"), None).is_err()); // text required
        assert_eq!(
            pf("note remove 4", None),
            Ok(ModCmd::NoteRemove { id: 4, user: None })
        );
        assert_eq!(
            pf(&format!("note {big}"), None),
            Ok(ModCmd::NoteList {
                user: Some(big),
                id: None,
                by: None
            })
        );
        assert_eq!(
            ModCmd::NoteList {
                user: None,
                id: None,
                by: None
            }
            .perm(),
            "notes_view"
        );
    }

    #[test]
    fn dehoist_alias() {
        assert_eq!(
            pf("dehoist 5000000", None),
            Ok(ModCmd::Sanitize { user: 5_000_000 })
        );
    }
}
