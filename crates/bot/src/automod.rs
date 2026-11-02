//! Message heat -> strikes -> timeouts, mention-triggered lockdown.
use anyhow::Result;
use tommybot_automod::{
    filters,
    heat::{heat, Breakdown, HeatCfg, MsgFacts, Post, Whitelist},
    join::account_created_ms,
    store,
    strike::strike,
};
use tommybot_core::{
    cases::{self, NewCase},
    sections::{AutomodCfg, FilterAction, FilterCfg, FiltersCfg, WhitelistList},
};
use tommybot_logging::{
    embed::{channel, clip, duration, ALERT, ORANGE, RED, YES},
    Log, LogKind,
};
use twilight_model::{
    channel::Message,
    gateway::payload::incoming::MessageCreate,
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{App, GuildCfg},
    commands::{self, timeout},
    events::send_log,
    lockdown, reply,
};

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub async fn on_message(app: &App, m: &MessageCreate) -> Result<()> {
    match as_app_user(m) {
        Some(m) => check(app, &m, true).await,
        None => check(app, m, false).await,
    }
}

/// A reply from an app the member installed on their own account says what the member wants it to,
/// so it gets the instant filters (as on an edit) under the member's name. Server-installed bots stay
/// exempt.
fn as_app_user(m: &Message) -> Option<Message> {
    let meta = m.interaction_metadata.as_deref()?;
    let owners = &meta.authorizing_integration_owners;
    (owners.user.is_some() && owners.guild.is_none()).then(|| Message {
        author: meta.user.clone(),
        member: None,
        webhook_id: None,
        interaction_metadata: None,
        ..m.clone()
    })
}

