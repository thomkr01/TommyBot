//! Gateway event dispatch: logging first, then commands, automod and anti-nuke.
use anyhow::Result;
use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Instant,
};
use tommybot_logging::{Log, LogKind, LogMessage};
use twilight_gateway::Event;
use twilight_model::{
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, Embed,
    },
    id::{
        marker::{ChannelMarker, RoleMarker},
        Id,
    },
};

use crate::{
    antinuke, app::App, automod, commands, metrics, prefix, reports, slash, tickets, verify,
};

fn guild_of(ev: &Event) -> Option<u64> {
    let g = match ev {
        Event::MessageCreate(m) => m.guild_id,
        Event::MessageUpdate(m) => m.0.guild_id,
        Event::MessageDelete(m) => m.guild_id,
        Event::MessageDeleteBulk(m) => m.guild_id,
        Event::MemberAdd(m) => Some(m.guild_id),
        Event::MemberRemove(m) => Some(m.guild_id),
        Event::GuildAuditLogEntryCreate(e) => e.0.guild_id,
        Event::AutoModerationActionExecution(e) => Some(e.guild_id),
        _ => None,
    };
    g.map(|g| g.get())
}

/// Logged text is user-controlled, so it is sent with mentions disabled.
async fn send(
    app: &App,
    channel: Id<ChannelMarker>,
    pings: &str,
    embed: &Embed,
    link: Option<&(&str, String)>,
    mentions: &AllowedMentions,
) -> Result<()> {
    let rows: Vec<Component> = link
        .map(|(label, url)| {
            Component::ActionRow(ActionRow {
                id: None,
                components: vec![Component::Button(Button {
                    id: None,
                    custom_id: None,
                    disabled: false,
                    emoji: None,
                    label: Some((*label).into()),
                    style: ButtonStyle::Link,
                    url: Some(url.clone()),
                    sku_id: None,
                })],
            })
        })
        .into_iter()
        .collect();
    app.http
        .create_message(channel)
        .content(pings)
        .embeds(std::slice::from_ref(embed))
        .components(&rows)
        .allowed_mentions(Some(mentions))
        .await?;
    Ok(())
}

async fn send_entry(app: &App, m: &LogMessage) -> Result<()> {
    let mentions = AllowedMentions::default();
    send(app, m.channel_id, "", &m.embed, m.link.as_ref(), &mentions).await
}

pub async fn send_log(app: &App, guild: u64, kind: LogKind, embed: Log) -> Result<()> {
    match app.cfg(guild).await?.logging.target(kind, None) {
        Some(ch) => {
            send(
                app,
                ch,
                "",
                &embed.build(),
                None,
                &AllowedMentions::default(),
            )
            .await
        }
        None => Ok(()),
    }
}

/// Like [`send_log`] but pings the given roles (panic mode warns staff roles).
pub async fn send_log_pinging(
    app: &App,
    guild: u64,
    kind: LogKind,
    embed: Log,
    roles: &[Id<RoleMarker>],
) -> Result<()> {
    let Some(ch) = app.cfg(guild).await?.logging.target(kind, None) else {
        return Ok(());
    };
    let pings: String = roles.iter().map(|r| format!("<@&{r}> ")).collect();
    let mentions = AllowedMentions {
        roles: roles.to_vec(),
        ..Default::default()
    };
    send(app, ch, pings.trim_end(), &embed.build(), None, &mentions).await
}

/// Sends held delete logs once their wait is over. `handle` only flushes when an event arrives,
/// so a quiet guild would otherwise never log its deletes.
pub async fn flush_logs(app: Arc<App>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        let due = app.logger.lock().unwrap().flush(Instant::now());
        for m in due {
            if let Err(e) = send_entry(&app, &m).await {
                tracing::warn!(?e, channel = %m.channel_id, "log send failed");
            }
        }
    }
}

