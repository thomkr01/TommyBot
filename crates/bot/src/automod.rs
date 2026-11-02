//! Message heat -> strikes -> timeouts, join gate and raid detection, mention-triggered lockdown.
use anyhow::Result;
use tommybot_automod::{
    filters,
    heat::{heat, Breakdown, HeatCfg, MsgFacts, Whitelist},
    join::{account_created_ms, gate, harshest, is_suspicious, Action, Filter, Joiner, RaidCfg},
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
    embed::{channel, clip, duration, ALERT, ARROW, DONE, ORANGE, RED, YES},
    Log, LogKind,
};
use twilight_model::{
    channel::Message,
    gateway::payload::incoming::{MemberAdd, MemberUpdate, MessageCreate},
    guild::Permissions,
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
    check(app, m, false).await
}

/// An edit can sneak in a link or word after the message passed, so only those filters run again.
pub async fn on_edit(app: &App, m: &Message) -> Result<()> {
    check(app, m, true).await
}

/// Heat parts that punish on their own; the only ones an edit can add.
const INSTANT: [&str; 5] = [
    "invites",
    "malicious",
    "nsfw",
    "links_blacklist",
    "blacklisted_words",
];

async fn check(app: &App, m: &Message, edit: bool) -> Result<()> {
    let Some(gid) = m.guild_id else { return Ok(()) };
    let cfg = app.cfg(gid.get()).await?;
    let am = &cfg.automod;
    // Other bots' slash command replies come through their application's webhook: not spam.
    let app_reply = m.interaction_metadata.is_some()
        || m.webhook_id
            .is_some_and(|w| m.application_id.is_some_and(|a| a.get() == w.get()));
    if !am.enabled || (m.webhook_id.is_some() && (!am.monitor_webhooks || app_reply)) {
        return Ok(());
    }
    if crate::panic::is_raider(app, gid, m.author.id) {
        crate::http_err::best_effort(
            app.http.delete_message(m.channel_id, m.id).await,
            "automod delete",
        );
        return timeout(
            &app.http,
            gid,
            m.author.id,
            am.strike.strike_timeout_ms / 1000,
            "Automod",
        )
        .await;
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
    let partner = wl.partner_channels.contains(&m.channel_id);
    // Members with dangerous permissions are spam-whitelisted unless only permits count as staff.
    let dangerous = !cfg.misc.staff_only_permitted && {
        const BITS: Permissions = Permissions::ADMINISTRATOR
            .union(Permissions::KICK_MEMBERS)
            .union(Permissions::BAN_MEMBERS)
            .union(Permissions::MANAGE_CHANNELS)
            .union(Permissions::MANAGE_GUILD)
            .union(Permissions::MANAGE_MESSAGES)
            .union(Permissions::MANAGE_ROLES)
            .union(Permissions::MANAGE_WEBHOOKS);
        // A failed lookup must not switch automod off for this message.
        app.guild_info(gid).await.is_ok_and(|info| {
            roles
                .iter()
                .any(|r| info.role_perms.get(r).is_some_and(|p| p & BITS.bits() != 0))
        })
    };
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
        channel_inactive: !edit
            && app
                .channels
                .lock()
                .unwrap()
                .entry(m.channel_id.get())
                .or_default()
                .on_message(now, f.inactive_messages, f.inactive_hours),
        invite_link: f.invites.enabled
            && [
                "discord.gg/",
                "discord.com/invite/",
                "discordapp.com/invite/",
                "discord.me/",
                "dsc.gg/",
            ]
            .iter()
            .any(|p| content.to_lowercase().contains(p)),
        malicious_link: bad_site || f.scam_links.enabled && filters::is_nitro_steam_scam(content),
        nsfw_link: f.nsfw_links.enabled && app.sites.nsfw(&checked),
        blacklisted_link: f.links.enabled
            && filters::any_on_domains(&hosts, &f.blacklisted_domains),
        blacklisted_words: if f.words.filter.enabled && !f.words.use_discord_automod {
            filters::word_hits(content, &blacklist_words(f)) + f.words.regex.hits(content)
        } else {
            0
        },
        zalgo: filters::is_zalgo(content),
        upper_chars,
        lower_chars,
        from_webhook: m.webhook_id.is_some(),
        // Wick's hidden factor: no avatar, or younger than the join gate's minimum age.
        suspicious_account: m.author.avatar.is_none()
            || now - account_created_ms(m.author.id.get()) < cfg.joingate.min_age_ms(),
        whitelist: Whitelist {
            spam: partner || dangerous || listed(&wl.spam),
            mention_spam: listed(&wl.mentions),
            advertising: partner || listed(&wl.invites),
            everyone: partner || listed(&wl.everyone),
        },
    };
    if facts.blacklisted_words > 0 && app.whitelisted(gid, m.author.id, "words").await? {
        facts.blacklisted_words = 0;
    }
    if facts.blacklisted_link && app.whitelisted(gid, m.author.id, "links").await? {
        facts.blacklisted_link = false;
    }
    // Mentions added by an edit ping nobody.
    let pings = if edit {
        0
    } else {
        facts.user_mentions + facts.everyone_mentions + facts.role_mentions
    };
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
    let over_hour_cap =
        am.anti_spam && f.mentions.enabled && !listed(&wl.mentions) && pings > 0 && {
            let mut map = app.pings.lock().unwrap();
            // Past 100k members, drop those with no ping in the hour window (they count 0 anyway);
            // everyone only if that frees nothing.
            if map.len() > 100_000 {
                map.retain(|_, w| w.last_at().is_some_and(|t| t > now - 3_600_000));
                if map.len() > 100_000 {
                    map.clear();
                }
            }
            map.entry((gid.get(), m.author.id.get()))
                .or_default()
                .on_mentions(pings, now, f.mention_hour_cap + 1, 3_600_000)
        };
    let key = (gid.get(), m.author.id.get());
    let heat_cfg = enabled_weights(am);
    let (triggered, fresh, breakdown) = {
        let mut map = app.heat.lock().unwrap();
        // Past 100k users, users idle for 10 minutes go first; everyone only if that frees nothing.
        if map.len() > 100_000 {
            map.retain(|_, (s, _)| s.at_ms > now - 600_000);
            if map.len() > 100_000 {
                map.clear();
            }
        }
        let (state, hist) = map.entry(key).or_default();
        // A quiet channel weighs on someone's first message there, not on each of their replies.
        let quiet_ms = i64::from(f.inactive_hours.max(1)) * 3_600_000;
        facts.channel_inactive &= !hist.posted_in(m.channel_id.get(), now - quiet_ms);
        let mut b = heat(&facts, hist, &heat_cfg);
        if over_hour_cap {
            b.parts.push(("mentions", heat_cfg.max));
        }
        if edit {
            b.parts.retain(|(n, _)| INSTANT.contains(n));
        } else {
            hist.remember(content);
            hist.track(m.channel_id.get(), m.id.get());
        }
        // An edit adding nothing must not trip on heat left over from earlier messages.
        let triggered = state.apply(&b, now, &heat_cfg) && !(edit && b.parts.is_empty());
        (triggered, triggered && state.claim(now), b)
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
    if let Some(hook) = m.webhook_id {
        if crate::webhooks::webhook_exempt(app, gid, hook, listed(&wl.spam)).await {
            return Ok(());
        }
    }
    if !fresh {
        // Sent before the punishment landed: remove it, the burst was already punished once.
        crate::http_err::best_effort(
            app.http.delete_message(m.channel_id, m.id).await,
            "automod delete",
        );
        return Ok(());
    }
    if m.webhook_id.is_some() {
        // A webhook is not a member: no strikes, remove its spam (the messages that built the heat
        // too, or the first ones stay) and the freshly made hooks.
        let tracked = app
            .heat
            .lock()
            .unwrap()
            .get_mut(&key)
            .map(|(_, h)| h.take_msgs());
        crate::http_err::best_effort(
            app.http.delete_message(m.channel_id, m.id).await,
            "automod delete",
        );
        reply::purge_ids(app, &tracked.unwrap_or_default()).await;
        return crate::webhooks::delete_new_webhooks(app, gid).await;
    }
    crate::panic::note(app, gid, m.author.id, true).await?;
    let tripped = tripped_filter(&breakdown, f, bad_site);
    if let Some((fc, factor)) = tripped {
        // Timeout without its own duration uses the Auto Timeouts, so it goes through the strikes.
        let own =
            |a: &FilterAction| *a != FilterAction::Timeout || fc.duration_secs.unwrap_or(0) > 0;
        if let Some(action) = fc.action.filter(own) {
            return filter_action(app, &cfg, gid, m, action, (factor, fc.duration_secs)).await;
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
    crate::http_err::best_effort(
        app.http.delete_message(m.channel_id, m.id).await,
        "automod delete",
    );
    if cfg.misc.purge_last_100 || act.purge {
        if let Err(e) = reply::purge_user(app, m.channel_id, m.author.id).await {
            tracing::warn!(?e, "automod purge failed");
        }
        let others: Vec<_> = tracked
            .unwrap_or_default()
            .into_iter()
            .filter(|&(c, _)| c != m.channel_id.get())
            .collect();
        reply::purge_ids(app, &others).await;
    }
    let reason = if act.purge {
        "Message spam (strike cap)"
    } else {
        tripped.map_or("Message spam", |t| t.1)
    };
    if am.strike.enabled {
        let why = if cfg.misc.show_reason_on_auto {
            format!(": {reason}")
        } else {
            String::new()
        };
        let note = crate::appeals::note(&cfg.appeals, &cfg.misc.ban_appeal_message, gid)
            .filter(|_| act.timeout_ms > 300_000)
            .map_or(String::new(), |n| format!("\n{n}"));
        if let Err(e) = timeout(
            &app.http,
            gid,
            m.author.id,
            act.timeout_ms / 1000,
            "Automod",
        )
        .await
        {
            action_failed(app, gid, m.author.id.get(), "Timeout", &e).await;
            return Err(e);
        }
        // Only after it worked: a failed timeout must not tell them they were timed out.
        reply::dm(
            app,
            m.author.id,
            &format!("You were timed out by automod{why}{note}"),
        )
        .await;
        auto_case(
            app,
            gid,
            m.author.id,
            "timeout",
            reason,
            Some(act.timeout_ms / 1000),
        )
        .await?;
    }
    let log = Log::new(&format!("*{} has been timed out!*", m.author.name), ORANGE)
        .line("Reason", &format!("*{reason}*"))
        .user("Member", m.author.id.get())
        .line("Channel", &channel(m.channel_id.get()))
        .line(
            "Duration",
            &format!("`{}`", duration(act.timeout_ms / 1000)),
        )
        .line("Strike", &format!("`{}`", act.strike));
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

/// The filter behind the biggest heat contribution, with Wick's "Factor" text for it.
fn tripped_filter<'a>(
    b: &Breakdown,
    f: &'a FiltersCfg,
    bad_site: bool,
) -> Option<(&'a FilterCfg, &'static str)> {
    let (name, _) = b.parts.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    Some(match *name {
        "message" => (&f.regular, "Message spam"),
        "similarity" => (&f.similar, "Spamming the same message"),
        "emojis" => (&f.emojis, "Emoji spam"),
        "newlines" => (&f.newlines, "Newline spam"),
        "uppercase" => (&f.chars_upper, "Excessive caps"),
        "lowercase" | "characters" => (&f.chars_lower, "Character spam"),
        "zalgo" => (&f.zalgo, "Use of zalgo text"),
        "suspicious_account" => (&f.suspicion, "Suspicious account"),
        "inactive_channel" => (&f.inactive_channel, "Spam in an inactive channel"),
        "blacklisted_words" => (&f.words.filter, "Use of blacklisted words"),
        "invites" => (&f.invites, "Posting an invite link"),
        "malicious" if bad_site => (&f.bad_sites, "Posting a malicious link"),
        "malicious" => (&f.scam_links, "Posting a scam link"),
        "nsfw" => (&f.nsfw_links, "Posting an NSFW link"),
        "links_blacklist" => (&f.links, "Posting a blacklisted link"),
        "mentions" | "role_mentions" | "everyone" => (&f.mentions, "Mass mentioning"),
        "images" | "files" | "embeds" | "links" | "stickers" => (&f.attachments, "Attachment spam"),
        _ => return None,
    })
}

/// The case an automod punishment leaves on the cases page, filed by the bot.
async fn auto_case(
    app: &App,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
    kind: &str,
    reason: &str,
    duration_s: Option<i64>,
) -> Result<()> {
    cases::create(
        &app.pool,
        NewCase {
            guild_id: gid.get() as i64,
            kind,
            user_id: uid.get() as i64,
            mod_id: app.me.get() as i64,
            reason: Some(reason),
            points: 0,
            duration_s,
            inverse: None,
            proof: None,
        },
    )
    .await?;
    Ok(())
}

/// A filter's own action replaces the strike system: no strike is recorded.
async fn filter_action(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    m: &Message,
    action: FilterAction,
    (factor, duration_secs): (&str, Option<i64>),
) -> Result<()> {
    let uid = m.author.id;
    crate::http_err::best_effort(
        app.http.delete_message(m.channel_id, m.id).await,
        "automod delete",
    );
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
                    proof: None,
                    inverse: None,
                },
            )
            .await?;
            // The threshold's timeout is the punishment; the strikes only keep count of it.
            if warn_actions(app, cfg, gid, uid).await? {
                let mut st = store::load_strikes(&app.pool, gid.get(), uid.get()).await?;
                strike(&mut st, &cfg.automod.strike, now_ms());
                store::save_strikes(&app.pool, gid.get(), uid.get(), &st).await?;
            }
        }
        FilterAction::Timeout | FilterAction::Kick | FilterAction::Ban => {
            let (kind, done, secs) = match action {
                FilterAction::Timeout => (
                    "timeout",
                    timeout(&app.http, gid, uid, secs, "Automod").await,
                    Some(secs),
                ),
                FilterAction::Kick => (
                    "kick",
                    app.http
                        .remove_guild_member(gid, uid)
                        .await
                        .map(drop)
                        .map_err(Into::into),
                    None,
                ),
                _ => (
                    "ban",
                    app.http
                        .create_ban(gid, uid)
                        .await
                        .map(drop)
                        .map_err(Into::into),
                    None,
                ),
            };
            if let Err(e) = done {
                action_failed(app, gid, uid.get(), kind, &e).await;
                return Err(e);
            }
            auto_case(app, gid, uid, kind, factor, secs).await?;
        }
    }
    send_log(
        app,
        gid.get(),
        LogKind::AutoMod,
        Log::new(&action_title(&m.author.name, action), action_color(action))
            .line("Factor", &format!("*{factor}*"))
            .line("Channel", &channel(m.channel_id.get()))
            .line("Message", &format!("||{}||", clip(&m.content, 500)))
            .user("Member", uid.get()),
    )
    .await
}

