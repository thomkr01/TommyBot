//! Thank-you message posted once when the bot joins a server.
use anyhow::Result;
use twilight_model::{
    channel::{
        message::component::{ActionRow, Button, ButtonStyle, Component},
        ChannelType,
    },
    gateway::payload::incoming::GuildCreate,
};
use twilight_util::builder::embed::EmbedBuilder;

use crate::app::App;

/// Only a join in the last two minutes counts, so the GuildCreate burst at startup stays quiet.
const FRESH_SECS: i64 = 120;

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
    let (true, Some(channel)) = (new, channel) else {
        return Ok(());
    };

    let dashboard = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let mut links = vec![];
    if let Some(url) = &dashboard {
        links.push(link("Setup Wizard", format!("{url}/guilds/{}", g.id)));
    }
    if let Some(url) = std::env::var("SUPPORT_URL").ok().filter(|u| !u.is_empty()) {
        links.push(link("Support", url));
    }
    let wizard = match &dashboard {
        Some(url) => format!(
            "You can set the bot up in the [dashboard]({url}/guilds/{}).",
            g.id
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
    app.http
        .create_message(channel)
        .embeds(&embeds)
        .components(&rows)
        .await?;
    Ok(())
}
