//! Message context menus: Report (anyone) and Purge from here (staff). A report is logged with
//! action buttons; the staff who press them need the Manage Reports permit plus the permit of the action.
use anyhow::Result;
use tommybot_logging::LogKind;
use twilight_model::{
    application::interaction::{application_command::CommandData, Interaction, InteractionData},
    channel::{
        message::{
            component::{ActionRow, Button, ButtonStyle, Component},
            AllowedMentions, MessageFlags,
        },
        Message,
    },
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::App,
    commands::{self, ModKind},
    modcmds::{self, ModCmd},
};

const SNIPPET: usize = 300;

pub async fn message_menu(
    app: &App,
    data: &CommandData,
    gid: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
) -> Result<String> {
    let msg = data
        .target_id
        .zip(data.resolved.as_ref())
        .and_then(|(t, r)| r.messages.get(&t.cast()));
    let Some(msg) = msg else {
        return Ok("Message not found".into());
    };
    if data.name == "Report" {
        return report(app, gid, actor_id, msg).await;
    }
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    let cmd = ModCmd::Purge {
        count: modcmds::PURGE_MAX,
        user: None,
        from: Some(msg.id.get()),
    };
    modcmds::run(
        app,
        gid,
        owner,
        (actor_id, &actor),
        msg.channel_id,
        cmd,
        None,
    )
    .await
}

fn button(label: &str, id: i64, action: &str) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(format!("report:{id}:{action}")),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Secondary,
        url: None,
        sku_id: None,
    })
}

fn rows(id: i64, jump: String) -> Vec<Component> {
    let row = |components| {
        Component::ActionRow(ActionRow {
            id: None,
            components,
        })
    };
    let link = Component::Button(Button {
        id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some("Jump".into()),
        style: ButtonStyle::Link,
        url: Some(jump),
        sku_id: None,
    });
    vec![
        row(vec![
            link,
            button("Delete", id, "delete"),
            button("Dismiss", id, "dismiss"),
        ]),
        row(vec![
            button("Warn", id, "warn"),
            button("Timeout", id, "timeout"),
            button("Kick", id, "kick"),
            button("Ban", id, "ban"),
            button("Cases", id, "cases"),
        ]),
    ]
}

