//! Confirm / Cancel prompts: a mod command parked in `pending_confirms` until its buttons are clicked.
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tommybot_core::jobs;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, Embed, MessageFlags,
    },
    gateway::payload::incoming::MessageCreate,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::App,
    commands::{apply_dm, ModKind},
    modcmds::{self, ModCmd},
    prefix::Flags,
    reply::{self, Applied},
};
use tommybot_logging::embed::{Log, ARROW, BLUE};

/// What a [`Pending`] does once confirmed.
#[derive(Serialize, Deserialize)]
pub enum Action {
    Mod {
        kind: ModKind,
        targets: Vec<Id<UserMarker>>,
        reason: Option<String>,
        duration_s: Option<i64>,
        flags: Flags,
        #[serde(default)]
        proof: Option<String>,
        /// A target typed as a raw id or a name, not a mention: worth a Yes/No first.
        #[serde(default)]
        unsure: bool,
    },
    Ext {
        cmd: ModCmd,
        channel: Id<ChannelMarker>,
        reason: Option<String>,
        #[serde(default)]
        proof: Option<String>,
        /// Quarantine, unquarantine and unban run once per member here; empty runs `cmd` as is.
        #[serde(default)]
        users: Vec<u64>,
        #[serde(default)]
        unsure: bool,
        /// The `?dm` flag.
        #[serde(default)]
        dm: bool,
    },
}

/// A command waiting for its Confirm click, stored in `pending_confirms` so it survives a restart.
pub struct Pending {
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    action: Action,
}

pub const CONFIRM_TTL: Duration = Duration::from_secs(10);

impl Pending {
    pub fn new(gid: Id<GuildMarker>, actor: Id<UserMarker>, action: Action) -> Self {
        Self { gid, actor, action }
    }

    /// Mod commands ask when a target was typed as an id or a name (a mention is sure), and
    /// hackbans always; destructive extras (purge, note sweep) always ask.
    /// `enabled` is the server's `misc.confirm`: off never asks.
    pub fn needs_confirm(&self, enabled: bool) -> bool {
        enabled
            && match &self.action {
                Action::Mod {
                    targets,
                    unsure,
                    flags,
                    ..
                } => !targets.is_empty() && (*unsure || flags.hack),
                Action::Ext { cmd, unsure, .. } => {
                    cmd.destructive() || (*unsure && cmd.asks_when_unsure())
                }
            }
    }

    fn question(&self) -> Embed {
        let target = match &self.action {
            Action::Mod { targets, .. } => {
                let who: Vec<String> = targets.iter().map(|t| format!("<@{t}> [`{t}`]")).collect();
                who.join("\n")
            }
            Action::Ext { users, .. } if !users.is_empty() => {
                let who: Vec<String> = users.iter().map(|t| format!("<@{t}> [`{t}`]")).collect();
                who.join("\n")
            }
            Action::Ext { cmd, .. } => cmd.describe(),
        };
        Log::new("Do you want to proceed?", BLUE)
            .text(&format!("{ARROW} **Target:**"))
            .text(&target)
            .text(&format!(
                "Press Yes or No. You have {} seconds.",
                CONFIRM_TTL.as_secs()
            ))
            .build()
    }

