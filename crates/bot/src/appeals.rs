//! The dashboard appeal link that goes into the DM of a member about to be banned or timed out.
use anyhow::Result;
use tommybot_core::sections::AppealsCfg;
use tommybot_logging::embed::{HANDLED, RED};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Component, TextInput, TextInputStyle},
        embed::EmbedField,
        AllowedMentions, MessageFlags,
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::GuildMarker, Id},
};

use crate::app::App;

pub fn link(base: &str, guild: Id<GuildMarker>) -> String {
    format!("{}/#/appeal/{guild}", base.trim_end_matches('/'))
}

/// Line appended to a ban or timeout DM, so the member gets one message and not two.
pub fn note(cfg: &AppealsCfg, guild: Id<GuildMarker>) -> Option<String> {
    let base = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| cfg.enabled && !u.is_empty())?;
    Some(format!(
        "You can appeal this on the dashboard: {}",
        link(&base, guild)
    ))
}

const MODAL: &str = "appealm:";

/// Applies a decision from the buttons on an appeal log. `Err` is the private refusal text.
async fn decide(
    app: &App,
    i: &Interaction,
    id: i64,
    act: &str,
    reply: &str,
) -> Result<std::result::Result<(String, u32), String>> {
    let (Some(gid), Some(actor_id)) = (i.guild_id, i.author_id()) else {
        return Ok(Err("Server only".into()));
    };
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT user_id, kind FROM appeals WHERE id=$1 AND guild_id=$2 AND status='open'",
    )
    .bind(id)
    .bind(gid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((user, kind)) = row else {
        return Ok(Err("This appeal was already handled".into()));
    };
    if !actor.has("appeals") || (act == "accept" && !actor.has(&kind)) {
        return Ok(Err(
            "You need the Review Appeals permit (and the ban or timeout permit to approve)".into(),
        ));
    }
    let user_id = Id::new(user as u64);
    // What the lift can be undone with, as in the dashboard's action log.
    let mut inverse = Some(serde_json::json!({"op": "ban", "user": user}));
    if act == "accept" {
        if kind == "timeout" {
            let until = app.http.guild_member(gid, user_id).await;
            let until = match until {
                Ok(m) => m
                    .model()
                    .await?
                    .communication_disabled_until
                    .map(|t| t.as_secs()),
                Err(_) => None,
            };
            inverse = until
                .map(|until| serde_json::json!({"op": "timeout", "user": user, "until": until}));
        }
        let lifted = if kind == "ban" {
            app.http.delete_ban(gid, user_id).await.map(drop)
        } else {
            app.http
                .update_guild_member(gid, user_id)
                .communication_disabled_until(None)
                .await
                .map(drop)
        };
        if let Err(e) = lifted {
            tracing::warn!(?e, "lifting the punishment failed");
            return Ok(Err(
                "Could not lift the punishment, it may already be lifted".into(),
            ));
        }
    }
    let status = match act {
        "accept" => "accepted",
        "dismiss" => "dismissed",
        _ => "denied",
    };
    let reply = reply.trim().chars().take(1000).collect::<String>();
    let done = sqlx::query(
        "UPDATE appeals SET status=$2, decided_by=$3, reply=nullif($4,'') WHERE id=$1 AND status='open'",
    )
    .bind(id)
    .bind(status)
    .bind(actor_id.get() as i64)
    .bind(&reply)
    .execute(&app.pool)
    .await?
    .rows_affected();
    if done == 0 {
        return Ok(Err("This appeal was already handled".into()));
    }
    if act == "accept" {
        sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,$4,$5)")
            .bind(gid.get() as i64)
            .bind(actor_id.get() as i64)
            .bind(if kind == "ban" { "unban" } else { "untimeout" })
            .bind(user)
            .bind(inverse)
            .execute(&app.pool)
            .await?;
    }
    if act == "blacklist" {
        sqlx::query("INSERT INTO appeal_blacklist (guild_id, user_id, added_by) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(gid.get() as i64)
            .bind(user)
            .bind(actor_id.get() as i64)
            .execute(&app.pool)
            .await?;
    }
    if act != "dismiss" {
        let verdict = if act == "accept" {
            "approved"
        } else {
            "denied"
        };
        let mut text = format!("Your {kind} appeal (#{id}) was {verdict}.");
        if !reply.is_empty() {
            text.push_str(&format!("\nReply from the staff: {reply}"));
        }
        let sent = async {
            let ch = app
                .http
                .create_private_channel(user_id)
                .await?
                .model()
                .await?;
            app.http.create_message(ch.id).content(&text).await?;
            anyhow::Ok(())
        };
        if let Err(e) = sent.await {
            tracing::warn!(?e, "appeal verdict DM failed");
        }
    }
    let (word, color) = match act {
        "accept" => ("Approved", HANDLED),
        "dismiss" => ("Dismissed", RED),
        "blacklist" => ("Denied and blacklisted", RED),
        _ => ("Denied", RED),
    };
    let mut line = format!("{word} by <@{actor_id}>");
    if !reply.is_empty() {
        line.push_str(&format!("\n{reply}"));
    }
    Ok(Ok((line, color)))
}