pub async fn handle(app: Arc<App>, ev: Event) {
    metrics::EVENTS.fetch_add(1, Relaxed);
    metrics::IN_FLIGHT.fetch_add(1, Relaxed);
    if let Err(e) = run(&app, &ev).await {
        tracing::warn!(?e, kind = ?ev.kind(), "event handler failed");
        metrics::EVENT_ERRORS.fetch_add(1, Relaxed);
    }
    metrics::IN_FLIGHT.fetch_sub(1, Relaxed);
}

/// Last step before exit: lets running handlers finish (strikes and locks are written as they
/// happen, so nothing else is pending), sends the held logs and closes the pool.
pub async fn shutdown(app: &App) {
    for _ in 0..50 {
        if metrics::IN_FLIGHT.load(Relaxed) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let held = app
        .logger
        .lock()
        .unwrap()
        .flush(Instant::now() + std::time::Duration::from_secs(3600));
    for m in held {
        if let Err(e) = send_entry(app, &m).await {
            tracing::warn!(?e, "log send failed during shutdown");
        }
    }
    app.pool.close().await;
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
            if let Err(e) = send_entry(app, &m).await {
                tracing::warn!(?e, channel = %m.channel_id, "log send failed");
            }
        }
    }
    match ev {
        // Webhook posts count as bots but still carry heat, so they reach automod (never commands).
        Event::MessageCreate(m) if !m.author.bot || m.webhook_id.is_some() => {
            if crate::catch::on_message(app, m).await? {
                return Ok(());
            }
            if let (false, Some(gid)) = (m.author.bot, m.guild_id) {
                let pfx = app.cfg(gid.get()).await?.misc.prefix.clone();
                if let Some(parsed) = m.content.strip_prefix(&pfx).and_then(prefix::parse) {
                    commands::run(app, m, parsed).await?;
                }
            }
            automod::on_message(app, m).await?;
        }
        Event::RoleCreate(r) => app.invalidate_guild(r.guild_id),
        Event::RoleUpdate(r) => app.invalidate_guild(r.guild_id),
        Event::RoleDelete(r) => app.invalidate_guild(r.guild_id),
        Event::ChannelCreate(c) => {
            app.note_parent(&c.0);
            c.guild_id.into_iter().for_each(|g| app.invalidate_guild(g))
        }
        Event::ChannelUpdate(c) => {
            c.guild_id.into_iter().for_each(|g| app.invalidate_guild(g));
            crate::catch::keep_top(app, c).await?;
        }
        Event::ChannelDelete(c) => {
            app.note_parent(&c.0);
            c.guild_id.into_iter().for_each(|g| app.invalidate_guild(g))
        }
        Event::GuildUpdate(g) => app.invalidate_guild(g.id),
        Event::GuildCreate(g) => crate::welcome::on_guild_create(app, g).await?,
        Event::MemberUpdate(m) if m.user.id == app.me => app.invalidate_guild(m.guild_id),
        Event::MemberUpdate(m) => {
            if let Err(e) = antinuke::enforce_permits(app, m).await {
                tracing::warn!(?e, "enforcing permits failed");
            }
            if let Err(e) = crate::timeout_roles::on_member_update(app, m).await {
                tracing::warn!(?e, "removing main roles on timeout failed");
            }
            automod::on_member_update(app, m).await?
        }
        Event::MemberAdd(m) => automod::on_join(app, m).await?,
        Event::InteractionCreate(i) => {
            if !verify::handle(app, &i.0).await?
                && !tickets::handle(app, &i.0).await?
                && !commands::handle_confirm(app, &i.0).await?
                && !reports::handle(app, &i.0).await?
            {
                slash::handle(app, &i.0).await?
            }
        }
        Event::GuildAuditLogEntryCreate(e) => antinuke::on_audit(app, &e.0).await?,
        Event::AutoModerationActionExecution(e) => automod::on_native_hit(app, e).await?,
        _ => {}
    }
    Ok(())
}
