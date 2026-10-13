//! Message heat -> strikes -> timeouts, join gate and raid detection, mention-triggered lockdown.
use anyhow::Result;
use std::collections::{hash_map::Entry, HashMap};
use tommybot_antinuke::{webhooks_to_delete, Webhook};
use tommybot_automod::{
    heat::{heat, MsgFacts},
    join::{gate, harshest, is_suspicious, Action, Joiner},
    lockdown::{JoinLock, LockKind},
    store,
    strike::strike,
};
use tommybot_logging::LogKind;
use twilight_model::{
    gateway::payload::incoming::{MemberAdd, MessageCreate},
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::{now_secs, App},
    commands::timeout,
    events::send_log,
    lockdown,
};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub async fn on_message(app: &App, m: &MessageCreate) -> Result<()> {
    let Some(gid) = m.guild_id else { return Ok(()) };
    let cfg = app.cfg(gid.get()).await?;
    if !cfg.automod.enabled {
        return Ok(());
    }
    let now = now_ms();
    let content = m.content.as_str();
    let facts = MsgFacts {
        content,
        // shortcut: custom emoji only, unicode emoji are not counted.
        emojis: content.matches("<:").count() + content.matches("<a:").count(),
        user_mentions: m.mentions.len(),
        everyone_mentions: usize::from(m.mention_everyone),
        role_mentions: m.mention_roles.len(),
        attachments: m.attachments.len(),
        embeds: m.embeds.len(),
        stickers: m.sticker_items.len(),
        invite_link: ["discord.gg/", "discord.com/invite/"]
            .iter()
            .any(|p| content.to_lowercase().contains(p)),
        from_webhook: m.webhook_id.is_some(),
        ..Default::default()
    };
    let mention_lock = cfg.automod.mention_lock.is_some_and(|(limit, span)| {
        let n = facts.user_mentions + facts.everyone_mentions + facts.role_mentions;
        n > 0
            && app
                .mentions
                .lock()
                .unwrap()
                .entry(gid.get())
                .or_default()
                .on_mentions(n, now, limit, span)
    });
    let key = (gid.get(), m.author.id.get());
    let triggered = {
        let mut map = app.heat.lock().unwrap();
        // shortcut: heat state is dropped wholesale past 100k users, move to a TTL cache when that bites.
        if map.len() > 100_000 {
            map.clear();
        }
        let (state, hist) = map.entry(key).or_default();
        let b = heat(&facts, hist, &cfg.automod.heat);
        hist.remember(content);
        state.apply(&b, now, &cfg.automod.heat)
    };
    if !triggered && !mention_lock {
        return Ok(());
    }
    if app.is_immune(gid, m.author.id).await? {
        return Ok(());
    }
    if mention_lock && lockdown::lock(app, gid, LockKind::Server).await? {
        send_log(
            app,
            gid.get(),
            LogKind::Panic,
            &format!("Mention spam from <@{}>: server locked", m.author.id),
        )
        .await?;
    }
    if !triggered {
        return Ok(());
    }
    if m.webhook_id.is_some() {
        // A webhook is not a member: no strikes, remove the message and the freshly made hooks.
        app.http.delete_message(m.channel_id, m.id).await.ok();
        return delete_new_webhooks(app, gid).await;
    }
    let mut st = store::load_strikes(&app.pool, gid.get(), m.author.id.get()).await?;
    let act = strike(&mut st, &cfg.automod.strike, now);
    store::save_strikes(&app.pool, gid.get(), m.author.id.get(), &st).await?;
    if act.reset_heat {
        if let Some((s, _)) = app.heat.lock().unwrap().get_mut(&key) {
            s.value = 0.0;
        }
    }
    // shortcut: only the triggering message is purged, not the user's recent history.
    app.http.delete_message(m.channel_id, m.id).await.ok();
    timeout(&app.http, gid, m.author.id, act.timeout_ms / 1000).await?;
    let text = format!(
        "<@{}> strike {} ({}s timeout{})",
        m.author.id,
        act.strike,
        act.timeout_ms / 1000,
        if act.purge { ", cap" } else { "" }
    );
    send_log(app, gid.get(), LogKind::AutoMod, &text).await
}

