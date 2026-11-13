//! Thank-you message posted once when the bot joins a server, and data deletion after it leaves.
use anyhow::Result;
use tommybot_logging::emoji::YES;
use twilight_model::{
    channel::{
        message::component::{ActionRow, Button, ButtonStyle, Component},
        ChannelType,
    },
    gateway::payload::incoming::{GuildCreate, GuildDelete},
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

pub(crate) fn link(label: &str, url: String) -> Component {
    Component::Button(tommybot_logging::emoji::LINK.on(Button {
        id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Link,
        url: Some(url),
        sku_id: None,
    }))
}

/// Days a removed server's data is kept, in case the bot is added back (PRIVACY.md).
const KEEP_DAYS: i32 = 30;

/// Starts the 30-day countdown when the bot is removed; an outage (`unavailable`) is not a removal.
pub async fn on_guild_delete(app: &App, ev: &GuildDelete) -> Result<()> {
    if ev.unavailable != Some(true) {
        sqlx::query("INSERT INTO guild_left (guild_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(ev.id.get() as i64)
            .execute(&app.pool)
            .await?;
        crate::bansync::on_removed(app, ev.id).await?;
    }
    Ok(())
}

/// Starts the countdown for servers that removed the bot while it was offline. One shard, so
/// `Ready` lists every server the bot is in.
pub async fn on_ready(
    app: &App,
    ev: &twilight_model::gateway::payload::incoming::Ready,
) -> Result<()> {
    tracing::info!(guilds = ev.guilds.len(), "gateway ready");
    let ids: Vec<i64> = ev.guilds.iter().map(|g| g.id.get() as i64).collect();
    sqlx::query(
        "INSERT INTO guild_left (guild_id) SELECT DISTINCT guild_id FROM guild_config \
         WHERE guild_id <> ALL($1) ON CONFLICT DO NOTHING",
    )
    .bind(&ids)
    .execute(&app.pool)
    .await?;
    // Ban Sync groups only servers the bot is still in.
    sqlx::query("DELETE FROM guild_owners WHERE guild_id <> ALL($1)")
        .bind(&ids)
        .execute(&app.pool)
        .await?;
    Ok(())
}

/// Deletes the data of servers the bot left more than `KEEP_DAYS` ago, hourly.
pub async fn purge_left(app: std::sync::Arc<App>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
    let mut sticky_synced = std::collections::HashMap::new();
    loop {
        tick.tick().await;
        if let Err(e) = crate::sticky::sweep(&app, &mut sticky_synced).await {
            tracing::warn!(?e, "sticky roles sync and cleanup failed");
        }
        match tommybot_core::db::purge_left(&app.pool, KEEP_DAYS).await {
            Ok(ids) if !ids.is_empty() => tracing::info!(?ids, "purged data of removed servers"),
            Ok(_) => {}
            Err(e) => tracing::warn!(?e, "purging removed servers failed"),
        }
        // The insert trigger only prunes servers whose Join Gate still acts.
        let old = sqlx::query(
            "DELETE FROM joingate_actions WHERE created_at < now() - interval '30 days'",
        )
        .execute(&app.pool)
        .await;
        if let Err(e) = old {
            tracing::warn!(?e, "pruning old join gate records failed");
        }
        // Moderator stats keep one year (Thomas 2026-11-11).
        let old =
            sqlx::query("DELETE FROM mod_actions WHERE created_at < now() - interval '365 days'")
                .execute(&app.pool)
                .await;
        if let Err(e) = old {
            tracing::warn!(?e, "pruning old mod stats failed");
        }
        crate::raiders::sweep(&app).await;
    }
}

/// Members already in voice, so their next move is not logged or counted as a join. Called first
/// in the event's task, before any await, so voice updates handled after it are not wiped.
pub fn seed_voice(app: &App, ev: &GuildCreate) {
    if let GuildCreate::Available(g) = ev {
        let in_voice = g.voice_states.iter();
        let in_voice = in_voice.filter_map(|v| Some((v.user_id, v.channel_id?)));
        app.logger.lock().unwrap().seed_voice(g.id, in_voice);
    }
}

pub async fn on_guild_create(app: &App, ev: &GuildCreate) -> Result<()> {
    sqlx::query("DELETE FROM guild_left WHERE guild_id = $1")
        .bind(ev.id().get() as i64)
        .execute(&app.pool)
        .await?;
    let GuildCreate::Available(g) = ev else {
        return Ok(());
    };
    crate::bansync::on_owner(app, g.id, g.owner_id).await?;
    let complete = g.member_count == Some(g.members.len() as u64);
    if let Err(e) = crate::joingate::requarantine_missed(app, g.id, &g.members, complete, None).await {
        tracing::warn!(?e, "quarantining members who rejoined while the bot was down failed");
    }
    if let Err(e) = crate::verify::given::catch_up(app, g.id).await {
        tracing::warn!(?e, "checking verification roles given while the bot was down failed");
    }
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
            "{YES} `TommyBot` has been added to `{}` successfully!\n\n{wizard} Every feature is free.",
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
    // Tell whoever added the bot, and fall back to the server when their DMs are closed.
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
