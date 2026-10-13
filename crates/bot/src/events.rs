//! Gateway event dispatch: logging first, then commands, automod and anti-nuke.
use anyhow::Result;
use std::{sync::Arc, time::Instant};
use tommybot_logging::LogKind;
use twilight_gateway::Event;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{marker::ChannelMarker, Id},
};

use crate::{antinuke, app::App, automod, commands, prefix};

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
async fn send(app: &App, channel: Id<ChannelMarker>, text: &str) -> Result<()> {
    app.http
        .create_message(channel)
        .content(text)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

pub async fn send_log(app: &App, guild: u64, kind: LogKind, text: &str) -> Result<()> {
    match app.cfg(guild).await?.logging.target(kind) {
        Some(ch) => send(app, ch, text).await,
        None => Ok(()),
    }
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
            if let Err(e) = send(app, m.channel_id, &m.content).await {
                tracing::warn!(?e, channel = %m.channel_id, "log send failed");
            }
        }
    }
    match ev {
        Event::MessageCreate(m) if !m.author.bot => {
            if let Some(parsed) = m
                .content
                .strip_prefix(commands::PREFIX)
                .and_then(prefix::parse)
            {
                commands::run(app, m, parsed).await?;
            } else {
                automod::on_message(app, m).await?;
            }
        }
        Event::MemberAdd(m) => automod::on_join(app, m).await?,
        Event::GuildAuditLogEntryCreate(e) => antinuke::on_audit(app, &e.0).await?,
        _ => {}
    }
    Ok(())
}