async fn report(
    app: &App,
    gid: Id<GuildMarker>,
    reporter: Id<UserMarker>,
    msg: &Message,
) -> Result<String> {
    let g = gid.get() as i64;
    let Some(log) = app
        .cfg(gid.get())
        .await?
        .logging
        .target(LogKind::Report, None)
    else {
        return Ok("Reports are not set up on this server".into());
    };
    if msg.author.bot || msg.author.id == reporter {
        return Ok("You cannot report this message".into());
    }
    let banned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM report_blacklist WHERE guild_id=$1 AND user_id=$2)",
    )
    .bind(g)
    .bind(reporter.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    if banned {
        return Ok("You can no longer report messages in this server".into());
    }
    if app.whitelisted(gid, msg.author.id, "reports").await? {
        return Ok("This member cannot be reported".into());
    }
    let content: String = msg.content.chars().take(SNIPPET).collect();
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO reports (guild_id, reporter_id, target_id, channel_id, message_id, content)
         VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(g)
    .bind(reporter.get() as i64)
    .bind(msg.author.id.get() as i64)
    .bind(msg.channel_id.get() as i64)
    .bind(msg.id.get() as i64)
    .bind(&content)
    .fetch_optional(&app.pool)
    .await?;
    let Some(id) = id else {
        return Ok("You already reported this message".into());
    };
    let quote = if content.is_empty() {
        "> (no text)".to_string()
    } else {
        content
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = format!(
        "**Report #{id}** by <@{reporter}> against <@{}> in <#{}>\n{quote}",
        msg.author.id, msg.channel_id
    );
    let jump = format!(
        "https://discord.com/channels/{gid}/{}/{}",
        msg.channel_id, msg.id
    );
    let sent = app
        .http
        .create_message(log)
        .content(&text)
        .components(&rows(id, jump))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await;
    if let Err(e) = sent {
        sqlx::query("DELETE FROM reports WHERE id=$1")
            .bind(id)
            .execute(&app.pool)
            .await?;
        return Err(e.into());
    }
    Ok(format!("Report #{id} sent to the staff, thank you"))
}

/// The pressed button's effect: the private answer, and for a success the verb for the history line.
struct Outcome(String, Option<&'static str>);

async fn act(
    app: &App,
    gid: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
    id: i64,
    action: &str,
) -> Result<Outcome> {
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    if !actor.has("reports") {
        return Ok(Outcome("You need the Manage Reports permit".into(), None));
    }
    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT target_id, channel_id, message_id FROM reports WHERE id=$1 AND guild_id=$2",
    )
    .bind(id)
    .bind(gid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((target, channel, message)) = row else {
        return Ok(Outcome("Report not found".into(), None));
    };
    let target = Id::new(target as u64);
    let who = (actor_id, &actor);
    let reason = format!("Report #{id}");
    let kind = match action {
        "dismiss" => {
            return Ok(Outcome(
                "Report dismissed".into(),
                Some("dismissed the report"),
            ))
        }
        "delete" => {
            let gone = app
                .http
                .delete_message(Id::new(channel as u64), Id::new(message as u64))
                .await;
            return Ok(match gone {
                Ok(_) => Outcome("Message deleted".into(), Some("deleted the message")),
                Err(_) => Outcome("Could not delete the message, it may be gone".into(), None),
            });
        }
        "cases" => {
            let cmd = ModCmd::CasesView {
                case: None,
                user: Some(target.get()),
            };
            let text =
                modcmds::run(app, gid, owner, who, Id::new(channel as u64), cmd, None).await?;
            return Ok(Outcome(text, None));
        }
        "warn" => (ModKind::Warn, "warned the user"),
        "timeout" => (ModKind::Timeout, "timed out the user"),
        "kick" => (ModKind::Kick, "kicked the user"),
        "ban" => (ModKind::Ban, "banned the user"),
        _ => return Ok(Outcome("Unknown action".into(), None)),
    };
    let points = app.cfg(gid.get()).await?.misc.warn_base_points;
    let text = commands::apply(
        app,
        gid,
        owner,
        who,
        kind.0,
        target,
        Some(&reason),
        None,
        points,
    )
    .await?;
    // A refusal never opens a case, so "Case #" tells the two apart.
    let verb = text.starts_with("Case #").then_some(kind.1);
    Ok(Outcome(text, verb))
}

/// Handles the buttons of a report log message; `false` when the interaction is not one of them.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let parsed = c
        .custom_id
        .strip_prefix("report:")
        .and_then(|r| r.split_once(':'));
    let Some((id, action)) = parsed.and_then(|(id, a)| Some((id.parse::<i64>().ok()?, a))) else {
        return Ok(false);
    };
    let (Some(gid), Some(actor_id)) = (i.guild_id, i.author_id()) else {
        return Ok(true);
    };
    let client = app.http.interaction(Id::new(app.me.get()));
    let defer = InteractionResponse {
        kind: InteractionResponseType::DeferredUpdateMessage,
        data: None,
    };
    client.create_response(i.id, &i.token, &defer).await?;
    let Outcome(text, verb) = act(app, gid, actor_id, id, action)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(?e, action, "report action failed");
            Outcome("Something went wrong, see logs".into(), None)
        });
    if let Some(verb) = verb {
        let status = if action == "dismiss" {
            "dismissed"
        } else {
            "actioned"
        };
        sqlx::query(
            "UPDATE reports SET status=$2, action=$3, handled_by=$4, handled_at=now() WHERE id=$1",
        )
        .bind(id)
        .bind(status)
        .bind(action)
        .bind(actor_id.get() as i64)
        .execute(&app.pool)
        .await?;
        let old = i.message.as_ref().map_or("", |m| m.content.as_str());
        let new = format!("{old}\n- <@{actor_id}> {verb}");
        // Discord caps content at 2000; a report with a very long history just stops growing.
        if new.chars().count() <= 2000 {
            let u = client.update_response(&i.token).content(Some(&new));
            if action == "dismiss" {
                u.components(Some(&[])).await?;
            } else {
                u.await?;
            }
        }
    }
    client
        .create_followup(&i.token)
        .content(&text)
        .flags(MessageFlags::EPHEMERAL)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(true)
}
