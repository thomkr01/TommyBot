//! Thank-you message posted once when the bot joins a server.
use anyhow::Result;
use twilight_model::{
    channel::{
        message::component::{ActionRow, Button, ButtonStyle, Component},
        ChannelType,
    },
    gateway::payload::incoming::GuildCreate,
    guild::audit_log::AuditLogEventType,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::embed::EmbedBuilder;

use crate::app::App;

/// Only a join in the last two minutes counts, so the GuildCreate burst at startup stays quiet.
const FRESH_SECS: i64 = 120;

/// The dashboard routes by URL hash, so a path like `/guilds/<id>` would 404.
fn wizard_url(base: &str, guild: impl std::fmt::Display) -> String {
    format!("{}/#/{guild}/wizard", base.trim_end_matches('/'))
}

fn link(label: &str, url: String) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Link,
        url: Some(url),
        sku_id: None,
    })
}

pub async fn on_guild_create(app: &App, ev: &GuildCreate) -> Result<()> {
    let GuildCreate::Available(g) = ev else {
        return Ok(());
    };
    let fresh = g
        .joined_at
        .is_some_and(|t| t.as_secs() > crate::app::now_secs() - FRESH_SECS);
    if !fresh {
        return Ok(());
    }
    let (new,): (bool,) = sqlx::query_as(
        "WITH i AS (INSERT INTO welcomed_guilds (guild_id) VALUES ($1)
                    ON CONFLICT DO NOTHING RETURNING 1)
         SELECT EXISTS (SELECT 1 FROM i)",
    )
    .bind(g.id.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    let channel = g.system_channel_id.or_else(|| {
        g.channels
            .iter()
            .find(|c| c.kind == ChannelType::GuildText)
            .map(|c| c.id)
    });
    if !new {
        return Ok(());
    }

    let dashboard = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let mut links = vec![];
    if let Some(url) = &dashboard {
        links.push(link("Setup Wizard", wizard_url(url, g.id)));
    }
    if let Some(url) = std::env::var("SUPPORT_URL").ok().filter(|u| !u.is_empty()) {
        links.push(link("Support", url));
    }
    let wizard = match &dashboard {
        Some(url) => format!(
            "You can set the bot up in the [dashboard]({}).",
            wizard_url(url, g.id)
        ),
        None => "Ask the bot owner for the dashboard link to set it up.".into(),
    };
    let embed = EmbedBuilder::new()
        .title("Thank you for choosing TommyBot!")
        .description(format!(
            "✅ `TommyBot` has been added to `{}` successfully!\n\n{wizard} Every feature is free.",
            g.name
        ))
        .build();
    let embeds = [embed];
    // Discord rejects an empty action row.
    let rows: Vec<Component> = if links.is_empty() {
        vec![]
    } else {
        vec![Component::ActionRow(ActionRow {
            id: None,
            components: links,
        })]
    };
    // Like Wick: tell whoever added the bot, and fall back to the server when their DMs are closed.
    if let Some(adder) = bot_adder(app, g.id, app.me).await {
        let dm = async {
            let ch = app
                .http
                .create_private_channel(adder)
                .await?
                .model()
                .await?;
            app.http
                .create_message(ch.id)
                .embeds(&embeds)
                .components(&rows)
                .await?;
            anyhow::Ok(())
        };
        if dm.await.is_ok() {
            return Ok(());
        }
    }
    let Some(channel) = channel else {
        return Ok(());
    };
    app.http
        .create_message(channel)
        .embeds(&embeds)
        .components(&rows)
        .await?;
    Ok(())
}

/// Who added `bot`, from the audit log; None when there is no entry or the lookup fails.
pub async fn bot_adder(
    app: &App,
    gid: Id<GuildMarker>,
    bot: Id<UserMarker>,
) -> Option<Id<UserMarker>> {
    let log = app
        .http
        .audit_log(gid)
        .action_type(AuditLogEventType::BotAdd)
        .limit(10)
        .await
        .ok()?
        .model()
        .await
        .ok()?;
    log.entries
        .iter()
        .find(|e| e.target_id.is_some_and(|t| t.get() == bot.get()))
        .and_then(|e| e.user_id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn wizard_link_uses_hash_route() {
        assert_eq!(
            super::wizard_url("https://d.example/", 5),
            "https://d.example/#/5/wizard"
        );
    }
}
