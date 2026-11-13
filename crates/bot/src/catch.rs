//! Catch channel (honeypot): hacked accounts spam every channel, so whoever writes in it is
//! punished through the normal mod path (case, mod log, appeal link) with the bot as moderator.
use anyhow::Result;
use tommybot_core::{permits::Level, sections::CatchAction};
use tommybot_logging::{embed::RED, Log, LogKind};
use twilight_model::{
    channel::Channel,
    gateway::payload::incoming::MessageCreate,
    http::channel_position::Position,
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::App,
    commands::{apply_dm, ModKind},
    events::send_log,
    prefix::Flags,
};

const REASON: &str = "Wrote in the catch channel";

/// True when the message was in the catch channel by someone it punishes; nothing else should run.
/// Log mode punishes nobody, so automod still runs.
pub async fn on_message(app: &App, m: &MessageCreate) -> Result<bool> {
    let Some(gid) = m.guild_id else {
        return Ok(false);
    };
    let cfg = app.cfg(gid.get()).await?;
    let c = &cfg.catch;
    let Some(catch) = c.channel.filter(|_| c.enabled) else {
        return Ok(false);
    };
    if m.author.bot || m.webhook_id.is_some() {
        return Ok(false);
    }
    // Threads of the catch channel catch too.
    if catch != m.channel_id
        && !app
            .channel_scope(gid, m.channel_id)
            .await
            .is_ok_and(|(parent, _)| parent == catch)
    {
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
    let (kind, soft) = match c.action {
        CatchAction::Ban => (ModKind::Ban, false),
        CatchAction::Softban => (ModKind::Ban, true),
        CatchAction::Kick => (ModKind::Kick, false),
        CatchAction::Timeout => (ModKind::Timeout, false),
        CatchAction::Log => {
            let log = Log::new("Catch channel (test mode)", RED)
                .user("Member", uid.get())
                .line("Action", "None, test mode only logs");
            if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
                tracing::warn!(?e, "catch channel test log failed");
            }
            return Ok(false);
        }
    };
    crate::http_err::best_effort(
        app.http.delete_message(m.channel_id, m.id).await,
        "delete caught message",
    );
    let flags = Flags {
        dm: c.dm,
        soft,
        compact: true,
        ..Default::default()
    };
    // The bot acts with owner rights, so the usual permit and hierarchy checks still refuse
    // owners and members above the bot's role; that refusal is logged, never silent.
    let done = apply_dm(
        app,
        gid,
        owner,
        (app.me, &Level::Owner),
        kind,
        uid,
        Some(REASON),
        None,
        0,
        flags,
        None,
    )
    .await;
    let text = match done {
        Ok(a) if a.case.is_none() => a.text,
        Ok(_) => return Ok(true),
        Err(e) => format!("Failed: {e}"),
    };
    let log = Log::new("Catch channel action failed", RED)
        .user("Member", uid.get())
        .line("Error", &text);
    // The member was caught: a failed log must not hand the message on as uncaught.
    if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
        tracing::warn!(?e, "catch channel failure log failed");
    }
    Ok(true)
}

const NOTICE: &str = "Catch channel";

/// What the catch channel says about itself, for its action.
fn notice(a: CatchAction) -> String {
    let what = match a {
        CatchAction::Ban | CatchAction::Softban => "get you automatically banned",
        CatchAction::Kick => "get you automatically kicked",
        CatchAction::Timeout => "get you automatically timed out",
        CatchAction::Log => "be logged for the staff team",
    };
    format!("This channel is to catch bots which automatically spam each channel with scams before they are able to post to the rest of the server.\n\n**Sending a message in this channel will {what}.**")
}

/// Posts the warning in the catch channel once: skipped when the bot's warning is already there.
pub async fn post_notice(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let c = &cfg.catch;
    let Some(ch) = c.channel.filter(|_| c.enabled) else {
        return Ok(());
    };
    let recent = app
        .http
        .channel_messages(ch)
        .limit(50)
        .await?
        .models()
        .await?;
    let mine = |m: &twilight_model::channel::Message| {
        m.author.id == app.me && m.embeds.iter().any(|e| e.title.as_deref() == Some(NOTICE))
    };
    if recent.iter().any(mine) {
        return Ok(());
    }
    let e = Log::new(NOTICE, RED).text(&notice(c.action)).build();
    app.http.create_message(ch).embeds(&[e]).await?;
    Ok(())
}

/// Moves the catch channel back to the top when someone drags it down.
pub async fn keep_top(app: &App, ch: &Channel) -> Result<()> {
    let Some(gid) = ch.guild_id else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    let moved = ch.position.unwrap_or(0) != 0 || ch.parent_id.is_some();
    if !cfg.catch.keep_top || cfg.catch.channel != Some(ch.id) || !moved {
        return Ok(());
    }
    // Uncategorized channels sit above every category.
    let pos = Position {
        parent_id: Some(None),
        ..Position::from((ch.id, 0))
    };
    app.http.update_guild_channel_positions(gid, &[pos]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_names_the_action() {
        assert!(notice(CatchAction::Softban).contains("automatically banned"));
        assert!(notice(CatchAction::Log).contains("logged"));
    }
}