    /// One outcome per target.
    pub(crate) async fn run(
        &self,
        app: &App,
        owner: Id<UserMarker>,
        points: i32,
    ) -> Result<Vec<Applied>> {
        let (kind, targets, reason, duration_s, flags, proof) = match &self.action {
            Action::Mod {
                kind,
                targets,
                reason,
                duration_s,
                flags,
                proof,
                ..
            } => (*kind, targets, reason, *duration_s, *flags, proof),
            Action::Ext {
                cmd,
                channel,
                reason,
                proof,
                users,
                dm,
                ..
            } => {
                let cmds: Vec<ModCmd> = users.iter().filter_map(|u| cmd.with_user(*u)).collect();
                let cmds = if cmds.is_empty() {
                    vec![cmd.clone()]
                } else {
                    cmds
                };
                let mut out = vec![];
                for cmd in cmds {
                    // Fresh level each time: a rate-limit quarantine mid-list must stop the rest.
                    let actor = app.level(self.gid, owner, self.actor).await?;
                    let done = modcmds::run(
                        app,
                        self.gid,
                        owner,
                        (self.actor, &actor),
                        *channel,
                        cmd,
                        (reason.as_deref(), proof.as_deref()),
                        *dm,
                    )
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(?e, "mod command failed");
                        format!("Command failed: {}", crate::http_err::why(&e)).into()
                    });
                    out.push(done);
                }
                return Ok(out);
            }
        };
        let mut out = vec![];
        for &t in targets {
            // Fresh level each time: a rate-limit quarantine mid-list must stop the rest.
            let actor = app.level(self.gid, owner, self.actor).await?;
            let done = match apply_dm(
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
                proof.as_deref(),
            )
            .await
            {
                Ok(done) => done,
                Err(e) => {
                    tracing::warn!(?e, "mod command failed");
                    let text = format!(
                        "Failed to {} <@{t}>: {}",
                        kind.name(),
                        crate::http_err::why(&e)
                    );
                    Applied {
                        target: Some(t.get()),
                        ..text.into()
                    }
                }
            };
            out.push(done);
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

/// The question for a warn on `target`, for the dashboard preview.
pub fn sample_question(target: u64) -> (Embed, Component) {
    let action = Action::Mod {
        kind: ModKind::Warn,
        targets: vec![Id::new(target)],
        reason: None,
        duration_s: None,
        flags: Flags::default(),
        proof: None,
        unsure: true,
    };
    let job = Pending::new(Id::new(1), Id::new(1), action);
    (job.question(), job.row(1))
}

impl Pending {
    fn row(&self, nonce: u64) -> Component {
        Component::ActionRow(ActionRow {
            id: None,
            components: vec![
                confirm_button(
                    "Yes",
                    format!("{CONFIRM}{nonce}:{}:y", self.actor),
                    ButtonStyle::Success,
                ),
                confirm_button(
                    "No",
                    format!("{CONFIRM}{nonce}:{}:n", self.actor),
                    ButtonStyle::Danger,
                ),
            ],
        })
    }
}

/// Stores `job` for its buttons and returns the question with the Yes / No row.
/// `nonce` is the command message or interaction id.
pub async fn stash_confirm(app: &App, nonce: u64, job: Pending) -> Result<(Embed, Component)> {
    let row = job.row(nonce);
    // Expired rows are dropped on the way in, so the table stays small without a sweeper.
    sqlx::query(
        "WITH gone AS (DELETE FROM pending_confirms WHERE created_at < now() - make_interval(secs => $4))
         INSERT INTO pending_confirms (nonce, guild_id, action) VALUES ($1, $2, $3)",
    )
    .bind(nonce as i64)
    .bind(job.gid.get() as i64)
    .bind(serde_json::to_value(&job.action)?)
    // A minute past expiry, so `run_expire` still finds the row and closes the question.
    .bind(CONFIRM_TTL.as_secs_f64() + 60.0)
    .execute(&app.pool)
    .await?;
    Ok((job.question(), row))
}

pub async fn ask_confirm(app: &App, msg: &MessageCreate, job: Pending) -> Result<()> {
    let nonce = msg.id.get();
    let (embed, row) = stash_confirm(app, nonce, job).await?;
    let sent = app
        .http
        .create_message(msg.channel_id)
        .embeds(&[embed])
        .components(&[row])
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?
        .model()
        .await?;
    let gid = msg.guild_id.context("not in a server")?;
    expire_later(app, gid, nonce, Asked::Message(msg.channel_id, sent.id)).await
}

/// Where a question was posted, so it can be closed when nobody answers.
pub enum Asked {
    Message(Id<ChannelMarker>, Id<MessageMarker>),
    /// A slash command's interaction token (valid 15 minutes).
    Interaction(String),
}

const TIMED_OUT: &str = "Timed out, nothing was done.";

/// Queues the `confirm_expire` job that closes the question after [`CONFIRM_TTL`] (the
/// scheduler ticks every 5 s, so it closes 10 to 15 s after asking).
pub async fn expire_later(app: &App, gid: Id<GuildMarker>, nonce: u64, at: Asked) -> Result<()> {
    let job = match at {
        Asked::Message(c, m) => json!({"nonce": nonce, "channel": c.get(), "message": m.get()}),
        Asked::Interaction(token) => json!({"nonce": nonce, "token": token}),
    };
    let when = crate::app::now_secs() + CONFIRM_TTL.as_secs() as i64;
    jobs::enqueue(&app.pool, gid.get() as i64, "confirm_expire", job, when).await?;
    Ok(())
}

/// Scheduler arm for `confirm_expire`: swaps an unanswered question for [`TIMED_OUT`]. The row is
/// taken first, so a click at the same moment either runs the command or sees it expired.
pub async fn run_expire(app: &App, p: &Value) -> Result<()> {
    let nonce = p["nonce"].as_i64().context("no nonce")?;
    let gone = sqlx::query("DELETE FROM pending_confirms WHERE nonce = $1")
        .bind(nonce)
        .execute(&app.pool)
        .await?;
    if gone.rows_affected() == 0 {
        return Ok(());
    }
    let none: &[Embed] = &[];
    let r = match (
        p["token"].as_str(),
        p["channel"].as_u64(),
        p["message"].as_u64(),
    ) {
        (Some(token), ..) => app
            .http
            .interaction(Id::new(app.me.get()))
            .update_response(token)
            .content(Some(TIMED_OUT))
            .embeds(Some(none))
            .components(Some(&[]))
            .await
            .map(drop),
        (None, Some(c), Some(m)) => app
            .http
            .update_message(Id::new(c), Id::new(m))
            .content(Some(TIMED_OUT))
            .embeds(Some(none))
            .components(Some(&[]))
            .await
            .map(drop),
        _ => return Ok(()),
    };
    // The question may already be deleted; nothing else to do then.
    crate::http_err::best_effort(r, "close timed out question");
    Ok(())
}

const CONFIRM: &str = "confirm:";

/// `confirm:<nonce>:<actor>:<y|n>` -> (nonce, actor, yes).
fn parse_answer(custom_id: &str) -> Option<(i64, u64, bool)> {
    let mut it = custom_id.strip_prefix(CONFIRM)?.splitn(3, ':');
    let (n, actor, a) = (it.next()?, it.next()?, it.next()?);
    Some((n.parse().ok()?, actor.parse().ok()?, a == "y"))
}

/// Only the member who typed the command may answer, whatever their level.
fn may_answer(presser: Option<u64>, actor: u64) -> bool {
    presser == Some(actor)
}

/// Handles the Confirm / Cancel buttons. Returns false for other interactions.
pub async fn handle_confirm(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some((nonce, actor, yes)) = parse_answer(&c.custom_id) else {
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
    let mut expired = false;
    let taken = if !may_answer(i.author_id().map(Id::get), actor) {
        Err("Only the moderator who ran this command can answer.")
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
            _ => {
                expired = true;
                Err(EXPIRED)
            }
        }
    };
    let job = match taken {
        Ok(j) => j,
        // The moderator answered too late: close the question for everyone.
        Err(_) if expired => {
            let r = InteractionResponse {
                kind: InteractionResponseType::UpdateMessage,
                data: Some(InteractionResponseData {
                    content: Some(TIMED_OUT.into()),
                    embeds: Some(vec![]),
                    components: Some(vec![]),
                    ..Default::default()
                }),
            };
            client.create_response(i.id, &i.token, &r).await?;
            return Ok(true);
        }
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
    let (text, embeds) = if yes {
        let owner = app.guild_info(job.gid).await?.owner;
        let cfg = app.cfg(job.gid.get()).await?;
        let done = job.run(app, owner, cfg.misc.warn_base_points).await?;
        job.reply(&done, cfg.misc.compact_replies, cfg.misc.timeout_secs())
    } else {
        ("Cancelled.".into(), vec![])
    };
    client
        .update_response(&i.token)
        .content(Some(&text))
        .embeds(Some(&embeds))
        .components(Some(&[]))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(true)
}

impl Pending {
    /// The reply once the command ran: result embeds, or text when compact.
    pub(crate) fn reply(
        &self,
        done: &[Applied],
        compact: bool,
        default_timeout_s: u64,
    ) -> (String, Vec<Embed>) {
        let actor = self.actor.get();
        match &self.action {
            Action::Mod {
                kind,
                reason,
                duration_s,
                flags,
                ..
            } => {
                let name = result_kind(*kind, *flags);
                let compact = compact || flags.compact;
                if compact {
                    let s = reply::summary(name, true, done, reason.as_deref(), actor);
                    return (s, vec![]);
                }
                let dur = match kind {
                    ModKind::Timeout => duration_s.or(Some(default_timeout_s as i64)),
                    _ => *duration_s,
                };
                let e = reply::result_embeds(name, done, (*flags, dur), reason.as_deref(), actor);
                (String::new(), e)
            }
            Action::Ext { cmd, reason, .. } => modcmds::reply_parts(
                cmd.result_kind(),
                compact,
                done,
                (cmd.flags(), None),
                reason.as_deref(),
                actor,
            ),
        }
    }
}

/// The result embed key: a ban with `?s` is a softban.
pub(crate) fn result_kind(kind: ModKind, flags: Flags) -> &'static str {
    match kind {
        ModKind::Ban if flags.soft => "softban",
        k => k.name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: ModKind, unsure: bool, hack: bool) -> Pending {
        let flags = Flags {
            hack,
            ..Default::default()
        };
        let action = Action::Mod {
            kind,
            targets: vec![Id::new(5)],
            reason: None,
            duration_s: None,
            flags,
            proof: None,
            unsure,
        };
        Pending::new(Id::new(1), Id::new(9), action)
    }

    #[test]
    fn asks_only_for_ids_and_names() {
        for kind in [ModKind::Ban, ModKind::Kick, ModKind::Timeout, ModKind::Warn] {
            assert!(
                !job(kind, false, false).needs_confirm(true),
                "mention {kind:?}"
            );
            assert!(
                job(kind, true, false).needs_confirm(true),
                "id or name {kind:?}"
            );
        }
        // A hackban always asks, a mention or not.
        assert!(job(ModKind::Ban, false, true).needs_confirm(true));
        // The settings switch off: nothing asks, not even a hackban by id.
        assert!(!job(ModKind::Ban, true, true).needs_confirm(false));
        assert!(!job(ModKind::Warn, true, false).needs_confirm(false));
    }

    fn ext(cmd: ModCmd, unsure: bool) -> Pending {
        let action = Action::Ext {
            cmd,
            channel: Id::new(2),
            reason: None,
            proof: None,
            users: vec![5],
            unsure,
            dm: false,
        };
        Pending::new(Id::new(1), Id::new(9), action)
    }

    #[test]
    fn quarantine_and_unban_ask_for_ids_too() {
        for cmd in [
            ModCmd::Quarantine { user: 5 },
            ModCmd::Unquarantine { user: 5 },
            ModCmd::Unban { user: 5 },
        ] {
            assert!(!ext(cmd.clone(), false).needs_confirm(true), "{cmd:?}");
            assert!(ext(cmd, true).needs_confirm(true));
        }
        assert!(!ext(ModCmd::Untimeout { user: 5 }, true).needs_confirm(true));
        assert!(ext(ModCmd::Hackban { user: 5 }, false).needs_confirm(true));
    }

    #[test]
    fn ten_second_question() {
        assert_eq!(CONFIRM_TTL, Duration::from_secs(10));
        let d = job(ModKind::Warn, true, false)
            .question()
            .description
            .unwrap();
        assert!(d.contains("Target:") && d.contains("<@5>") && d.contains("10 seconds"));
    }

    #[test]
    fn only_the_moderator_who_ran_it_answers() {
        let (nonce, actor, yes) = parse_answer("confirm:77:9:y").unwrap();
        assert_eq!((nonce, actor, yes), (77, 9, true));
        assert!(may_answer(Some(9), actor));
        // Anyone else, owner or trusted admin included, is refused before the row is touched.
        assert!(!may_answer(Some(1), actor));
        assert!(!may_answer(None, actor));
        assert_eq!(parse_answer("confirm:77:9:n").map(|a| a.2), Some(false));
        assert!(parse_answer("other:77:9:y").is_none());
    }
}