/// Handles the appeal log buttons and the reply modal; `false` when the interaction is not one of them.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let client = app.http.interaction(Id::new(app.me.get()));
    let (custom_id, reply) = match &i.data {
        Some(InteractionData::MessageComponent(c)) => (c.custom_id.as_str(), String::new()),
        Some(InteractionData::ModalSubmit(m)) => (
            m.custom_id.as_str(),
            m.components
                .iter()
                .find_map(crate::verify::typed_text)
                .unwrap_or_default(),
        ),
        _ => return Ok(false),
    };
    let modal = custom_id.starts_with(MODAL);
    let parsed = custom_id
        .strip_prefix(MODAL)
        .or_else(|| custom_id.strip_prefix("appeal:"))
        .and_then(|r| r.split_once(':'))
        .and_then(|(id, a)| Some((id.parse::<i64>().ok()?, a)));
    let Some((id, act)) = parsed else {
        return Ok(false);
    };
    if !modal && act != "dismiss" {
        let input = Component::TextInput(TextInput {
            id: None,
            custom_id: "reply".into(),
            #[allow(deprecated)]
            label: Some("Reply (optional, sent to the member)".into()),
            max_length: Some(1000),
            min_length: None,
            placeholder: None,
            required: Some(false),
            style: TextInputStyle::Paragraph,
            value: None,
        });
        let resp = InteractionResponse {
            kind: InteractionResponseType::Modal,
            data: Some(InteractionResponseData {
                custom_id: Some(format!("{MODAL}{id}:{act}")),
                title: Some("Appeal reply".into()),
                components: Some(vec![Component::ActionRow(ActionRow {
                    id: None,
                    components: vec![input],
                })]),
                ..Default::default()
            }),
        };
        client.create_response(i.id, &i.token, &resp).await?;
        return Ok(true);
    }
    let mut data = InteractionResponseData {
        allowed_mentions: Some(AllowedMentions::default()),
        ..Default::default()
    };
    let kind = match decide(app, i, id, act, &reply).await {
        Ok(Ok((line, color))) => {
            if let Some(mut e) = i.message.as_ref().and_then(|m| m.embeds.first().cloned()) {
                e.color = Some(color);
                e.fields.push(EmbedField {
                    inline: false,
                    name: "Decision".into(),
                    value: line,
                });
                data.embeds = Some(vec![e]);
            }
            data.components = Some(vec![]);
            InteractionResponseType::UpdateMessage
        }
        other => {
            data.content = Some(match other {
                Ok(Err(m)) => m,
                Err(e) => {
                    tracing::warn!(?e, "appeal decision failed");
                    format!("Something went wrong: {}", crate::commands::why(&e))
                }
                Ok(Ok(_)) => unreachable!(),
            });
            data.flags = Some(MessageFlags::EPHEMERAL);
            InteractionResponseType::ChannelMessageWithSource
        }
    };
    client
        .create_response(
            i.id,
            &i.token,
            &InteractionResponse {
                kind,
                data: Some(data),
            },
        )
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_has_no_double_slash() {
        let g = twilight_model::id::Id::new(5);
        assert_eq!(super::link("https://x.io/", g), "https://x.io/#/appeal/5");
    }
}
