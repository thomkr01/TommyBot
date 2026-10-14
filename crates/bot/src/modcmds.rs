//! Moderation commands beyond ban/kick/timeout/warn: unban, untimeout, unwarn, hackban,
//! quarantine, purge, slowmode, verify, sanitize and case viewing/editing. One implementation, prefix and slash parsers.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_antinuke::Inverse;
use tommybot_core::{cases, permits};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            application_command::{CommandDataOption, CommandOptionValue},
            InteractionContextType,
        },
    },
    gateway::payload::incoming::MessageCreate,
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::command::{
    CommandBuilder, IntegerBuilder, StringBuilder, SubCommandBuilder, UserBuilder,
};

use crate::{
    antinuke,
    app::{now_secs, App},
    commands::{self, gated, target_above_bot},
    prefix::Parsed,
    slash::{int, string, user},
    targets,
};

#[derive(Debug, PartialEq, Eq)]
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
        /// Start at this message id (message context menu) instead of the newest.
        from: Option<u64>,
    },
    Slowmode {
        secs: u16,
    },
    CasesView {
        case: Option<i32>,
        user: Option<u64>,
    },
    CasesEdit {
        case: i32,
        reason: String,
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
    NoteList {
        user: u64,
    },
    NoteRemove {
        id: i64,
    },
    NoteSweep {
        user: u64,
    },
}

/// Numbers below this are case numbers / counts, not snowflakes.
const SMALL: u64 = 1_000_000;
const MAX_SLOWMODE: u64 = 21_600;
pub(crate) const PURGE_MAX: usize = 100;
const NOTE_MAX: usize = 500;
/// Discord refuses bulk deletes of messages older than 14 days (a minute of margin).
const BULK_AGE_S: i64 = 14 * 86_400 - 60;

impl ModCmd {
    /// Permit name and rate-limit action. Unban/hackban share `ban`, untimeout shares `timeout`.
    fn perm(&self) -> &'static str {
        match self {
            Self::Unban { .. } | Self::Hackban { .. } => "ban",
            Self::Untimeout { .. } => "timeout",
            Self::Unwarn { .. } => "warn",
            Self::Quarantine { .. } | Self::Unquarantine { .. } => "quarantine",
            Self::Purge { .. } => "purge",
            Self::NoteList { .. } => "notes_view",
            Self::NoteAdd { .. } | Self::NoteRemove { .. } | Self::NoteSweep { .. } => {
                "notes_manage"
            }
            Self::Slowmode { .. } => "slowmode",
            Self::CasesView { .. } | Self::CasesEdit { .. } => "cases",
            Self::Verify { .. } => "verify",
            Self::Sanitize { .. } => "sanitize",
        }
    }

    /// The member the permit/hierarchy check applies to.
    fn target(&self) -> Option<u64> {
        match *self {
            Self::Unban { user }
            | Self::Hackban { user }
            | Self::Untimeout { user }
            | Self::Quarantine { user }
            | Self::Unquarantine { user }
            | Self::Sanitize { user } => Some(user),
            _ => None,
        }
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
        "quarantine" => ModCmd::Quarantine { user: need_user()? },
        "unquarantine" => ModCmd::Unquarantine { user: need_user()? },
        "verify" => ModCmd::Verify { user: need_user()? },
        "sanitize" | "dehoist" => ModCmd::Sanitize { user: need_user()? },
        "purge" | "clear" => ModCmd::Purge {
            count: small
                .filter(|n| (1..=PURGE_MAX as u64).contains(n))
                .ok_or("Give a message count from 1 to 100")? as usize,
            user,
            from: None,
        },
        "slowmode" => {
            let off = p.names.iter().any(|n| n == "off");
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
            }
        }
        "note" | "notes" => {
            let (sub, text) = (p.names.first().map(String::as_str), p.reason.clone());
            match sub {
                Some("add") => ModCmd::NoteAdd {
                    user: need_user()?,
                    text: text.ok_or("Give the note with ?r")?,
                },
                Some("remove") => ModCmd::NoteRemove { id: case()? as i64 },
                Some("sweep") => ModCmd::NoteSweep { user: need_user()? },
                _ => ModCmd::NoteList { user: need_user()? },
            }
        }
        "cases" | "case" if p.names.first().is_some_and(|n| n == "edit") => ModCmd::CasesEdit {
            case: case()?,
            reason: p.reason.clone().ok_or("Give a new reason with ?r")?,
        },
        "cases" | "case" => {
            let case = small.map(|n| n as i32);
            if case.is_none() && user.is_none() {
                return Err("Give a case number or a user".into());
            }
            ModCmd::CasesView { case, user }
        }
        _ => return Err("Unknown command".into()),
    })
}

