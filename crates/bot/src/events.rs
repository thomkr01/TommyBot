//! Gateway event dispatch: logging first, then commands, automod and anti-nuke.
use anyhow::Result;
use std::{sync::Arc, time::Instant};
use tommybot_logging::LogKind;
use twilight_gateway::Event;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        marker::{ChannelMarker, RoleMarker},
        Id,
    },
};

use crate::{antinuke, app::App, automod, commands, prefix, slash};

fn guild_of(ev: &Event) -> Option<u64> {
    let g = match ev {
        Event::MessageCreate(m) => m.guild_id,
        Event::MessageUpdate(m) => m.0.guild_id,
        Event::MessageDelete(m) => m.guild_id,
        Event::MessageDeleteBulk(m) => m.guild_id,
        Event::MemberAdd(m) => Some(m.guild_id),
        Event::MemberRemove(m) => Some(m.guild_id),
        Event::GuildAuditLogEntryCreate(e) => e.0.guild_id,
        _ => None,
    };
    g.map(|g| g.get())
}

/// Logged text is user-controlled, so it is sent with mentions disabled.
async fn send(
    app: &App,
    channel: Id<ChannelMarker>,
    text: &str,
    mentions: &AllowedMentions,
) -> Result<()> {
    app.http
        .create_message(channel)
        .content(text)
        .allowed_mentions(Some(mentions))
        .await?;
    Ok(())
}

pub async fn send_log(app: &App, guild: u64, kind: LogKind, text: &str) -> Result<()> {
    match app.cfg(guild).await?.logging.target(kind) {
        Some(ch) => send(app, ch, text, &AllowedMentions::default()).await,
        None => Ok(()),
    }
}

/// Like [`send_log`] but pings the given roles (panic mode warns staff roles).
pub async fn send_log_pinging(
    app: &App,
    guild: u64,
    kind: LogKind,
    text: &str,
    roles: &[Id<RoleMarker>],
) -> Result<()> {
    let Some(ch) = app.cfg(guild).await?.logging.target(kind) else {
        return Ok(());
    };
    let pings: String = roles.iter().map(|r| format!("<@&{r}> ")).collect();
    let mentions = AllowedMentions {
        roles: roles.to_vec(),
        ..Default::default()
    };
    send(app, ch, &format!("{pings}{text}"), &mentions).await
}

pub async fn handle(app: Arc<App>, ev: Event) {
    if let Err(e) = run(&app, &ev).await {
        tracing::warn!(?e, kind = ?ev.kind(), "event handler failed");
    }
}

async fn run(app: &App, ev: &Event) -> Result<()> {
    if let Some(guild) = guild_of(ev) {
        let cfg = app.cfg(guild).await?;
        let msgs = app
            .logger
            .lock()
            .unwrap()
            .handle(&cfg.logging, ev, Instant::now());
        for m in msgs {
            if let Err(e) = send(app, m.channel_id, &m.content, &AllowedMentions::default()).await {
                tracing::warn!(?e, channel = %m.channel_id, "log send failed");
            }
        }
    }
    match ev {
        // Webhook posts count as bots but still carry heat, so they reach automod (never commands).
        Event::MessageCreate(m) if !m.author.bot || m.webhook_id.is_some() => {
            if !m.author.bot {
                if let Some(parsed) = m
                    .content
                    .strip_prefix(commands::PREFIX)
                    .and_then(prefix::parse)
                {
                    commands::run(app, m, parsed).await?;
                }
            }
            automod::on_message(app, m).await?;
        }
        Event::RoleCreate(r) => app.invalidate_guild(r.guild_id),
        Event::RoleUpdate(r) => app.invalidate_guild(r.guild_id),
        Event::RoleDelete(r) => app.invalidate_guild(r.guild_id),
        Event::GuildUpdate(g) => app.invalidate_guild(g.id),
        Event::MemberUpdate(m) if m.user.id == app.me => app.invalidate_guild(m.guild_id),
        Event::MemberAdd(m) => automod::on_join(app, m).await?,
        Event::InteractionCreate(i) => slash::handle(app, &i.0).await?,
        Event::GuildAuditLogEntryCreate(e) => antinuke::on_audit(app, &e.0).await?,
        _ => {}
    }
    Ok(())
}
