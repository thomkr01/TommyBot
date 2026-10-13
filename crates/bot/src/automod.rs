//! Message heat -> strikes -> timeouts, join gate and raid detection, mention-triggered lockdown.
use anyhow::Result;
use std::collections::{hash_map::Entry, HashMap};
use tommybot_antinuke::{webhooks_to_delete, Webhook};
use tommybot_automod::{
    filters,
    heat::{heat, Breakdown, HeatCfg, MsgFacts},
    join::{gate, harshest, is_suspicious, Action, Joiner},
    lockdown::{JoinLock, LockKind},
    store,
    strike::strike,
};
use tommybot_core::{
    cases::{self, NewCase},
    jobs,
    sections::{FilterAction, FilterCfg, FiltersCfg},
};
use tommybot_logging::LogKind;
use twilight_model::{
    gateway::payload::incoming::{MemberAdd, MessageCreate},
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::{now_secs, App, GuildCfg},
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
    let wl = &cfg.whitelist;
    if wl.channels.contains(&m.channel_id)
        || wl.users.contains(&m.author.id)
        || m.webhook_id
            .is_some_and(|w| wl.webhooks.iter().any(|x| x.get() == w.get()))
        || m.member
            .as_ref()
            .is_some_and(|mem| mem.roles.iter().any(|r| wl.roles.contains(r)))
    {
        return Ok(());
    }
    let now = now_ms();
    let content = m.content.as_str();
    let f = &cfg.automod.filters;
    let hosts = filters::hosts(content);
    let (upper_chars, lower_chars) = filters::case_counts(content);
    let words = if f.words.use_discord_automod {
        vec![]
    } else {
        blacklist_words(f)
    };
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
        invite_link: f.invites.enabled
            && ["discord.gg/", "discord.com/invite/"]
                .iter()
                .any(|p| content.to_lowercase().contains(p)),
        // shortcut: no phishing/NSFW site lists are bundled, so bad_sites and nsfw_links never trip.
        malicious_link: f.scam_links.enabled && filters::is_nitro_steam_scam(content),
        blacklisted_link: f.links.enabled
            && filters::any_on_domains(&hosts, &f.blacklisted_domains),
        blacklisted_words: if f.words.filter.enabled {
            filters::word_hits(content, &words)
        } else {
            0
        },
        zalgo: filters::is_zalgo(content),
        upper_chars,
        lower_chars,
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
    let heat_cfg = enabled_weights(&cfg.automod.heat, f);
    let (triggered, breakdown) = {
        let mut map = app.heat.lock().unwrap();
        // shortcut: heat state is dropped wholesale past 100k users, move to a TTL cache when that bites.
        if map.len() > 100_000 {
            map.clear();
        }
        let (state, hist) = map.entry(key).or_default();
        let b = heat(&facts, hist, &heat_cfg);
        hist.remember(content);
        (state.apply(&b, now, &heat_cfg), b)
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
    if let Some(fc) = tripped_filter(&breakdown, &facts, f) {
        if let Some(action) = fc.action {
            return filter_action(app, &cfg, gid, m, action, fc.duration_secs).await;
        }
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
    send_log(app, gid.get(), LogKind::AutoMod, &text).await?;
    if cfg
        .automod
        .strike
        .lock_at_strikes
        .is_some_and(|n| act.strike >= n)
        && lockdown::lock(app, gid, LockKind::Server).await?
    {
        send_log(
            app,
            gid.get(),
            LogKind::Panic,
            &format!(
                "<@{}> reached strike {}: server locked",
                m.author.id, act.strike
            ),
        )
        .await?;
    }
    Ok(())
}

/// Strict words as written, wildcard words wrapped in `*`, plus the toggled premade lists.
/// Discord's native keyword rules use the same syntax.
pub fn blacklist_words(f: &FiltersCfg) -> Vec<String> {
    let w = &f.words;
    w.strict
        .iter()
        .cloned()
        .chain(w.wildcard.iter().map(|x| format!("*{x}*")))
        .chain(filters::premade_words(
            w.premade_profane,
            w.premade_vulgar,
            w.premade_racist,
        ))
        .collect()
}

/// Heat weights with every disabled filter zeroed.
fn enabled_weights(h: &HeatCfg, f: &FiltersCfg) -> HeatCfg {
    let mut h = h.clone();
    for (on, w) in [
        (f.regular.enabled, &mut h.base),
        (f.similar.enabled, &mut h.similarity),
        (f.emojis.enabled, &mut h.per_emoji),
        (f.newlines.enabled, &mut h.per_newline),
        (f.chars_upper.enabled, &mut h.per_upper),
        (f.chars_lower.enabled, &mut h.per_lower),
        (f.zalgo.enabled, &mut h.zalgo),
        (f.suspicion.enabled, &mut h.suspicious_account),
        (f.inactive_channel.enabled, &mut h.inactive_channel),
    ] {
        if !on {
            *w = 0.0;
        }
    }
    h
}

/// The filter that pushed the message over: the link filter behind an instant trip, else the biggest contributor.
fn tripped_filter<'a>(b: &Breakdown, m: &MsgFacts, f: &'a FiltersCfg) -> Option<&'a FilterCfg> {
    if b.instant_max {
        return Some(if m.invite_link {
            &f.invites
        } else if m.malicious_link {
            &f.scam_links
        } else {
            &f.links
        });
    }
    let (name, _) = b.parts.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    Some(match *name {
        "message" => &f.regular,
        "similarity" => &f.similar,
        "emojis" => &f.emojis,
        "newlines" => &f.newlines,
        "uppercase" => &f.chars_upper,
        "lowercase" => &f.chars_lower,
        "zalgo" => &f.zalgo,
        "suspicious_account" => &f.suspicion,
        "inactive_channel" => &f.inactive_channel,
        "blacklisted_words" => &f.words.filter,
        _ => return None,
    })
}