/// An edit can sneak in a link or word after the message passed, so only those filters run again.
pub async fn on_edit(app: &App, m: &Message) -> Result<()> {
    check(app, as_app_user(m).as_ref().unwrap_or(m), true).await
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
    // Server-installed apps' replies are not spam (user-installed ones were rewritten above).
    let app_reply = m.interaction_metadata.is_some();
    if !am.enabled || app_reply || (m.webhook_id.is_some() && !am.monitor_webhooks) {
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
    // In a thread or forum post the whitelists of its parent channel apply.
    let (parent, category) = app
        .channel_scope(gid, m.channel_id)
        .await
        .unwrap_or((m.channel_id, None));
    let listed = |l: &WhitelistList| l.covers(m.author.id, roles, parent, category, m.webhook_id);
    let partner = wl.partner_channels.contains(&parent);
    // Members with dangerous permissions are spam-whitelisted unless only permits count as staff.
    let dangerous = app.staff_by_perms(&cfg, gid, roles).await;
    let now = now_ms();
    let text = filter_text(m);
    let content = text.as_str();
    let f = &am.filters;
    let hosts = filters::hosts(content);
    let mut checked = hosts.clone();
    if f.nsfw_crawl_shorteners || f.invites.enabled {
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
        invite_link: f.invites.enabled && is_invite(content, &checked),
        malicious_link: bad_site || f.scam_links.enabled && filters::is_nitro_steam_scam(content),
        nsfw_link: f.nsfw_links.enabled && app.sites.nsfw(&checked),
        blacklisted_link: f.links.enabled
            && filters::any_on_domains(&hosts, &f.blacklisted_domains),
        blacklisted_words: if f.words.filter.enabled && !f.words.use_discord_automod {
            let forms = filters::word_forms(content);
            filters::word_hits(&forms, &blacklist_words(f)) + f.words.regex.hits(&forms)
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
    let (triggered, fresh, breakdown, crowd) = {
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
        // The same text from several members: their heat is pooled (webhooks have their own).
        let crowd =
            (!edit && heat_cfg.similarity > 0.0 && !facts.whitelist.spam && m.webhook_id.is_none())
                .then(|| {
                    let post = Post {
                        user: m.author.id.get(),
                        name: m.author.name.clone(),
                        channel: m.channel_id.get(),
                        msg: m.id.get(),
                    };
                    app.crowd
                        .lock()
                        .unwrap()
                        .on_text(gid.get(), post, content, &b, now, &heat_cfg)
                })
                .flatten();
        if crowd.is_some() {
            b.parts.push(("similarity", heat_cfg.max));
        }
        if edit {
            b.parts.retain(|(n, _)| INSTANT.contains(n));
        } else {
            hist.remember(content);
            hist.track(m.channel_id.get(), m.id.get());
        }
        // An edit adding nothing must not trip on heat left over from earlier messages.
        let triggered = state.apply(&b, now, &heat_cfg) && !(edit && b.parts.is_empty());
        (triggered, triggered && state.claim(now), b, crowd)
    };
    if let Some(posts) = &crowd {
        punish_crowd(app, &cfg, gid, m, posts, now).await;
    }
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
    crate::http_err::best_effort(
        app.http.delete_message(m.channel_id, m.id).await,
        "automod delete",
    );
    strike_member(
        app,
        &cfg,
        gid,
        (m.author.id, &m.author.name),
        m.channel_id,
        tripped
            .map(|t| t.1)
            .or(crowd.is_some().then_some("Coordinated spam")),
    )
    .await
}

/// Everyone else who sent the crowd's text gets the same punishment as the member who tripped it.
async fn punish_crowd(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    m: &Message,
    posts: &[Post],
    now: i64,
) {
    let msgs: Vec<_> = posts
        .iter()
        .filter(|p| p.msg != m.id.get())
        .map(|p| (p.channel, p.msg))
        .collect();
    reply::purge_ids(app, &msgs).await;
    let mut done = vec![m.author.id.get()];
    for p in posts {
        if done.contains(&p.user) {
            continue;
        }
        done.push(p.user);
        let user = Id::new(p.user);
        // Not twice for one burst if their own heat tripped too.
        let fresh = app
            .heat
            .lock()
            .unwrap()
            .entry((gid.get(), p.user))
            .or_default()
            .0
            .claim(now);
        let res = async {
            if !fresh || app.whitelisted(gid, user, "automod").await? {
                return Ok(());
            }
            crate::panic::note(app, gid, user, true).await?;
            strike_member(
                app,
                cfg,
                gid,
                (user, &p.name),
                Id::new(p.channel),
                Some("Coordinated spam"),
            )
            .await
        };
        if let Err(e) = res.await {
            tracing::warn!(?e, user = p.user, "punishing a crowd member failed");
        }
    }
}

/// Strike and time out a spammer as the strike settings say, then log it.
async fn strike_member(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    (user, name): (Id<UserMarker>, &str),
    ch: Id<ChannelMarker>,
    reason: Option<&str>,
) -> Result<()> {
    let am = &cfg.automod;
    let lock = &am.auto_lockdown;
    let now = now_ms();
    let key = (gid.get(), user.get());
    let mut st = store::load_strikes(&app.pool, gid.get(), user.get()).await?;
    let act = strike(&mut st, &am.strike, now);
    store::save_strikes(&app.pool, gid.get(), user.get(), &st).await?;
    let tracked = app.heat.lock().unwrap().get_mut(&key).map(|(s, h)| {
        if act.reset_heat {
            s.value = 0.0;
        }
        h.take_msgs()
    });
    if cfg.misc.purge_last_100 || act.purge {
        if let Err(e) = reply::purge_user(app, ch, user).await {
            tracing::warn!(?e, "automod purge failed");
        }
        let others: Vec<_> = tracked
            .unwrap_or_default()
            .into_iter()
            .filter(|&(c, _)| c != ch.get())
            .collect();
        reply::purge_ids(app, &others).await;
    }
    let reason = if act.purge {
        "Message spam (strike cap)"
    } else {
        reason.unwrap_or("Message spam")
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
        if let Err(e) = timeout(&app.http, gid, user, act.timeout_ms / 1000, "Automod").await {
            action_failed(app, gid, user.get(), "Timeout", &e).await;
            return Err(e);
        }
        // Only after it worked: a failed timeout must not tell them they were timed out.
        reply::dm(
            app,
            user,
            &format!("You were timed out by automod{why}{note}"),
        )
        .await;
        auto_case(
            app,
            gid,
            user,
            "timeout",
            reason,
            Some(act.timeout_ms / 1000),
        )
        .await?;
    }
    let log = Log::new(&format!("*{} has been timed out!*", name), ORANGE)
        .line("Reason", &format!("*{reason}*"))
        .user("Member", user.get())
        .line("Channel", &channel(ch.get()))
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
                .user("Member", user.get())
                .line("Reason", &format!("Reached strike `{}`", act.strike)),
        )
        .await?;
    }
    Ok(())
}

/// The text the filters read: the message, what it forwards, and a poll's question and answers.
fn filter_text(m: &Message) -> String {
    let poll = m.poll.iter().flat_map(|p| {
        std::iter::once(&p.question)
            .chain(p.answers.iter().map(|a| &a.poll_media))
            .filter_map(|q| q.text.as_deref())
    });
    std::iter::once(m.content.as_str())
        .chain(
            m.message_snapshots
                .iter()
                .map(|s| s.message.content.as_str()),
        )
        .chain(poll)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

const INVITE_PATHS: [&str; 7] = [
    "discord.gg/",
    "discord.com/invite/",
    "discordapp.com/invite/",
    "discord.me/",
    "dsc.gg/",
    "discord.io/",
    "invite.gg/",
];

/// An invite in the text (hidden characters ignored), or a shortener that lands on discord.gg.
fn is_invite(text: &str, landed: &[String]) -> bool {
    let lower = filters::strip_invisible(text).to_lowercase();
    INVITE_PATHS.iter().any(|p| lower.contains(p))
        || landed.iter().any(|h| h == "discord.gg" || h == "dsc.gg")
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
pub(crate) fn enabled_weights(am: &AutomodCfg) -> HeatCfg {
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
pub(crate) async fn auto_case(
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
            let (kind, timed) = match action {
                FilterAction::Timeout => ("timeout", Some(secs)),
                FilterAction::Kick => ("kick", None),
                _ => ("ban", None),
            };
            let done = match action {
                FilterAction::Timeout => timeout(&app.http, gid, uid, secs, "Automod").await,
                FilterAction::Kick => app
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
                action_failed(app, gid, uid.get(), kind, &e).await;
                return Err(e);
            }
            auto_case(app, gid, uid, kind, factor, timed).await?;
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
