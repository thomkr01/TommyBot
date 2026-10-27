//! Message heat -> strikes -> timeouts, join gate and raid detection, mention-triggered lockdown.
use anyhow::Result;
use std::collections::{hash_map::Entry, HashMap};
use tommybot_antinuke::{webhooks_to_delete, Webhook};
use tommybot_automod::{
    discord_api::BotAction,
    filters,
    heat::{heat, Breakdown, HeatCfg, MsgFacts, Whitelist},
    join::{gate, harshest, is_suspicious, Action, Joiner, RaidCfg},
    lockdown::JoinLock,
    store,
    strike::strike,
};
use tommybot_core::{
    cases::{self, NewCase},
    jobs,
    sections::{AutomodCfg, FilterAction, FilterCfg, FiltersCfg, WhitelistList},
};
use tommybot_logging::{
    embed::{channel, duration, ALERT, ARROW, ORANGE, RED, YES},
    Log, LogKind,
};
use twilight_model::{
    gateway::payload::incoming::{
        AutoModerationActionExecution, MemberAdd, MemberUpdate, MessageCreate,
    },
    guild::auto_moderation::AutoModerationActionType,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
    user::UserFlags,
};

use crate::{
    app::{now_secs, App, GuildCfg},
    commands::{self, timeout},
    events::{send_log, send_log_pinging},
    lockdown, reply, verify,
};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub async fn on_message(app: &App, m: &MessageCreate) -> Result<()> {
    let Some(gid) = m.guild_id else { return Ok(()) };
    let cfg = app.cfg(gid.get()).await?;
    let am = &cfg.automod;
    if !am.enabled || (m.webhook_id.is_some() && !am.monitor_webhooks) {
        return Ok(());
    }
    let wl = &cfg.whitelist;
    let roles = m.member.as_ref().map_or(&[][..], |mem| &mem.roles);
    let any_categories = [&wl.spam, &wl.invites, &wl.mentions, &wl.everyone]
        .iter()
        .any(|l| !l.categories.is_empty());
    let category = if any_categories {
        app.guild_info(gid)
            .await?
            .parents
            .get(&m.channel_id)
            .copied()
    } else {
        None
    };
    let listed =
        |l: &WhitelistList| l.covers(m.author.id, roles, m.channel_id, category, m.webhook_id);
    if listed(&wl.spam) {
        return Ok(());
    }
    let now = now_ms();
    let content = m.content.as_str();
    let f = &am.filters;
    let hosts = filters::hosts(content);
    let mut checked = hosts.clone();
    if f.nsfw_crawl_shorteners {
        for u in filters::shortener_urls(content).into_iter().take(3) {
            checked.extend(app.sites.unshorten(u).await);
        }
    }
    let bad_site = f.bad_sites.enabled && app.sites.phishing(&checked);
    let (upper_chars, lower_chars) = filters::case_counts(content);
    let words = if f.words.use_discord_automod {
        vec![]
    } else {
        blacklist_words(f)
    };
    let images = m
        .attachments
        .iter()
        .filter(|a| {
            a.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
        })
        .count();
    let mut facts = MsgFacts {
        content,
        emojis: count_emojis(content),
        user_mentions: m.mentions.len(),
        everyone_mentions: usize::from(m.mention_everyone),
        role_mentions: m.mention_roles.len(),
        images,
        files: m.attachments.len() - images,
        embeds: m.embeds.len(),
        links: hosts.len(),
        stickers: m.sticker_items.len(),
        channel_inactive: app
            .channels
            .lock()
            .unwrap()
            .entry(m.channel_id.get())
            .or_default()
            .on_message(now, f.inactive_messages, f.inactive_hours),
        invite_link: f.invites.enabled
            && ["discord.gg/", "discord.com/invite/"]
                .iter()
                .any(|p| content.to_lowercase().contains(p)),
        malicious_link: bad_site || f.scam_links.enabled && filters::is_nitro_steam_scam(content),
        nsfw_link: f.nsfw_links.enabled && app.sites.nsfw(&checked),
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
        whitelist: Whitelist {
            spam: false,
            mention_spam: listed(&wl.mentions),
            advertising: listed(&wl.invites),
            everyone: listed(&wl.everyone),
        },
        ..Default::default()
    };
    if facts.blacklisted_words > 0 && app.whitelisted(gid, m.author.id, "words").await? {
        facts.blacklisted_words = 0;
    }
    if facts.blacklisted_link && app.whitelisted(gid, m.author.id, "links").await? {
        facts.blacklisted_link = false;
    }
    let pings = facts.user_mentions + facts.everyone_mentions + facts.role_mentions;
    let lock = &am.auto_lockdown;
    let mention_lock = lock.enabled
        && lock.mentions
        && pings > 0
        && app
            .mentions
            .lock()
            .unwrap()
            .entry(gid.get())
            .or_default()
            .on_mentions(pings, now, lock.mention_count, lock.mention_window_ms);
    let over_hour_cap = am.anti_spam && f.mentions.enabled && pings > 0 && {
        let mut map = app.pings.lock().unwrap();
        // shortcut: dropped wholesale past 100k members, move to a TTL cache when that bites.
        if map.len() > 100_000 {
            map.clear();
        }
        map.entry((gid.get(), m.author.id.get()))
            .or_default()
            .on_mentions(pings, now, f.mention_hour_cap + 1, 3_600_000)
    };
    let key = (gid.get(), m.author.id.get());
    let heat_cfg = enabled_weights(am);
    let (triggered, breakdown) = {
        let mut map = app.heat.lock().unwrap();
        // Past 100k users, users idle for 10 minutes go first; everyone only if that frees nothing.
        if map.len() > 100_000 {
            map.retain(|_, (s, _)| s.at_ms > now - 600_000);
            if map.len() > 100_000 {
                map.clear();
            }
        }
        let (state, hist) = map.entry(key).or_default();
        let mut b = heat(&facts, hist, &heat_cfg);
        if over_hour_cap {
            b.parts.push(("mentions", heat_cfg.max));
        }
        hist.remember(content);
        hist.track(m.channel_id.get(), m.id.get());
        (state.apply(&b, now, &heat_cfg), b)
    };
    if !triggered && !mention_lock {
        return Ok(());
    }
    if app.whitelisted(gid, m.author.id, "automod").await? {
        return Ok(());
    }
    if mention_lock && lockdown::lock(app, gid, cfg.lockdown_prefs.kind()).await? {
        send_log(
            app,
            gid.get(),
            LogKind::Panic,
            Log::new("Server Locked", ALERT)
                .text(&format!("{YES} **The server has been locked down.**"))
                .user("Member", m.author.id.get())
                .line("Reason", "Mention spam"),
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
    crate::panic::note(app, gid, m.author.id, true).await?;
    if let Some(fc) = tripped_filter(&breakdown, f, bad_site) {
        if let Some(action) = fc.action {
            return filter_action(app, &cfg, gid, m, action, fc.duration_secs).await;
        }
    }
    let mut st = store::load_strikes(&app.pool, gid.get(), m.author.id.get()).await?;
    let act = strike(&mut st, &am.strike, now);
    store::save_strikes(&app.pool, gid.get(), m.author.id.get(), &st).await?;
    let tracked = app.heat.lock().unwrap().get_mut(&key).map(|(s, h)| {
        if act.reset_heat {
            s.value = 0.0;
        }
        h.take_msgs()
    });
    app.http.delete_message(m.channel_id, m.id).await.ok();
    if cfg.misc.purge_last_100 {
        reply::purge_user(app, m.channel_id, m.author.id).await.ok();
        let others: Vec<_> = tracked
            .unwrap_or_default()
            .into_iter()
            .filter(|&(c, _)| c != m.channel_id.get())
            .collect();
        reply::purge_ids(app, &others).await;
    }
    if am.strike.enabled {
        if cfg.misc.show_reason_on_auto {
            reply::dm(
                app,
                m.author.id,
                "You were timed out by automod: message spam",
            )
            .await;
        }
        timeout(&app.http, gid, m.author.id, act.timeout_ms / 1000).await?;
    }
    let title = format!(
        "{} has been silenced for {}!",
        m.author.name,
        duration(act.timeout_ms / 1000)
    );
    let log = Log::new(&title, ALERT)
        .thumbnail(&m.author)
        .user("Member", m.author.id.get())
        .line("Channel", &channel(m.channel_id.get()))
        .line("Strike", &format!("`{}`", act.strike))
        .field(
            "REASON:",
            if act.purge {
                "Message Spam (strike cap)"
            } else {
                "Message Spam"
            },
        );
    send_log(app, gid.get(), LogKind::AutoMod, log).await?;
    if lock.enabled
        && lock.strikes.is_some_and(|n| act.strike >= n)
        && lockdown::lock(app, gid, cfg.lockdown_prefs.kind()).await?
    {
        send_log(
            app,
            gid.get(),
            LogKind::Panic,
            Log::new("Server Locked", ALERT)
                .text(&format!("{YES} **The server has been locked down.**"))
                .user("Member", m.author.id.get())
                .line("Reason", &format!("Reached strike `{}`", act.strike)),
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

/// Heat weights with every disabled filter zeroed; anti spam off leaves only the link and word filters.
fn enabled_weights(am: &AutomodCfg) -> HeatCfg {
    let (f, mut h) = (&am.filters, am.heat.clone());
    let spam = am.anti_spam;
    let (pings, attach) = (spam && f.mentions.enabled, spam && f.attachments.enabled);
    for (on, w) in [
        (spam && f.regular.enabled, &mut h.base),
        (spam && f.similar.enabled, &mut h.similarity),
        (spam && f.emojis.enabled, &mut h.per_emoji),
        (spam && f.newlines.enabled, &mut h.per_newline),
        (spam && f.chars_upper.enabled, &mut h.per_upper),
        (spam && f.chars_lower.enabled, &mut h.per_lower),
        (spam && f.chars_lower.enabled, &mut h.per_char),
        (spam && f.zalgo.enabled, &mut h.zalgo),
        (spam && f.suspicion.enabled, &mut h.suspicious_account),
        (spam && f.inactive_channel.enabled, &mut h.inactive_channel),
        (pings && f.mention_users.enabled, &mut h.per_mention),
        (pings && f.mention_roles.enabled, &mut h.per_role_mention),
        (pings && f.mention_everyone.enabled, &mut h.per_everyone),
        (attach && f.embeds.enabled, &mut h.per_embed),
        (attach && f.images.enabled, &mut h.per_image),
        (attach && f.files.enabled, &mut h.per_file),
        (attach && f.attach_links.enabled, &mut h.per_link),
        (attach && f.stickers.enabled, &mut h.per_sticker),
    ] {
        if !on {
            *w = 0.0;
        }
    }
    h
}

/// The filter behind the biggest heat contribution.
fn tripped_filter<'a>(b: &Breakdown, f: &'a FiltersCfg, bad_site: bool) -> Option<&'a FilterCfg> {
    let (name, _) = b.parts.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    Some(match *name {
        "message" => &f.regular,
        "similarity" => &f.similar,
        "emojis" => &f.emojis,
        "newlines" => &f.newlines,
        "uppercase" => &f.chars_upper,
        "lowercase" | "characters" => &f.chars_lower,
        "zalgo" => &f.zalgo,
        "suspicious_account" => &f.suspicion,
        "inactive_channel" => &f.inactive_channel,
        "blacklisted_words" => &f.words.filter,
        "invites" => &f.invites,
        "malicious" if bad_site => &f.bad_sites,
        "malicious" => &f.scam_links,
        "nsfw" => &f.nsfw_links,
        "links_blacklist" => &f.links,
        "mentions" | "role_mentions" | "everyone" => &f.mentions,
        "images" | "files" | "embeds" | "links" | "stickers" => &f.attachments,
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
            warn_actions(app, cfg, gid, uid).await?;
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
        Log::new("Auto Mod Action", ORANGE)
            .user("Member", uid.get())
            .line("Action", &format!("`{action:?}`"))
            .line("Reason", "Filter"),
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
        // Hooks without an owner (integrations, channel follows) count as owner 0, never immune.
        let owner = h.user.as_ref().map(|u| u.id);
        if let Some(owner) = owner {
            if let Entry::Vacant(e) = immune.entry(owner.get()) {
                e.insert(app.is_immune(gid, owner).await?);
            }
        }
        // Snowflake timestamp: ms since the Discord epoch in the top bits.
        let created_at = ((h.id.get() >> 22) + 1_420_070_400_000) / 1000;
        hooks.push(Webhook {
            id: h.id.get(),
            owner: owner.map_or(0, |o| o.get()),
            created_at: created_at as i64,
        });
    }
    let cfg = app.cfg(gid.get()).await?;
    let doomed = webhooks_to_delete(
        &hooks,
        now_secs() - 600,
        |o| immune.get(&o).copied().unwrap_or(false),
        |id| cfg.whitelist.spam.webhooks.iter().any(|w| w.get() == id),
    );
    for id in &doomed {
        app.http.delete_webhook(Id::new(*id)).await.ok();
    }
    let log = Log::new("Webhooks Deleted", RED)
        .line("Count", &format!("`{}`", doomed.len()))
        .line("Reason", "Webhook spam");
    send_log(app, gid.get(), LogKind::AutoMod, log).await
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
        return punish(app, &cfg, gid, user.id.get(), action, "join lock").await;
    }
    let name = cure(&user.name);
    let j = Joiner {
        id: user.id.get(),
        name: &name,
        has_avatar: user.avatar.is_some(),
        is_bot: user.bot,
        bot_verified: user
            .public_flags
            .is_some_and(|f| f.contains(UserFlags::VERIFIED_BOT)),
        added_by_authorized: !user.bot || bot_added_by_authorized(app, gid, user.id).await,
    };
    let suspicious = is_suspicious(&j, &cfg.joingate, cfg.joingate.min_age_ms(), now);
    verify::on_join(app, gid, user.id, suspicious).await;
    if let Some(action) = harshest(&gate(&j, &cfg.joingate, now)) {
        punish(app, &cfg, gid, j.id, action, "join gate").await?;
    }
    if let Some(raid) = cfg.joinraid.as_ref().filter(|r| r.enabled) {
        join_raid(app, &cfg, gid, raid, &j, suspicious, now).await?;
    }
    Ok(())
}

async fn join_raid(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    raid: &RaidCfg,
    j: &Joiner<'_>,
    suspicious: bool,
    now: i64,
) -> Result<()> {
    let (out, started, names) = {
        let mut raids = app.raids.lock().unwrap();
        let d = raids.entry(gid.get()).or_default();
        let out = d.on_join(j, suspicious, raid, now);
        let names: Vec<String> = out
            .flagged
            .iter()
            .map(|id| d.name_of(*id).unwrap_or_default().to_string())
            .collect();
        (out, d.started, names)
    };
    if !out.flagged.is_empty() {
        let ids: Vec<i64> = out.flagged.iter().map(|&id| id as i64).collect();
        let logged = sqlx::query(
            "insert into raid_log (guild_id, raid_ms, user_id, name)
             select $1, $2, * from unnest($3::bigint[], $4::text[]) on conflict do nothing",
        )
        .bind(gid.get() as i64)
        .bind(started)
        .bind(&ids)
        .bind(&names)
        .execute(&app.pool)
        .await;
        if let Err(e) = logged {
            tracing::warn!(?e, "saving the raid log failed");
        }
    }
    if out.triggered {
        jobs::replace(
            &app.pool,
            gid.get() as i64,
            "raid_end",
            serde_json::json!({ "until_ms": now + raid.active_ms, "started_ms": started }),
            (now / 1000) + raid.active_ms / 1000,
        )
        .await?;
        let warned: Vec<_> = raid
            .warned_roles
            .iter()
            .filter_map(|r| r.parse().ok().map(Id::new))
            .collect();
        let mut log = Log::new("Join Raid Trigger!", ALERT)
            .text(&format!("{YES} A weird join pattern has been identified."))
            .text(&format!(
                "{ARROW} Flagged accounts joining for the next `{}` will be **{:?}**.",
                duration(raid.active_ms / 1000),
                raid.action
            ))
            .field(
                "Initial Flags:",
                // 40 IDs stay under Discord's 1024-character field limit; the page lists all
                &(out.flagged.iter().take(40).map(|u| format!("`{u}`")))
                    .chain(
                        (out.flagged.len() > 40)
                            .then(|| format!("+{} more", out.flagged.len() - 40)),
                    )
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        let base = std::env::var("DASHBOARD_URL").unwrap_or_default();
        if !base.is_empty() {
            log = log.text(&format!(
                "{ARROW} [Every caught account]({}/#/{gid}/raid_log)",
                base.trim_end_matches('/')
            ));
        }
        send_log_pinging(app, gid.get(), LogKind::JoinRaid, log, &warned).await?;
    }
    let auto_lock = cfg.automod.auto_lockdown.enabled;
    if let Some(kind) = (out.triggered && auto_lock)
        .then(|| raid.lock.clone())
        .flatten()
    {
        lockdown::lock(app, gid, kind).await?;
    }
    for id in out.flagged {
        punish(app, cfg, gid, id, raid.action, "join raid").await?;
    }
    Ok(())
}

/// Bot-side actions for a native rule; the rule's block-message event fires once per hit.
pub async fn on_native_hit(app: &App, e: &AutoModerationActionExecution) -> Result<()> {
    if e.action.kind != AutoModerationActionType::BlockMessage {
        return Ok(());
    }
    let gid = e.guild_id;
    let cfg = app.cfg(gid.get()).await?;
    let Some(actions) = cfg.discord_automod.actions.get(&e.rule_id.to_string()) else {
        return Ok(());
    };
    for &a in actions {
        if let Err(err) = native_action(app, &cfg, e, a).await {
            tracing::warn!(?err, ?a, "native rule action failed");
        }
    }
    Ok(())
}

async fn warn_actions(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
) -> Result<()> {
    let added = cfg.misc.warn_base_points;
    if let Some(text) = commands::apply_warn_actions(app, &cfg.misc, gid, uid, added).await? {
        let log = Log::new("Warn Action", ORANGE)
            .user("Member", uid.get())
            .line("Result", &text);
        send_log(app, gid.get(), LogKind::AutoMod, log).await?;
    }
    Ok(())
}

async fn native_action(
    app: &App,
    cfg: &GuildCfg,
    e: &AutoModerationActionExecution,
    action: BotAction,
) -> Result<()> {
    const WHY: &str = "discord automod rule";
    let (gid, uid) = (e.guild_id, e.user_id);
    match action {
        BotAction::Report => {
            let log = Log::new("Discord AutoMod Report", ORANGE)
                .user("Member", uid.get())
                .line("Rule", &format!("`{}`", e.rule_id))
                .text(&format!("{ARROW} **Content:**"))
                .diff(Some(&e.content), None);
            send_log(app, gid.get(), LogKind::AutoMod, log).await?;
        }
        BotAction::Warn => {
            cases::create(
                &app.pool,
                NewCase {
                    guild_id: gid.get() as i64,
                    kind: "warn",
                    user_id: uid.get() as i64,
                    mod_id: app.me.get() as i64,
                    reason: Some(WHY),
                    points: cfg.misc.warn_base_points,
                    duration_s: None,
                    inverse: None,
                },
            )
            .await?;
            warn_actions(app, cfg, gid, uid).await?;
        }
        BotAction::Timeout => timeout(&app.http, gid, uid, cfg.misc.timeout_secs() as i64).await?,
        BotAction::Kick => punish(app, cfg, gid, uid.get(), Action::Kick, WHY).await?,
        BotAction::Ban => punish(app, cfg, gid, uid.get(), Action::Ban, WHY).await?,
        BotAction::Lock => {
            lockdown::lock(app, gid, cfg.lockdown_prefs.kind()).await?;
        }
    }
    Ok(())
}

/// Custom plus unicode emoji; a ZWJ sequence counts per symbol.
fn count_emojis(s: &str) -> usize {
    let unicode = s
        .chars()
        .filter(|c| matches!(*c as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x2B50 | 0x2B55 | 0x231A..=0x231B | 0x23E9..=0x23F3))
        .count();
    unicode + s.matches("<:").count() + s.matches("<a:").count()
}

/// Names with fancy Unicode, lookalikes and leetspeak folded to plain letters, for the username filter.
fn cure(name: &str) -> String {
    decancer::cure!(name).map_or_else(|_| name.to_string(), |c| c.to_string())
}

/// Whether whoever added the bot is Owner/EO/TA; when the audit log has no entry the bot counts as authorized.
async fn bot_added_by_authorized(app: &App, gid: Id<GuildMarker>, bot: Id<UserMarker>) -> bool {
    match crate::welcome::bot_adder(app, gid, bot).await {
        Some(adder) => app.is_immune(gid, adder).await.unwrap_or(true),
        None => true,
    }
}

/// Re-runs the username filter when a member renames, if the gate asks for it.
pub async fn on_member_update(app: &App, m: &MemberUpdate) -> Result<()> {
    let cfg = app.cfg(m.guild_id.get()).await?;
    let g = &cfg.joingate;
    let Some(action) = g
        .username_filter
        .filter(|_| g.enabled && g.username_post_join)
    else {
        return Ok(());
    };
    let timed_out = m
        .communication_disabled_until
        .is_some_and(|t| t.as_secs() > now_secs());
    let names = [Some(m.user.name.as_str()), m.nick.as_deref()];
    let key = (m.guild_id.get(), m.user.id.get());
    let bad = names
        .into_iter()
        .flatten()
        .any(|n| g.name_matches(&cure(n)));
    let skip = {
        let mut seen = app.punished_names.lock().unwrap();
        if !bad {
            seen.remove(&key);
        }
        let names = format!("{}\0{}", m.user.name, m.nick.as_deref().unwrap_or_default());
        // A timeout fires another update and a role change keeps the name: act once per bad name pair.
        timed_out || !bad || seen.insert(key, names.clone()) == Some(names)
    };
    if skip {
        return Ok(());
    }
    punish(
        app,
        &cfg,
        m.guild_id,
        m.user.id.get(),
        action,
        "username filter",
    )
    .await
}

async fn punish(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: u64,
    action: Action,
    why: &str,
) -> Result<()> {
    let uid = Id::new(user);
    let punishing = !matches!(action, Action::Log | Action::Timeout(..=-1));
    if punishing && cfg.joingate.dm_punished {
        let text = if cfg.misc.show_reason_on_auto {
            format!("Automod action: {action:?} ({why})")
        } else {
            format!("Automod action: {action:?}")
        };
        let note = crate::appeals::note(&cfg.appeals, gid)
            .filter(|_| matches!(action, Action::Ban | Action::Timeout(_)));
        reply::dm(
            app,
            uid,
            &note.map_or(text.clone(), |n| format!("{text}\n{n}")),
        )
        .await;
    }
    match action {
        Action::Log => {}
        Action::Warn => {
            cases::create(
                &app.pool,
                NewCase {
                    guild_id: gid.get() as i64,
                    kind: "warn",
                    user_id: user as i64,
                    mod_id: app.me.get() as i64,
                    reason: Some(why),
                    points: cfg.misc.warn_base_points,
                    duration_s: None,
                    inverse: None,
                },
            )
            .await?;
            warn_actions(app, cfg, gid, uid).await?;
        }
        Action::Timeout(0) => timeout(&app.http, gid, uid, cfg.misc.timeout_secs() as i64).await?,
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
        Log::new("Join Gate", ORANGE)
            .user("Member", user)
            .line("Action", &format!("`{action:?}`"))
            .line("Reason", why),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_filters_lose_their_weight() {
        let mut am = AutomodCfg::default();
        am.filters.zalgo.enabled = false;
        am.filters.regular.enabled = false;
        am.filters.mentions.enabled = false;
        let h = enabled_weights(&am);
        assert_eq!((h.zalgo, h.base, h.per_everyone), (0.0, 0.0, 0.0));
        assert_eq!(h.per_emoji, HeatCfg::default().per_emoji);
        am.anti_spam = false;
        let h = enabled_weights(&am);
        assert_eq!((h.per_emoji, h.invite), (0.0, HeatCfg::default().invite));
    }

    #[test]
    fn emojis_count_unicode_and_custom() {
        assert_eq!(count_emojis("hi 😀🔥 <:a:1> <a:b:2> ok"), 4);
        assert_eq!(count_emojis("plain text"), 0);
    }

    #[test]
    fn cure_folds_fancy_names() {
        assert_eq!(cure("ⓗⓔⓛⓛⓞ"), "hello");
    }

    #[test]
    fn biggest_part_picks_the_filter() {
        let mut f = FiltersCfg::default();
        f.zalgo.action = Some(FilterAction::Ban);
        f.invites.action = Some(FilterAction::Kick);
        let pick = |parts| {
            tripped_filter(&Breakdown { parts }, &f, false)
                .unwrap()
                .action
        };
        assert_eq!(
            pick(vec![("message", 18.0), ("zalgo", 40.0)]),
            Some(FilterAction::Ban)
        );
        assert_eq!(
            pick(vec![("message", 18.0), ("invites", 100.0)]),
            Some(FilterAction::Kick)
        );
    }
}