/// Prefix entry for the commands above; the caller only routes known names here.
pub async fn run_prefix(app: &App, msg: &MessageCreate, p: Parsed) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    let mut resolved = None;
    let name = p.names.iter().find(|n| {
        !matches!(
            n.as_str(),
            "edit" | "view" | "off" | "add" | "remove" | "sweep" | "list"
        )
    });
    if let (Some(n), true) = (name, p.targets.iter().all(|t| *t < SMALL)) {
        match targets::resolve(app, gid, n).await? {
            Ok(id) => resolved = Some(id.get()),
            Err(e) => return commands::say(app, msg, &e).await,
        }
    }
    let text = match from_prefix(&p, resolved) {
        Err(e) => e,
        Ok(cmd) => {
            let owner = app.guild_info(gid).await?.owner;
            let actor = app.level(gid, owner, msg.author.id).await?;
            let actor = (msg.author.id, &actor);
            run(
                app,
                gid,
                owner,
                actor,
                msg.channel_id,
                cmd,
                p.reason.as_deref(),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(?e, "mod command failed");
                "Command failed, see logs".into()
            })
        }
    };
    commands::say(app, msg, &text).await
}

/// Permit check -> hierarchy -> rate limit -> action -> case. Returns the reply text.
pub async fn run(
    app: &App,
    gid: Id<GuildMarker>,
    owner: Id<UserMarker>,
    (actor_id, actor): (Id<UserMarker>, &permits::Level),
    channel: Id<ChannelMarker>,
    cmd: ModCmd,
    reason: Option<&str>,
) -> Result<String> {
    let perm = cmd.perm();
    if let Some(t) = cmd.target() {
        let target = Id::new(t);
        let level = app.level(gid, owner, target).await?;
        let above = target_above_bot(app, gid, target).await?;
        let actor_n = actor_id.get() as i64;
        if let Err(d) = permits::check(actor, perm, actor_n, &level, t as i64, above) {
            return Ok(format!("Cannot {perm} <@{t}>: {d:?}"));
        }
    } else if !actor.has(perm) {
        return Ok(format!("Cannot use `{perm}`: no permit"));
    }
    if let Some(msg) = gated(app, gid, actor_id, actor, perm).await? {
        return Ok(msg);
    }
    let g = gid.get() as i64;
    // (case kind, case user, inverse, reason prefix, reply)
    let (kind, uid, inverse, note, reply): (&str, u64, Option<Value>, Option<String>, String) =
        match cmd {
            ModCmd::CasesView { case, user } => return view_cases(app, g, case, user).await,
            ModCmd::NoteAdd { user, text } => {
                let text: String = text.chars().take(NOTE_MAX).collect();
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
            ModCmd::NoteList { user } => return view_notes(app, g, user).await,
            ModCmd::NoteRemove { id } => {
                let n = sqlx::query("DELETE FROM notes WHERE guild_id=$1 AND id=$2")
                    .bind(g)
                    .bind(id)
                    .execute(&app.pool)
                    .await?
                    .rows_affected();
                return Ok(match n {
                    0 => format!("No note #{id}"),
                    _ => format!("Removed note #{id}"),
                });
            }
            ModCmd::NoteSweep { user } => {
                let n = sqlx::query("DELETE FROM notes WHERE guild_id=$1 AND user_id=$2")
                    .bind(g)
                    .bind(user as i64)
                    .execute(&app.pool)
                    .await?
                    .rows_affected();
                return Ok(format!("Removed {n} notes of <@{user}>"));
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
                return Ok(match done {
                    0 => format!("No case #{case}"),
                    _ => format!("Case #{case} reason updated"),
                });
            }
            ModCmd::Unban { user } => {
                app.http.delete_ban(gid, Id::new(user)).await?;
                ("unban", user, None, None, format!("Unbanned <@{user}>"))
            }
            ModCmd::Hackban { user } => {
                app.http.create_ban(gid, Id::new(user)).await?;
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
                if cfg.verification.roles.is_empty() {
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
                let clean = sanitized(&name);
                if clean == name {
                    return Ok(format!("<@{user}> already has a clean name"));
                }
                app.http
                    .update_guild_member(gid, Id::new(user))
                    .nick(Some(&clean))
                    .await?;
                let note = format!("`{name}` -> `{clean}`");
                let text = format!("Renamed <@{user}> to `{clean}`");
                ("sanitize", user, None, Some(note), text)
            }
            ModCmd::Purge { count, user, from } => {
                let n = purge(app, channel, count, user, from).await?;
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
            ModCmd::Slowmode { secs } => {
                app.http
                    .update_channel(channel)
                    .rate_limit_per_user(secs)
                    .await?;
                let note = format!("{secs}s in <#{channel}>");
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
            inverse,
        },
    )
    .await?;
    Ok(format!("Case #{case_no}: {reply}"))
}

fn sanitized(name: &str) -> String {
    let ascii: String = name
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let s = ascii
        .trim_start_matches(|c: char| !c.is_ascii_alphabetic())
        .trim_end();
    if s.is_empty() {
        "Moderated Nickname".into()
    } else {
        s.into()
    }
}

/// (message id, author id, created unix secs), newest first.
type Msg = (u64, u64, i64);

/// Up to `count` ids by `user` (any author when None) that bulk delete still accepts.
fn purge_ids(msgs: &[Msg], count: usize, user: Option<u64>, now: i64) -> Vec<u64> {
    msgs.iter()
        .filter(|(_, author, ts)| user.is_none_or(|u| u == *author) && now - ts < BULK_AGE_S)
        .take(count)
        .map(|m| m.0)
        .collect()
}

/// Scans the last 100 messages (so a user filter may find fewer than `count`); returns how many were deleted.
async fn purge(
    app: &App,
    ch: Id<ChannelMarker>,
    count: usize,
    user: Option<u64>,
    from: Option<u64>,
) -> Result<usize> {
    let req = app.http.channel_messages(ch);
    let page = match from {
        // `after` is exclusive, so start one id earlier to include the clicked message.
        Some(m) => {
            req.after(Id::new(m.max(2) - 1))
                .limit(PURGE_MAX as u16)
                .await?
        }
        None => req.limit(PURGE_MAX as u16).await?,
    };
    let msgs: Vec<Msg> = page
        .models()
        .await?
        .iter()
        .map(|m| (m.id.get(), m.author.id.get(), m.timestamp.as_secs()))
        .collect();
    let ids: Vec<Id<MessageMarker>> = purge_ids(&msgs, count, user, now_secs())
        .into_iter()
        .map(Id::new)
        .collect();
    match ids.as_slice() {
        [] => {}
        [one] => {
            app.http.delete_message(ch, *one).await?;
        }
        many => {
            app.http.delete_messages(ch, many).await?;
        }
    }
    Ok(ids.len())
}

type CaseRow = (i32, String, i64, i64, Option<String>, bool, i64);

fn fmt_case((no, kind, user, moderator, reason, active, at): &CaseRow) -> String {
    format!(
        "`#{no}` {kind}{} <@{user}> by <@{moderator}> <t:{at}:R>: {}",
        if *active { "" } else { " (inactive)" },
        reason.as_deref().unwrap_or("no reason")
    )
}

/// One case by number, or the 10 latest (optionally of one user).
async fn view_cases(app: &App, g: i64, case: Option<i32>, user: Option<u64>) -> Result<String> {
    let rows: Vec<CaseRow> = sqlx::query_as(
        "SELECT case_no, kind, user_id, mod_id, reason, active, EXTRACT(EPOCH FROM created_at)::bigint
         FROM cases WHERE guild_id=$1 AND ($2::int IS NULL OR case_no=$2) AND ($3::bigint IS NULL OR user_id=$3)
         ORDER BY case_no DESC LIMIT 10",
    )
    .bind(g)
    .bind(case)
    .bind(user.map(|u| u as i64))
    .fetch_all(&app.pool)
    .await?;
    Ok(if rows.is_empty() {
        "No matching cases".into()
    } else {
        rows.iter().map(fmt_case).collect::<Vec<_>>().join("\n")
    })
}

async fn view_notes(app: &App, g: i64, user: u64) -> Result<String> {
    let rows: Vec<(i64, i64, String, i64)> = sqlx::query_as(
        "SELECT id, author_id, text, EXTRACT(EPOCH FROM created_at)::bigint FROM notes
         WHERE guild_id=$1 AND user_id=$2 ORDER BY id DESC LIMIT 10",
    )
    .bind(g)
    .bind(user as i64)
    .fetch_all(&app.pool)
    .await?;
    Ok(if rows.is_empty() {
        format!("No notes for <@{user}>")
    } else {
        rows.iter()
            .map(|(id, by, text, at)| format!("`#{id}` by <@{by}> <t:{at}:R>: {text}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Slash options -> command plus the optional `reason`.
pub fn from_slash(name: &str, o: &[CommandDataOption]) -> Result<(ModCmd, Option<String>), String> {
    let u = || user(o, "user").map(|u| u.get()).ok_or("missing user");
    let case = |o| int(o, "case").map(|n| n as i32).ok_or("missing case");
    let cmd = match name {
        "unban" => ModCmd::Unban { user: u()? },
        "hackban" => ModCmd::Hackban {
            user: string(o, "user_id")
                .and_then(|s| s.trim().parse().ok())
                .filter(|id| *id >= SMALL)
                .ok_or("`user_id` must be a numeric user id")?,
        },
        "untimeout" => ModCmd::Untimeout { user: u()? },
        "unwarn" => ModCmd::Unwarn { case: case(o)? },
        "quarantine" => ModCmd::Quarantine { user: u()? },
        "unquarantine" => ModCmd::Unquarantine { user: u()? },
        "verify" => ModCmd::Verify { user: u()? },
        "sanitize" => ModCmd::Sanitize { user: u()? },
        "purge" => ModCmd::Purge {
            count: int(o, "count")
                .ok_or("missing count")?
                .clamp(1, PURGE_MAX as i64) as usize,
            user: user(o, "user").map(|u| u.get()),
            from: None,
        },
        "slowmode" => ModCmd::Slowmode {
            secs: int(o, "seconds")
                .ok_or("missing seconds")?
                .clamp(0, MAX_SLOWMODE as i64) as u16,
        },
        "note" => {
            let Some(CommandDataOption {
                name,
                value: CommandOptionValue::SubCommand(s),
            }) = o.first()
            else {
                return Err("Pick a subcommand".into());
            };
            let u = || user(s, "user").map(|u| u.get()).ok_or("missing user");
            match name.as_str() {
                "add" => ModCmd::NoteAdd {
                    user: u()?,
                    text: string(s, "text").ok_or("missing text")?.into(),
                },
                "list" => ModCmd::NoteList { user: u()? },
                "remove" => ModCmd::NoteRemove {
                    id: int(s, "id").ok_or("missing id")?,
                },
                "sweep" => ModCmd::NoteSweep { user: u()? },
                _ => return Err("Unknown subcommand".into()),
            }
        }
        "cases" => {
            let Some(CommandDataOption {
                name,
                value: CommandOptionValue::SubCommand(s),
            }) = o.first()
            else {
                return Err("Pick a subcommand".into());
            };
            match name.as_str() {
                "view" => ModCmd::CasesView {
                    case: case(s).ok(),
                    user: user(s, "user").map(|u| u.get()),
                },
                "edit" => ModCmd::CasesEdit {
                    case: case(s)?,
                    reason: string(s, "reason").ok_or("missing reason")?.into(),
                },
                _ => return Err("Unknown subcommand".into()),
            }
        }
        _ => return Err("Unknown command".into()),
    };
    Ok((cmd, string(o, "reason").map(String::from)))
}

pub fn definitions() -> Vec<Command> {
    let user = |d| UserBuilder::new("user", d).required(true);
    let reason = || StringBuilder::new("reason", "Reason for the case");
    let case = |req| {
        IntegerBuilder::new("case", "Case number")
            .min_value(1)
            .required(req)
    };
    let chat = |n, d| {
        CommandBuilder::new(n, d, CommandType::ChatInput).contexts([InteractionContextType::Guild])
    };
    vec![
        chat("unban", "Unban a user")
            .option(user("User to unban"))
            .option(reason())
            .build(),
        chat("hackban", "Ban someone by id, even if not in the server")
            .option(StringBuilder::new("user_id", "User id").required(true))
            .option(reason())
            .build(),
        chat("untimeout", "Remove a member's timeout")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("unwarn", "Deactivate a warn case")
            .option(case(true))
            .option(reason())
            .build(),
        chat("quarantine", "Strip a member's roles")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("unquarantine", "Restore a quarantined member's roles")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("verify", "Manually verify a member")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("sanitize", "Clean up a member's hoisted or unreadable name")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("purge", "Bulk delete recent messages here")
            .option(
                IntegerBuilder::new("count", "How many (1-100)")
                    .min_value(1)
                    .max_value(100)
                    .required(true),
            )
            .option(UserBuilder::new("user", "Only this user's messages"))
            .option(reason())
            .build(),
        chat("slowmode", "Set this channel's slowmode")
            .option(
                IntegerBuilder::new("seconds", "0 turns it off")
                    .min_value(0)
                    .max_value(21_600)
                    .required(true),
            )
            .option(reason())
            .build(),
        chat("note", "Staff notes about members")
            .option(
                SubCommandBuilder::new("add", "Add a note")
                    .option(user("Member"))
                    .option(StringBuilder::new("text", "The note").required(true)),
            )
            .option(
                SubCommandBuilder::new("list", "Show a member's latest notes")
                    .option(user("Member")),
            )
            .option(
                SubCommandBuilder::new("remove", "Remove one note").option(
                    IntegerBuilder::new("id", "Note number")
                        .min_value(1)
                        .required(true),
                ),
            )
            .option(
                SubCommandBuilder::new("sweep", "Remove all notes of a member")
                    .option(user("Member")),
            )
            .build(),
        chat("cases", "View or edit cases")
            .option(
                SubCommandBuilder::new("view", "Show a case, or the latest cases of a user")
                    .option(case(false))
                    .option(UserBuilder::new("user", "Member")),
            )
            .option(
                SubCommandBuilder::new("edit", "Change a case's reason")
                    .option(case(true))
                    .option(StringBuilder::new("reason", "New reason").required(true)),
            )
            .build(),
    ]
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
                from: None
            })
        );
        assert!(pf("purge 500", None).is_err());
        assert_eq!(pf("slowmode 10s", None), Ok(ModCmd::Slowmode { secs: 10 }));
        assert_eq!(pf("slowmode off", None), Ok(ModCmd::Slowmode { secs: 0 }));
        assert!(pf("slowmode 7h", None).is_err());
        assert_eq!(
            pf("cases edit 3 ?r typo", None),
            Ok(ModCmd::CasesEdit {
                case: 3,
                reason: "typo".into()
            })
        );
        assert_eq!(
            pf("cases 3", None),
            Ok(ModCmd::CasesView {
                case: Some(3),
                user: None
            })
        );
        assert!(pf("cases", None).is_err());
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
        assert_eq!(pf("note remove 4", None), Ok(ModCmd::NoteRemove { id: 4 }));
        assert_eq!(
            pf(&format!("note {big}"), None),
            Ok(ModCmd::NoteList { user: big })
        );
        assert_eq!(ModCmd::NoteList { user: 1 }.perm(), "notes_view");
    }

    #[test]
    fn purge_filters_user_age_count() {
        let now = 10_000_000;
        let old = now - BULK_AGE_S - 1;
        let msgs = [(5, 1, now), (4, 2, now - 5), (3, 1, now - 10), (2, 1, old)];
        assert_eq!(purge_ids(&msgs, 2, None, now), vec![5, 4]);
        assert_eq!(purge_ids(&msgs, 10, Some(1), now), vec![5, 3]); // old one skipped
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitized("!!!Bob"), "Bob");
        assert_eq!(sanitized("  .1ｂｏｂ the Builder"), "the Builder");
        assert_eq!(sanitized("Ann-Marie 2"), "Ann-Marie 2");
        assert_eq!(sanitized("⚡⚡"), "Moderated Nickname");
        assert_eq!(
            pf("dehoist 5000000", None),
            Ok(ModCmd::Sanitize { user: 5_000_000 })
        );
    }

    #[test]
    fn case_line() {
        let row = (3, "warn".to_string(), 9, 8, None, false, 100);
        assert_eq!(
            fmt_case(&row),
            "`#3` warn (inactive) <@9> by <@8> <t:100:R>: no reason"
        );
    }
}