/// Deletes webhooks made in the last 10 minutes unless their owner is immune.
async fn delete_new_webhooks(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let mut hooks = vec![];
    let mut immune: HashMap<u64, bool> = HashMap::new();
    for h in app.http.guild_webhooks(gid).await?.models().await? {
        // shortcut: hooks without an owner (integrations) are never deleted.
        let Some(owner) = h.user.as_ref().map(|u| u.id) else {
            continue;
        };
        if let Entry::Vacant(e) = immune.entry(owner.get()) {
            e.insert(app.is_immune(gid, owner).await?);
        }
        // Snowflake timestamp: ms since the Discord epoch in the top bits.
        let created_at = ((h.id.get() >> 22) + 1_420_070_400_000) / 1000;
        hooks.push(Webhook {
            id: h.id.get(),
            owner: owner.get(),
            created_at: created_at as i64,
        });
    }
    // shortcut: no per-webhook whitelist in the config yet.
    let doomed = webhooks_to_delete(&hooks, now_secs() - 600, |o| immune[&o], |_| false);
    for id in &doomed {
        app.http.delete_webhook(Id::new(*id)).await.ok();
    }
    let text = format!("Deleted {} new webhook(s) after webhook spam", doomed.len());
    send_log(app, gid.get(), LogKind::AutoMod, &text).await
}

pub async fn on_join(app: &App, m: &MemberAdd) -> Result<()> {
    let (gid, user) = (m.guild_id, &m.member.user);
    let cfg = app.cfg(gid.get()).await?;
    let now = now_ms();
    if let Some(lock) = lockdown::join_lock(app, gid).await? {
        let action = match lock {
            JoinLock::Kick => Action::Kick,
            JoinLock::Ban => Action::Ban,
        };
        return punish(app, gid, user.id.get(), action, "join lock").await;
    }
    // shortcut: names are not run through decancer, and bot adders/verification are not looked up
    // (bots count as authorized so the unauthorized-bot filter never fires).
    let j = Joiner {
        id: user.id.get(),
        name: &user.name,
        has_avatar: user.avatar.is_some(),
        is_bot: user.bot,
        bot_verified: false,
        added_by_authorized: true,
    };
    if let Some(action) = harshest(&gate(&j, &cfg.joingate, now)) {
        punish(app, gid, j.id, action, "join gate").await?;
    }
    if let Some(raid) = &cfg.joinraid {
        let min_age = cfg.joingate.min_age.map_or(0, |a| a.0);
        let suspicious = is_suspicious(&j, &cfg.joingate, min_age, now);
        let out = app
            .raids
            .lock()
            .unwrap()
            .entry(gid.get())
            .or_default()
            .on_join(&j, suspicious, raid, now);
        if out.triggered {
            send_log(
                app,
                gid.get(),
                LogKind::JoinRaid,
                &format!("Raid detected, {} accounts flagged", out.flagged.len()),
            )
            .await?;
        }
        for id in out.flagged {
            punish(app, gid, id, raid.action, "join raid").await?;
        }
    }
    Ok(())
}

async fn punish(
    app: &App,
    gid: Id<GuildMarker>,
    user: u64,
    action: Action,
    why: &str,
) -> Result<()> {
    let uid = Id::new(user);
    match action {
        Action::Log => {}
        Action::Timeout(ms) if ms > 0 => timeout(&app.http, gid, uid, ms / 1000).await?,
        Action::Timeout(_) => return Ok(()),
        Action::Kick => {
            app.http.remove_guild_member(gid, uid).await?;
        }
        Action::Ban => {
            app.http.create_ban(gid, uid).await?;
        }
    }
    send_log(
        app,
        gid.get(),
        LogKind::JoinGate,
        &format!("<@{user}>: {action:?} ({why})"),
    )
    .await
}
