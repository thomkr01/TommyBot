//! Confirm / Cancel prompts: a mod command parked in `pending_confirms` until its buttons are clicked.
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
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
};

use crate::{
    app::App,
    commands::{apply_dm, ModKind},
    modcmds::{self, ModCmd},
    prefix::Flags,
};

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
    },
    Ext {
        cmd: ModCmd,
        channel: Id<ChannelMarker>,
        reason: Option<String>,
        #[serde(default)]
        proof: Option<String>,
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

    /// `confirm_on` is the server's `misc.confirm`. Warns and non-destructive commands run straight away.
    pub fn needs_confirm(&self, confirm_on: bool) -> bool {
        match &self.action {
            Action::Mod { kind, targets, .. } => {
                confirm_on && *kind != ModKind::Warn && !targets.is_empty()
            }
            Action::Ext { cmd, .. } => cmd.asks_confirm(confirm_on),
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
    pub(crate) async fn run(
        &self,
        app: &App,
        owner: Id<UserMarker>,
        points: i32,
    ) -> Result<Vec<String>> {
        let (kind, targets, reason, duration_s, flags, proof) = match &self.action {
            Action::Mod {
                kind,
                targets,
                reason,
                duration_s,
                flags,
                proof,
            } => (*kind, targets, reason, *duration_s, *flags, proof),
            Action::Ext {
                cmd,
                channel,
                reason,
                proof,
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
                    proof.as_deref(),
                )
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(?e, "mod command failed");
                    format!("Command failed: {}", crate::http_err::why(&e))
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
                proof.as_deref(),
            )
            .await
            {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(?e, "mod command failed");
                    format!(
                        "Failed to {} <@{t}>: {}",
                        kind.name(),
                        crate::http_err::why(&e)
                    )
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