/// A filter's own action replaces the strike system: no strike is recorded.
async fn filter_action(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    m: &MessageCreate,
    action: FilterAction,
    duration_secs: Option<i64>,
) -> Result<()> {
    let uid = m.author.id;
    app.http.delete_message(m.channel_id, m.id).await.ok();
    if action != FilterAction::Delete && cfg.automod.strike.reset_heat_on_timeout {
        if let Some((s, _)) = app.heat.lock().unwrap().get_mut(&(gid.get(), uid.get())) {
            s.value = 0.0;
        }
    }
    let secs = duration_secs.unwrap_or(cfg.misc.timeout_secs() as i64);
    match action {
        FilterAction::Delete => {}
        FilterAction::Warn => {
            cases::create(
                &app.pool,
                NewCase {
                    guild_id: gid.get() as i64,
                    kind: "warn",
                    user_id: uid.get() as i64,
                    mod_id: app.me.get() as i64,
                    reason: Some("automod filter"),
                    points: cfg.misc.warn_base_points,
                    duration_s: None,
                    inverse: None,
                },
            )
            .await?;
        }
        FilterAction::Timeout => timeout(&app.http, gid, uid, secs).await?,
        FilterAction::Kick => {
            app.http.remove_guild_member(gid, uid).await?;
        }
        FilterAction::Ban => {
            app.http.create_ban(gid, uid).await?;
        }
    }
    send_log(
        app,
        gid.get(),
        LogKind::AutoMod,
        &format!("<@{uid}>: {action:?} (filter)"),
    )
    .await
}

/// Keeps the native Discord AutoMod keyword rule in step with the saved word lists.
/// Only runs while the guild opted in; turning it off is done from the rule list (`Api::set_rule_enabled`).
pub async fn sync_discord_rule(app: std::sync::Arc<App>, guild: u64) {
    let run = async {
        let cfg = app.cfg(guild).await?;
        let f = &cfg.automod.filters;
        if f.words.use_discord_automod {
            app.api
                .sync_keyword_rule(guild, &blacklist_words(f))
                .await?;
        }
        anyhow::Ok(())
    };
    if let Err(e) = run.await {
        tracing::warn!(?e, guild, "syncing the Discord AutoMod rule failed");
    }
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
    let cfg = app.cfg(gid.get()).await?;
    let doomed = webhooks_to_delete(
        &hooks,
        now_secs() - 600,
        |o| immune[&o],
        |id| cfg.whitelist.webhooks.iter().any(|w| w.get() == id),
    );
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
    if let Some(raid) = cfg.joinraid.as_ref().filter(|r| r.enabled) {
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
            jobs::replace(
                &app.pool,
                gid.get() as i64,
                "raid_end",
                serde_json::Value::Null,
                (now / 1000) + raid.active_ms / 1000,
            )
            .await?;
            send_log(
                app,
                gid.get(),
                LogKind::JoinRaid,
                &format!("Raid detected, {} accounts flagged", out.flagged.len()),
            )
            .await?;
        }
        if let Some(kind) = out.triggered.then(|| raid.lock.clone()).flatten() {
            lockdown::lock(app, gid, kind).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_filters_lose_their_weight() {
        let mut f = FiltersCfg::default();
        f.zalgo.enabled = false;
        f.regular.enabled = false;
        let h = enabled_weights(&HeatCfg::default(), &f);
        assert_eq!((h.zalgo, h.base), (0.0, 0.0));
        assert_eq!(h.per_emoji, HeatCfg::default().per_emoji);
    }

    #[test]
    fn biggest_part_picks_the_filter_and_links_win_on_instant() {
        let mut f = FiltersCfg::default();
        f.zalgo.action = Some(FilterAction::Ban);
        let b = Breakdown {
            parts: vec![("message", 18.0), ("zalgo", 40.0)],
            instant_max: false,
        };
        let got = tripped_filter(&b, &MsgFacts::default(), &f).unwrap();
        assert_eq!(got.action, Some(FilterAction::Ban));
        f.invites.action = Some(FilterAction::Kick);
        let facts = MsgFacts {
            invite_link: true,
            ..Default::default()
        };
        let b = Breakdown {
            parts: vec![],
            instant_max: true,
        };
        let got = tripped_filter(&b, &facts, &f).unwrap();
        assert_eq!(got.action, Some(FilterAction::Kick));
    }
}
