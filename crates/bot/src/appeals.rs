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
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use crate::app::App;

pub fn link(base: &str, guild: Id<GuildMarker>) -> String {
    format!("{}/#/appeal/{guild}", base.trim_end_matches('/'))
}

/// Line appended to a ban or timeout DM, so the member gets one message and not two.
pub fn note(cfg: &AppealsCfg, message: &str, guild: Id<GuildMarker>) -> Option<String> {
    let base = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| cfg.enabled && !u.is_empty())?;
    let message = if message.is_empty() {
        "You can appeal this on the dashboard:"
    } else {
        message
    };
    Some(format!("{message} {}", link(&base, guild)))
}

const MODAL: &str = "appealm:";

/// Applies a decision from the buttons on an appeal log or the dashboard (job `appeal_accept`).
/// `Err` is the refusal text.
async fn decide(
    app: &App,
    (gid, actor_id): (Id<GuildMarker>, Id<UserMarker>),
    id: i64,
    act: &str,
    reply: &str,
) -> Result<std::result::Result<(String, u32), String>> {
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    type Row = (i64, String, String, Option<i64>, Option<String>, bool);
    let row: Option<Row> = sqlx::query_as(
        "SELECT user_id, kind, status, decided_by, reply, verdict_sent_at IS NOT NULL FROM appeals
         WHERE id=$1 AND guild_id=$2",
    )
    .bind(id)
    .bind(gid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((user, kind, was, by, stored_reply, sent)) = row else {
        return Ok(Err("This appeal was already handled".into()));
    };
    // A retry of this same accept after its writes committed (R-h3 4): only the DM and the log
    // answer are left to do.
    let resumed = act == "accept" && was == "accepted" && by == Some(actor_id.get() as i64);
    if was != "open" && !resumed {
        return Ok(Err("This appeal was already handled".into()));
    }
    if !actor.has("appeals") || (act == "accept" && !actor.has(&kind)) {
        return Ok(Err(
            "You need the Review Appeals permit (and the ban or timeout permit to approve)".into(),
        ));
    }
    let action = if act == "accept" {
        kind.as_str()
    } else {
        "appeals"
    };
    if let Some(why) = crate::commands::gated(app, gid, actor_id, &actor, action).await? {
        return Ok(Err(why));
    }
    let user_id = Id::new(user as u64);
    // What the lift can be undone with, as in the dashboard's action log.
    let mut inverse = None;
    if act == "accept" && !resumed {
        // The real end, so a revert gives the same punishment back (D23b): a temp ban's pending
        // unban, a long timeout's renewal target (Discord shows at most 28 days).
        let end = |sql: &'static str| {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(gid.get() as i64)
                .bind(user.to_string())
                .fetch_optional(&app.pool)
        };
        if kind == "ban" {
            let until = end(
                "SELECT EXTRACT(EPOCH FROM run_at)::BIGINT FROM scheduled_jobs
                WHERE guild_id = $1 AND kind = 'unban' AND payload->>'user' = $2
                ORDER BY run_at DESC LIMIT 1",
            )
            .await?;
            inverse = Some(serde_json::json!({"op": "ban", "user": user, "until": until}));
        } else {
            let mut until = end("SELECT (payload->>'until_secs')::BIGINT FROM scheduled_jobs
                WHERE guild_id = $1 AND kind = 'timeout_renew' AND payload->>'user' = $2
                ORDER BY run_at DESC LIMIT 1")
            .await?;
            if until.is_none() {
                until = match app.http.guild_member(gid, user_id).await {
                    Ok(m) => m
                        .model()
                        .await?
                        .communication_disabled_until
                        .map(|t| t.as_secs()),
                    Err(_) => None,
                };
            }
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
        // A 404 means it was already lifted by hand; an outage is retried (job) or reported.
        if let Err(e) = lifted.map_err(anyhow::Error::new) {
            if crate::http_err::refused(&e) && !crate::http_err::gone_any(&e) {
                tracing::warn!(?e, "lifting the punishment was refused");
                let why = crate::http_err::why(&e);
                return Ok(Err(format!("Could not lift the punishment: {why}")));
            }
            if !crate::http_err::gone_any(&e) {
                return Err(e);
            }
        }
    }
    let status = match act {
        "accept" => "accepted",
        "dismiss" => "dismissed",
        _ => "denied",
    };
    let reply = match resumed {
        true => stored_reply.unwrap_or_default(),
        false => reply.trim().chars().take(1000).collect::<String>(),
    };
    if !resumed {
        // One transaction: a crash between them would drop the jobs that hold the real end, so a
        // retry would record a permanent ban as the undo (R-h3 4).
        let mut tx = app.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE appeals SET status=$2, decided_by=$3, reply=nullif($4,'') WHERE id=$1 AND status='open'",
        )
        .bind(id)
        .bind(status)
        .bind(actor_id.get() as i64)
        .bind(&reply)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if done == 0 {
            return Ok(Err("This appeal was already handled".into()));
        }
        if act == "accept" {
            // A ban's `lifted` below drops a pending raid punishment retry; a timeout has none.
            let jobs: &[&str] = if kind == "ban" {
                &["unban"]
            } else {
                &["timeout_renew", "raid_punish"]
            };
            tommybot_core::jobs::cancel_user(&mut *tx, gid.get() as i64, jobs, user as u64, None)
                .await?;
            sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,$4,$5)")
                .bind(gid.get() as i64)
                .bind(actor_id.get() as i64)
                .bind(if kind == "ban" { "unban" } else { "untimeout" })
                .bind(user)
                .bind(inverse)
                .execute(&mut *tx)
                .await?;
        }
        // In the transaction too, so a crash after the commit never loses the Raider List allow
        // of an accepted ban appeal (R-h3 round 2): a listed user is let back in for good.
        if act == "accept" && kind == "ban" {
            tommybot_core::raiders::allow(&mut *tx, gid.get() as i64, user, actor_id.get() as i64)
                .await?;
        }
        if act == "blacklist" {
            sqlx::query("INSERT INTO appeal_blacklist (guild_id, user_id, added_by) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(gid.get() as i64)
                .bind(user)
                .bind(actor_id.get() as i64)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    if act == "accept" && !resumed {
        // The appeal is already decided; a lost stats row must not report it as failed.
        let stat = async {
            tommybot_core::cases::record(
                &mut *app.pool.acquire().await?,
                tommybot_core::cases::ModAction {
                    guild_id: gid.get() as i64,
                    mod_id: actor_id.get() as i64,
                    kind: if kind == "ban" { "unban" } else { "untimeout" },
                    source: "appeal",
                    target_id: Some(user),
                    case_no: None,
                    audit_id: None,
                },
            )
            .await
        }
        .await;
        if let Err(e) = stat {
            tracing::warn!(?e, "recording the appeal for mod stats failed");
        }
    }
    if act != "dismiss" && !sent {
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
        match sent.await {
            Ok(()) => {
                sqlx::query("UPDATE appeals SET verdict_sent_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(&app.pool)
                    .await?;
            }
            Err(e) => tracing::warn!(?e, "appeal verdict DM failed"),
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
    let (Some(gid), Some(actor_id)) = (i.guild_id, i.author_id()) else {
        return Ok(false);
    };
    let kind = match decide(app, (gid, actor_id), id, act, &reply).await {
        Ok(Ok((line, color))) => {
            if let Some(mut e) = i.message.as_ref().and_then(|m| m.embeds.first().cloned()) {
                e.color = Some(color);
                // Over 1024 or a 26th field, Discord refuses the update and the buttons stay.
                if e.fields.len() < 25 {
                    e.fields.push(EmbedField {
                        inline: false,
                        name: "Decision".into(),
                        value: line.chars().take(1024).collect(),
                    });
                }
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
                    format!("Something went wrong: {}", crate::http_err::why(&e))
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

/// Job `appeal_accept`: a dashboard accept runs here, so it goes through the same permit, panic
/// lock and command rate limit as the log button (D24). A refusal is posted in the appeal log.
pub async fn accept_job(app: &App, gid: Id<GuildMarker>, p: &serde_json::Value) -> Result<()> {
    use anyhow::Context;
    let id = p["id"].as_i64().context("job payload has no id")?;
    let by = p["by"]
        .as_u64()
        .filter(|b| *b > 0)
        .context("job payload has no by")?;
    let reply = p["reply"].as_str().unwrap_or_default();
    match decide(app, (gid, Id::new(by)), id, "accept", reply).await? {
        Ok((line, color)) => close_log(app, id, &line, color).await,
        Err(why) => {
            let log = tommybot_logging::Log::new("Appeal Not Accepted", RED)
                .line("Appeal", &format!("#{id}"))
                .line("Staff", &format!("<@{by}>"))
                .text(&why);
            crate::events::send_log(app, gid.get(), tommybot_logging::LogKind::Appeal, log).await?;
        }
    }
    Ok(())
}

/// What the log button's answer does for a decision made elsewhere: Decision field, colour, no buttons.
async fn close_log(app: &App, id: i64, line: &str, color: u32) {
    let row: Option<(Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT log_channel, log_message FROM appeals WHERE id = $1")
            .bind(id)
            .fetch_optional(&app.pool)
            .await
            .ok()
            .flatten();
    let Some((Some(ch), Some(msg))) = row else {
        return;
    };
    let (ch, msg) = (Id::new(ch as u64), Id::new(msg as u64));
    let closed = async {
        let m = app.http.message(ch, msg).await?.model().await?;
        let mut embeds = m.embeds;
        if let Some(e) = embeds.first_mut() {
            e.color = Some(color);
            if e.fields.len() < 25 {
                e.fields.push(EmbedField {
                    inline: false,
                    name: "Decision".into(),
                    value: line.chars().take(1024).collect(),
                });
            }
        }
        embeds.truncate(1);
        app.http
            .update_message(ch, msg)
            .embeds(Some(&embeds))
            .components(Some(&[]))
            .await?;
        anyhow::Ok(())
    };
    if let Err(e) = closed.await {
        tracing::warn!(?e, "closing the appeal log failed");
    }
}

#[cfg(test)]
mod tests {
    /// D24/D23b: a dashboard accept runs through the bot's checks, and a revert gives the temp ban
    /// back with its old end.
    #[tokio::test]
    async fn a_queued_accept_is_checked_and_a_revert_keeps_the_temp_ban_temp() {
        use crate::test_support::T;
        use serde_json::json;
        let Some(t) = T::new(201).await else { return };
        let (g, user) = (t.g(0), t.base + 5);
        t.server_config(0, t.owner(), json!({})).await;
        t.exec("DELETE FROM appeals WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.ban(0, user, "spam");
        let end = crate::app::now_secs() + 3_600;
        let job = json!({ "user": user });
        tommybot_core::jobs::enqueue(&t.app.pool, g.get() as i64, "unban", job, end)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO appeals (guild_id, user_id, username, kind, answers) VALUES ($1, $2, 'u', 'ban', '[]') RETURNING id",
        )
        .bind(g.get() as i64)
        .bind(user as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
        let status = || {
            sqlx::query_scalar::<_, String>("SELECT status FROM appeals WHERE id = $1")
                .bind(id)
                .fetch_one(&t.app.pool)
        };
        // No permit: refused, nothing lifted.
        let p = json!({"id": id, "by": t.base + 6, "reply": ""});
        super::accept_job(&t.app, g, &p).await.unwrap();
        assert_eq!(status().await.unwrap(), "open");
        assert!(t.banned(0, user).is_some());
        // The Raider List had banned them: the accept lets them back in, in the same transaction.
        sqlx::query(
            "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, 'ban')",
        )
        .bind(g.get() as i64)
        .bind(user as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
        // The owner: lifted, and the undo remembers the temp ban's end.
        let p = json!({"id": id, "by": t.owner(), "reply": ""});
        super::accept_job(&t.app, g, &p).await.unwrap();
        assert_eq!(status().await.unwrap(), "accepted");
        assert!(t.banned(0, user).is_none());
        let allowed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM raider_allow WHERE guild_id = $1 AND user_id = $2",
        )
        .bind(g.get() as i64)
        .bind(user as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
        assert_eq!(
            allowed, 1,
            "the Raider List allow is saved with the decision"
        );
        // R-h3 4: a retry after the writes committed but before the DM sends the DM, once, and
        // writes nothing twice.
        let dms = || t.calls("POST /users/@me/channels");
        let before = dms();
        sqlx::query("UPDATE appeals SET verdict_sent_at = NULL WHERE id = $1")
            .bind(id)
            .execute(&t.app.pool)
            .await
            .unwrap();
        super::accept_job(&t.app, g, &p).await.unwrap();
        super::accept_job(&t.app, g, &p).await.unwrap();
        assert_eq!(dms(), before + 1, "the missed DM is sent once");
        let logs: i64 = sqlx::query_scalar("SELECT count(*) FROM action_log WHERE guild_id = $1")
            .bind(g.get() as i64)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
        assert_eq!(logs, 1);
        let inv: serde_json::Value = sqlx::query_scalar(
            "SELECT inverse FROM action_log WHERE guild_id = $1 AND action = 'unban'",
        )
        .bind(g.get() as i64)
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
        assert_eq!(inv["until"], json!(end));
        assert!(
            t.take("unban").await.is_empty(),
            "the lift drops the old unban"
        );
        let inv = serde_json::from_value(inv).unwrap();
        crate::antinuke::run_inverse(&t.app, g, &inv).await.unwrap();
        assert!(t.banned(0, user).is_some());
        let unbans = t.take("unban").await;
        assert_eq!(unbans.len(), 1);
        assert!((unbans[0].2 - 3_600).abs() < 30, "{unbans:?}");
        // R-h3 3: a revert whose ban fails leaves no unban job that could lift a later ban.
        t.set(|m| {
            m.bans.clear();
            m.fail_put_5xx = true;
        });
        assert!(crate::antinuke::run_inverse(&t.app, g, &inv).await.is_err());
        assert!(
            t.take("unban").await.is_empty(),
            "the failed revert's job is dropped"
        );
        t.set(|m| m.fail_put_5xx = false);
        t.exec("DELETE FROM appeals WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
    }

    #[test]
    fn link_has_no_double_slash() {
        let g = twilight_model::id::Id::new(5);
        assert_eq!(super::link("https://x.io/", g), "https://x.io/#/appeal/5");
    }
}