/// Wick's automod log title: "*name has been warned!*".
fn action_title(name: &str, action: FilterAction) -> String {
    match action {
        FilterAction::Delete => format!("*{name}'s message has been deleted!*"),
        FilterAction::Warn => format!("*{name} has been warned!*"),
        FilterAction::Timeout => format!("*{name} has been timed out!*"),
        FilterAction::Kick => format!("*{name} has been kicked!*"),
        FilterAction::Ban => format!("*{name} has been banned!*"),
    }
}

fn action_color(action: FilterAction) -> u32 {
    match action {
        FilterAction::Kick | FilterAction::Ban => RED,
        _ => ORANGE,
    }
}

/// Keeps the native Discord AutoMod keyword rule in step with the saved word lists.
/// Only runs while the guild opted in; turning it off is done from the rule list (`Api::set_rule_enabled`).
pub async fn sync_discord_rule(app: std::sync::Arc<App>, guild: u64) {
    let run = async {
        let cfg = app.cfg(guild).await?;
        let f = &cfg.automod.filters;
        if f.words.use_discord_automod {
            app.api
                .sync_keyword_rule(guild, &blacklist_words(f), f.words.regex.as_slice())
                .await?;
        }
        anyhow::Ok(())
    };
    if let Err(e) = run.await {
        tracing::warn!(?e, guild, "syncing the Discord AutoMod rule failed");
    }
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
    // One invites call per join: only when something shows the result.
    let invite = if cfg.joinraid.as_ref().is_some_and(|r| r.enabled)
        || cfg.logging.target(LogKind::Member, None).is_some()
    {
        crate::invites::used_invite(app, gid).await
    } else {
        None
    };
    if let Some((code, inviter)) = &invite {
        let log = Log::new("Invite Used", DONE)
            .user("Member", user.id.get())
            .line("Invite", &format!("`{code}`"))
            .line("Inviter", inviter);
        if let Err(e) = send_log(app, gid.get(), LogKind::Member, log).await {
            tracing::warn!(?e, "invite log failed");
        }
    }
    let name = cure(&user.name);
    // Without an audit entry the bot counts as authorised.
    let adder = if user.bot {
        crate::welcome::bot_adder(app, gid, user.id).await
    } else {
        None
    };
    let authorized = match adder {
        Some(a) => app.is_immune(gid, a).await.unwrap_or(true),
        None => true,
    };
    let j = Joiner {
        invite: invite.as_ref().map(|i| i.0.as_str()),
        id: user.id.get(),
        name: &name,
        has_avatar: user.avatar.is_some(),
        is_bot: user.bot,
        bot_verified: user
            .public_flags
            .is_some_and(|f| f.contains(UserFlags::VERIFIED_BOT)),
        added_by_authorized: authorized,
    };
    let suspicious = is_suspicious(&j, &cfg.joingate, cfg.joingate.min_age_ms(), now);
    verify::on_join(app, gid, user.id, suspicious, user.bot).await;
    let hits = gate(&j, &cfg.joingate, now);
    if let Some(action) = harshest(&hits) {
        let why = if hits.iter().any(|h| h.0 == Filter::AccountAge) {
            let min = duration(cfg.joingate.min_age_ms() / 1000);
            format!("join gate: accounts must be at least {min} old")
        } else {
            "join gate".into()
        };
        punish(app, &cfg, gid, j.id, action, &why).await?;
        // The bot filter also punishes whoever added it.
        if let (Some(adder), true) = (adder, hits.iter().any(|h| h.0 == Filter::UnauthorizedBot)) {
            punish(
                app,
                &cfg,
                gid,
                adder.get(),
                action,
                "adding a bot without permission",
            )
            .await?;
        }
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
    let (out, started, names, invites, code) = {
        let mut raids = app.raids.lock().unwrap();
        let d = raids.entry(gid.get()).or_default();
        let out = d.on_join(j, suspicious, raid, now);
        if out.triggered || d.code.is_empty() {
            d.code = crate::invites::raid_code(gid.get());
        }
        let names: Vec<String> = out
            .flagged
            .iter()
            .map(|id| d.name_of(*id).unwrap_or_default().to_string())
            .collect();
        let invites: Vec<String> = out
            .flagged
            .iter()
            .map(|id| d.invite_of(*id).unwrap_or_default().to_string())
            .collect();
        (out, d.started, names, invites, d.code.clone())
    };
    if !out.flagged.is_empty() {
        let ids: Vec<i64> = out.flagged.iter().map(|&id| id as i64).collect();
        let logged = sqlx::query(
            "insert into raid_log (guild_id, raid_ms, user_id, name, invite_code, code)
             select $1, $2, u, n, nullif(i, ''), $6
             from unnest($3::bigint[], $4::text[], $5::text[]) as t(u, n, i) on conflict do nothing",
        )
        .bind(gid.get() as i64)
        .bind(started)
        .bind(&ids)
        .bind(&names)
        .bind(&invites)
        .bind(&code)
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
            serde_json::json!({ "until_ms": now + raid.active_ms, "started_ms": started, "code": code }),
            (now / 1000) + raid.active_ms / 1000,
        )
        .await?;
        let warned: Vec<_> = raid
            .warned_roles
            .iter()
            .filter_map(|r| r.parse().ok().map(Id::new))
            .collect();
        let mut log = Log::new("🕵 Join Raid Trigger!", ALERT)
            .text(&format!(
                "{YES} TommyBot has identified a weird join pattern."
            ))
            .text(&format!(
                "{ARROW} Flagged accounts joining for the next `{}` will be **{}**.",
                duration(raid.active_ms / 1000),
                match raid.action {
                    Action::Log => "logged",
                    Action::Warn => "warned",
                    Action::Timeout(_) => "timed out",
                    Action::Kick => "kicked",
                    Action::Ban => "banned",
                }
            ))
            .text(&format!(
                "{ARROW} A full report will be posted once the duration ends."
            ))
            .field(
                "Initial Flags:",
                // 40 IDs stay under Discord's 1024-character field limit; the page lists all
                &(out
                    .flagged
                    .iter()
                    .zip(&names)
                    .take(40)
                    .map(|(u, n)| format!("`{} | {n}`", crate::invites::short_id(*u))))
                .chain(
                    (out.flagged.len() > 40).then(|| format!("+{} more", out.flagged.len() - 40)),
                )
                .collect::<Vec<_>>()
                .join("\n"),
            );
        log = log.field("Code:", &format!("`{code}`"));
        if let Some(url) = crate::invites::raid_url(&code) {
            log = log.text(&format!("{ARROW} [Every caught account]({url})"));
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

pub(crate) async fn warn_actions(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
) -> Result<bool> {
    let added = cfg.misc.warn_base_points;
    let Some((text, timed_out)) =
        commands::apply_warn_actions(app, &cfg.misc, gid, uid, added).await?
    else {
        return Ok(false);
    };
    let log = Log::new("Warn Action", ORANGE)
        .user("Member", uid.get())
        .line("Result", &text);
    send_log(app, gid.get(), LogKind::AutoMod, log).await?;
    Ok(timed_out)
}

/// Custom plus unicode emoji; a ZWJ sequence counts per symbol.
pub(crate) fn count_emojis(s: &str) -> usize {
    let unicode = s
        .chars()
        .filter(|c| matches!(*c as u32, 0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x2B50 | 0x2B55 | 0x231A..=0x231B | 0x23E9..=0x23F3))
        .count();
    unicode + s.matches("<:").count() + s.matches("<a:").count()
}

/// Names with fancy Unicode, lookalikes and leetspeak folded to plain letters, for the username filter.
pub(crate) fn cure(name: &str) -> String {
    decancer::cure!(name).map_or_else(|_| name.to_string(), |c| c.to_string())
}

/// Re-runs the username filter when a member renames, if the gate asks for it.
pub async fn on_member_update(app: &App, m: &MemberUpdate) -> Result<()> {
    let cfg = app.cfg(m.guild_id.get()).await?;
    let g = &cfg.joingate;
    let Some(action) = g
        .username_filter
        .get(Filter::UsernameFilter.default_action())
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

/// A punishment Discord refused (role too low, permission missing) is shown to staff, not
/// only written to the container log.
pub(crate) async fn action_failed(
    app: &App,
    gid: Id<GuildMarker>,
    user: u64,
    action: &str,
    e: &anyhow::Error,
) {
    let log = Log::new("Automod Action Failed", RED)
        .user("Member", user)
        .line("Action", &format!("`{action}`"))
        .line("Error", &crate::http_err::why(e));
    if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
        tracing::warn!(?e, "could not log the failed automod action");
    }
}

pub(crate) async fn punish(
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
        let mut text = if cfg.misc.show_reason_on_auto {
            format!("Automod action: {action:?} ({why})")
        } else {
            format!("Automod action: {action:?}")
        };
        if cfg.joingate.expose_min_age && why.starts_with("join gate") {
            let min = duration(cfg.joingate.min_age_ms() / 1000);
            text.push_str(&format!("\nMinimum account age: `{min}`"));
        }
        let note =
            crate::appeals::note(&cfg.appeals, &cfg.misc.ban_appeal_message, gid).filter(|_| {
                match action {
                    Action::Ban => true,
                    // Wick: short automatic timeouts (5 minutes or less) get no appeal link.
                    Action::Timeout(0) => cfg.misc.timeout_secs() > 300,
                    Action::Timeout(ms) => ms > 300_000,
                    _ => false,
                }
            });
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
                    proof: None,
                    inverse: None,
                },
            )
            .await?;
            warn_actions(app, cfg, gid, uid).await?;
        }
        Action::Timeout(..=-1) => return Ok(()),
        Action::Timeout(_) | Action::Kick | Action::Ban => {
            let done = match action {
                Action::Timeout(ms) => {
                    let secs = match ms {
                        0 => cfg.misc.timeout_secs() as i64,
                        ms => ms / 1000,
                    };
                    timeout(&app.http, gid, uid, secs, "Automod").await
                }
                Action::Kick => app
                    .http
                    .remove_guild_member(gid, uid)
                    .await
                    .map(drop)
                    .map_err(Into::into),
                _ => app
                    .http
                    .create_ban(gid, uid)
                    .await
                    .map(drop)
                    .map_err(Into::into),
            };
            if let Err(e) = done {
                action_failed(app, gid, user, &format!("{action:?}"), &e).await;
                return Err(e);
            }
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
                .0
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
