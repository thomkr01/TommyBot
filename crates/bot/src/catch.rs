//! Catch channel (honeypot): hacked accounts spam every channel, so whoever writes in it is
//! punished through the normal mod path (case, mod log, appeal link) with the bot as moderator.
use anyhow::Result;
use tommybot_core::{permits::Level, sections::CatchAction};
use tommybot_logging::{embed::RED, Log, LogKind};
use twilight_model::{
    channel::Channel,
    gateway::payload::incoming::MessageCreate,
    http::channel_position::Position,
};

use crate::{
    app::App,
    commands::{apply_dm, ModKind},
    events::send_log,
    prefix::Flags,
};

const REASON: &str = "Wrote in the catch channel";

/// True when the message was in the catch channel by someone it punishes; nothing else should run.
pub async fn on_message(app: &App, m: &MessageCreate) -> Result<bool> {
    let Some(gid) = m.guild_id else { return Ok(false) };
    let cfg = app.cfg(gid.get()).await?;
    let c = &cfg.catch;
    if !c.enabled || c.channel != Some(m.channel_id) || m.author.bot || m.webhook_id.is_some() {
        return Ok(false);
    }
    let uid = m.author.id;
    let roles = m.member.as_ref().map_or(&[][..], |mem| &mem.roles);
    if c.users.contains(&uid) || roles.iter().any(|r| c.roles.contains(r)) {
        return Ok(false);
    }
    let owner = app.guild_info(gid).await?.owner;
    if app.level(gid, owner, uid).await? != Level::None {
        return Ok(false);
    }
    app.http.delete_message(m.channel_id, m.id).await.ok();
    let (kind, soft) = match c.action {
        CatchAction::Ban => (ModKind::Ban, false),
        CatchAction::Softban => (ModKind::Ban, true),
        CatchAction::Kick => (ModKind::Kick, false),
        CatchAction::Timeout => (ModKind::Timeout, false),
        CatchAction::Log => {
            let log = Log::new("Catch channel (test mode)", RED)
                .user("Member", uid.get())
                .line("Action", "None, test mode only logs");
            send_log(app, gid.get(), LogKind::AutoMod, log).await?;
            return Ok(true);
        }
    };
    let flags = Flags { dm: c.dm, soft, compact: true };
    // The bot acts with owner rights, so the usual permit and hierarchy checks still refuse
    // owners and members above the bot's role; that refusal is logged, never silent.
    let done = apply_dm(app, gid, owner, (app.me, &Level::Owner), kind, uid, Some(REASON), None, 0, flags).await;
    let text = match done {
        Ok(t) if t.starts_with("Cannot") => t,
        Ok(_) => return Ok(true),
        Err(e) => format!("Failed: {e}"),
    };
    let log = Log::new("Catch channel action failed", RED)
        .user("Member", uid.get())
        .line("Error", &text);
    send_log(app, gid.get(), LogKind::AutoMod, log).await?;
    Ok(true)
}

/// Moves the catch channel back to the top when someone drags it down.
pub async fn keep_top(app: &App, ch: &Channel) -> Result<()> {
    let Some(gid) = ch.guild_id else { return Ok(()) };
    let cfg = app.cfg(gid.get()).await?;
    let moved = ch.position.unwrap_or(0) != 0 || ch.parent_id.is_some();
    if !cfg.catch.keep_top || cfg.catch.channel != Some(ch.id) || !moved {
        return Ok(());
    }
    // Uncategorized channels sit above every category.
    let pos = Position { parent_id: Some(None), ..Position::from((ch.id, 0)) };
    app.http.update_guild_channel_positions(gid, &[pos]).await?;
    Ok(())
}
