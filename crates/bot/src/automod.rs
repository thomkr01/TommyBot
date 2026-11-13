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
    application::interaction::InteractionType,
    channel::Message,
    gateway::payload::incoming::MessageCreate,
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{App, GuildCfg},
    commands::{self, timeout},
    events::send_log,
    lockdown, reply,
};

#[path = "automod_parts.rs"]
mod parts;
use parts::app_text;
pub(crate) use parts::{attachments, files_can_be_new, Seen};

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub async fn on_message(app: &App, m: &MessageCreate) -> Result<()> {
    if app_message(m) {
        new_text(m);
    }
    match as_app_user(m) {
        Some(m) => check(app, &m, true, false).await,
        None => check(app, m, false, false).await,
    }
}

/// A reply from an app the member installed on their own account says what the member wants it to,
/// so it gets the instant filters (as on an edit) under the member's name. Server-installed bots stay
/// exempt. A reply to a button click is not the clicker's words: it keeps the app as author and its
/// metadata, so the filters only delete it (see [`clicked`]).
pub(crate) fn as_app_user(m: &Message) -> Option<Message> {
    let meta = m.interaction_metadata.as_deref()?;
    let owners = &meta.authorizing_integration_owners;
    if owners.user.is_none() || owners.guild.is_some() {
        return None;
    }
    Some(if clicked(m) {
        Message {
            member: None,
            webhook_id: None,
            ..m.clone()
        }
    } else {
        Message {
            author: meta.user.clone(),
            member: None,
            webhook_id: None,
            interaction_metadata: None,
            ..m.clone()
        }
    })
}

/// Sent or updated because someone clicked a button or menu: whoever clicked did not write it.
pub(crate) fn clicked(m: &Message) -> bool {
    m.interaction_metadata
        .as_ref()
        .is_some_and(|i| i.kind == InteractionType::MessageComponent)
}

/// Sent by a webhook or an app (the caller leaves out TommyBot's own).
pub(crate) fn app_message(m: &Message) -> bool {
    m.webhook_id.is_some() || m.application_id.is_some() || m.interaction_metadata.is_some()
}

/// Remembers the text a webhook or app message was filtered on (content and embed text); true when
/// it is new for that message, so an update that changes nothing is not filtered twice.
pub(crate) fn new_text(m: &Message) -> bool {
    use std::hash::BuildHasher;
    static READ: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u64, u64>>> =
        std::sync::LazyLock::new(Default::default);
    // Random keys per process: a fixed-key hash can be collided offline to pass a bad edit.
    static KEYS: std::sync::LazyLock<std::collections::hash_map::RandomState> =
        std::sync::LazyLock::new(Default::default);
    // A file added with the same text (an image-only fill) counts too: its scan and QR must run.
    let files: Vec<u64> = attachments(m).map(|a| a.id.get()).collect();
    let h = KEYS.hash_one((filter_text(m), app_text(m), files));
    let mut read = READ.lock().unwrap();
    // shortcut: past the cap every message is forgotten; its next update is filtered once more.
    if read.len() >= 20_000 {
        read.clear();
    }
    read.insert(m.id.get(), h) != Some(h)
}

/// An edit can sneak in a link or word after the message passed, so only those filters run again.
pub async fn on_edit(app: &App, m: &Message) -> Result<()> {
    check(app, as_app_user(m).as_ref().unwrap_or(m), true, false).await
}

/// The image scan found an NSFW image in `m`: it runs through the filters like an edit that adds only
/// that, so whitelists, the filter's action or the strike system, the delete and the log all apply.
pub async fn on_nsfw_image(app: &App, m: &Message) -> Result<()> {
    check(app, m, true, true).await
}

/// Heat parts that punish on their own; the only ones an edit can add.
const INSTANT: [&str; 6] = [
    "invites",
    "malicious",
    "nsfw",
    "nsfw_image",
    "links_blacklist",
    "blacklisted_words",
];

/// Instant filters the automod permit does not exempt from: scam, malicious and NSFW links and images.
const UNEXEMPT: [&str; 3] = ["malicious", "nsfw", "nsfw_image"];

/// `nsfw_image`: called by the image scan with a hit; only that part counts then.
async fn check(app: &App, m: &Message, edit: bool, nsfw_image: bool) -> Result<()> {
    let mut slow = None;
    let r = check_message(app, m, edit, nsfw_image, &mut slow).await;
    // After the punishment: a slowmode start (several Discord calls) must not hold it back.
    if let (Some(gid), Some(cfg)) = (m.guild_id, slow) {
        crate::slowmode::start(app, gid, m.channel_id, &cfg).await;
    }
    r
}

/// `slow` gets the auto slowmode settings when this message tripped one.
async fn check_message(
    app: &App,
    m: &Message,
    edit: bool,
    nsfw_image: bool,
    slow: &mut Option<tommybot_automod::heat::AutoSlowmodeCfg>,
) -> Result<()> {
    let Some(gid) = m.guild_id else { return Ok(()) };
    let cfg = app.cfg(gid.get()).await?;
    let am = &cfg.automod;
    // Heat panic's raiders are held even with automod off, also when a server app posts for them.
    // Not for a button click's message: a held raider clicking a shared panel must not delete it.
    let poster = m
        .interaction_metadata
        .as_ref()
        .filter(|_| !clicked(m))
        .map_or(m.author.id, |i| i.user.id);
    if crate::panic::is_raider(app, gid, poster) {
        crate::http_err::best_effort(
            app.http.delete_message(m.channel_id, m.id).await,
            "automod delete",
        );
        return timeout(
            &app.http,
            gid,
            poster,
            am.strike.strike_timeout_ms / 1000,
            "Automod",
        )
        .await;
    }
    if !am.enabled || (m.webhook_id.is_some() && !am.monitor_webhooks) {
        return Ok(());
    }
    // A user-installed app's reply runs as an edit (instant filters only), but its files are new.
    let new_files = !nsfw_image && files_can_be_new(m);
    if !nsfw_image && am.filters.nsfw_images.enabled {
        app.nsfw.enqueue(m, !new_files, false);
    }
    // Server-installed apps' replies are not spam (user-installed ones were rewritten above); the
    // image scan above only logs theirs. A user-installed app's click reply kept its metadata.
    let click = clicked(m);
    if m.interaction_metadata.is_some() && !(click && as_app_user(m).is_some()) {
        return Ok(());
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
    // Embed and button text only reach the link and word filters: their length or caps are no spam.
    let linked = [content, &app_text(m)].join("\n");
    let f = &am.filters;
    let hosts = filters::hosts(&linked);
    let mut checked = hosts.clone();
    // Every filter that reads where a shortener points.
    if f.nsfw_crawl_shorteners || f.invites.enabled || f.bad_sites.enabled || f.nsfw_links.enabled {
        for u in filters::shortener_urls(&linked).into_iter().take(3) {
            checked.extend(app.sites.unshorten(u).await);
        }
    }
    let bad_site = f.bad_sites.enabled && app.sites.phishing(&checked);
    let (upper_chars, lower_chars) = filters::case_counts(content);
    let images = attachments(m)
        .filter(|a| {
            a.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
        })
        .count();
    // Edits cannot add attachments, so images are only read once.
    let qr_scam = new_files && images > 0 && (f.scam_links.enabled || f.bad_sites.enabled) && {
        let qr = crate::qr::texts(app, m.id.get(), attachments(m)).await;
        filters::is_discord_login(&qr)
            || f.bad_sites.enabled && app.sites.phishing(&filters::hosts(&qr))
            || f.scam_links.enabled && filters::is_nitro_steam_scam(&qr)
    };
    let mut facts = MsgFacts {
        content,
        emojis: count_emojis(content),
        user_mentions: m.mentions.len(),
        everyone_mentions: usize::from(m.mention_everyone),
        role_mentions: m.mention_roles.len(),
        images,
        files: attachments(m).count() - images,
        embeds: m.embeds.len(),
        links: filters::hosts(content).len(),
        stickers: m.sticker_items.len(),
        channel_inactive: !edit
            && app
                .channels
                .lock()
                .unwrap()
                .entry(m.channel_id.get())
                .or_default()
                .on_message(now, f.inactive_messages, f.inactive_hours),
        invite_link: f.invites.enabled && is_invite(&linked, &checked),
        malicious_link: bad_site
            || qr_scam
            || f.scam_links.enabled && filters::is_nitro_steam_scam(&linked),
        nsfw_link: f.nsfw_links.enabled && app.sites.nsfw(&checked),
        nsfw_image: nsfw_image && f.nsfw_images.enabled,
        blacklisted_link: f.links.enabled
            && filters::any_on_domains(&hosts, &f.blacklisted_domains),
        blacklisted_words: if f.words.filter.enabled {
            local_word_hits(gid.get(), f, &filters::word_forms(&linked))
        } else {
            0
        },
        zalgo: filters::is_zalgo(content),
        upper_chars,
        lower_chars,
        from_webhook: m.webhook_id.is_some(),
        // Hidden factor: no avatar, or younger than the join gate's minimum age.
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
    // Capped per member only where the mentions filter punishes one member's mass mention.
    let punished = am.anti_spam && f.mentions.enabled && !listed(&wl.mentions);
    let lock = &am.auto_lockdown;
    // Whitelisted pings never count towards the mention lockdown.
    let w = facts.whitelist;
    let lock_pings = if edit {
        0
    } else {
        usize::from(!w.mention_spam) * (facts.user_mentions + facts.role_mentions)
            + usize::from(!w.everyone) * facts.everyone_mentions
    };
    let mention_lock = lock.enabled
        && lock.mentions
        && lock_pings > 0
        && !app.whitelisted(gid, m.author.id, "automod").await?
        && app
            .mentions
            .lock()
            .unwrap()
            .entry(gid.get())
            .or_default()
            .on_mentions(
                m.author.id.get(),
                punished,
                lock_pings,
                now,
                lock.mention_count,
                lock.mention_window_ms,
            );
    let over_hour_cap = punished && pings > 0 && {
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
    let (mut triggered, fresh, mut breakdown, crowd) = {
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
        if nsfw_image {
            b.parts.retain(|(n, _)| *n == "nsfw_image");
        } else if edit {
            b.parts.retain(|(n, _)| INSTANT.contains(n));
        } else {
            hist.remember(content);
            hist.track(m.channel_id.get(), m.id.get());
        }
        // An edit adding nothing must not trip on heat left over from earlier messages.
        let triggered = state.apply(&b, now, &heat_cfg) && !(edit && b.parts.is_empty());
        (triggered, triggered && state.claim(now), b, crowd)
    };
    // Threads and spam-whitelisted channels or members never count towards an auto slowmode.
    if !edit && !facts.whitelist.spam && parent == m.channel_id {
        *slow = crate::slowmode::feed(app, am, gid, m, &breakdown, &heat_cfg, now).await;
    }
    if let Some(posts) = &crowd {
        punish_crowd(app, &cfg, gid, m, posts, now).await;
    }
    if !triggered && !mention_lock {
        return Ok(());
    }
    // The automod permit skips spam heat, not a scam or NSFW post: a hacked trusted account sends those.
    if triggered && app.whitelisted(gid, m.author.id, "automod").await? {
        breakdown.parts.retain(|(n, _)| UNEXEMPT.contains(n));
        triggered = breakdown.total() >= heat_cfg.max;
    }
    // A tripped message with a scam link or QR code is deleted below, whichever path punishes.
    if triggered && facts.malicious_link && m.webhook_id.is_none() && !click {
        crate::raiders::scam_hit(app, gid, m.author.id, m.id).await;
    }
    // A failed lock or log must not keep the spammer from being punished below.
    let locked = if mention_lock {
        lockdown::lock(app, gid, cfg.lockdown_prefs.kind())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(?e, "mention spam lockdown failed");
                false
            })
    } else {
        false
    };
    if locked {
        let log = Log::new("Server Locked", ALERT)
            .text(&format!("{YES} **The server has been locked down.**"))
            .user("Member", m.author.id.get())
            .line("Reason", "Mention spam");
        if let Err(e) = send_log(app, gid.get(), LogKind::Panic, log).await {
            tracing::warn!(?e, "lockdown log failed");
        }
    }
    if !triggered {
        return Ok(());
    }
    if click {
        // Nobody wrote it whom a strike could reach: delete and log only.
        crate::http_err::best_effort(
            app.http.delete_message(m.channel_id, m.id).await,
            "automod delete",
        );
        let factor = tripped_filter(&breakdown, f, bad_site).map_or("Automod", |t| t.1);
        let what = (m.channel_id.get(), m.content.as_str());
        let log = filter_log(
            &m.author.name,
            FilterAction::Delete,
            factor,
            what,
            m.author.id.get(),
        );
        if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
            tracing::warn!(?e, "automod log failed");
        }
        return Ok(());
    }
    if let Some(hook) = m.webhook_id {
        if crate::webhooks::webhook_exempt(app, gid, hook, listed(&wl.spam)).await {
            // The exemption (PluralKit, Tupperbox) is for spam heat: a relayed scam, NSFW post,
            // invite or blacklisted word is still deleted.
            if breakdown.parts.iter().any(|(n, _)| INSTANT.contains(n)) {
                crate::http_err::best_effort(
                    app.http.delete_message(m.channel_id, m.id).await,
                    "automod delete",
                );
            }
            return Ok(());
        }
    }
    let tripped = tripped_filter(&breakdown, f, bad_site);
    let own = tripped.and_then(|(fc, factor)| own_action(fc, am).map(|a| (a, factor)));
    // A filter with its own action (a Ban for scam or NSFW) is no part of a heat burst, so the
    // grace window does not soften it. One message is still punished once.
    let instant = own.is_some() && top_part(&breakdown).is_some_and(|n| INSTANT.contains(&n));
    if ACTED.contains((m.id.get(), m.author.id.get())) || !fresh && !instant {
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
        reply::purge_ids(
            app,
            &with_msg(tracked, Some((m.channel_id.get(), m.id.get()))),
        )
        .await;
        return crate::webhooks::delete_new_webhooks(app, gid).await;
    }
    crate::panic::note_heat(app, gid, m.author.id).await;
    let r = match own {
        Some(((action, secs), factor)) => {
            filter_action(app, &cfg, gid, m, action, (factor, secs)).await
        }
        None => {
            strike_member(
                app,
                &cfg,
                gid,
                (m.author.id, &m.author.name),
                m.channel_id,
                Some(m.id),
                tripped
                    .map(|t| t.1)
                    .or(crowd.is_some().then_some("Coordinated spam")),
            )
            .await
        }
    };
    // Landed (also when a step after the punishment failed): a later pass over it only deletes.
    if r.as_ref()
        .err()
        .is_none_or(|e| e.downcast_ref::<Punished>().is_some())
    {
        ACTED.first((m.id.get(), m.author.id.get()));
    }
    r
}

/// Messages automod punished their author for, keyed (message, author): the NSFW scan, a preview update or
/// an edit of the same message must not punish it again, also past the grace window.
static ACTED: std::sync::LazyLock<Seen> = std::sync::LazyLock::new(Default::default);

/// A filter's own action and its timeout length, or None when the strikes decide. A Timeout
/// without its own duration uses the Auto Timeouts; with those off, the strike timeout length.
fn own_action(fc: &FilterCfg, am: &AutomodCfg) -> Option<(FilterAction, Option<i64>)> {
    let action = fc.action?;
    let secs = fc.duration_secs.filter(|&s| s > 0);
    match action {
        FilterAction::Timeout if secs.is_none() && am.strike.enabled => None,
        FilterAction::Timeout => Some((action, secs.or(Some(am.strike.strike_timeout_ms / 1000)))),
        _ => Some((action, secs)),
    }
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
            crate::panic::note_heat(app, gid, user).await;
            strike_member(
                app,
                cfg,
                gid,
                (user, &p.name),
                Id::new(p.channel),
                None,
                Some("Coordinated spam"),
            )
            .await
        };
        if let Err(e) = res.await {
            tracing::warn!(?e, user = p.user, "punishing a crowd member failed");
        }
    }
    crate::raiders::spam_crowd(app, gid, &done, m.id).await;
}

/// Strike and time out a spammer as the strike settings say, then log it.
async fn strike_member(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    (user, name): (Id<UserMarker>, &str),
    ch: Id<ChannelMarker>,
    msg: Option<Id<MessageMarker>>,
    reason: Option<&str>,
) -> Result<()> {
    let am = &cfg.automod;
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
    // Automod deletes every message that built the heat, so they go in one bulk delete per channel.
    let mut msgs = with_msg(tracked, msg.map(|id| (ch.get(), id.get())));
    if cfg.misc.purge_last_100 || act.purge {
        if let Err(e) = reply::purge_user(app, ch, user).await {
            tracing::warn!(?e, "automod purge failed");
        }
        msgs.retain(|&(c, _)| c != ch.get());
    }
    reply::purge_ids(app, &msgs).await;
    let reason = if act.purge {
        "Message spam (strike cap)"
    } else {
        reason.unwrap_or("Message spam")
    };
    // A failed timeout or case must not skip the strike-cap lockdown; a failed timeout is returned last.
    let mut failed = None;
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
            failed = Some(e);
        } else {
            // Only after it worked: a failed timeout must not tell them they were timed out.
            reply::dm(
                app,
                user,
                &format!("You were timed out by automod{why}{note}"),
            )
            .await;
            let secs = Some(act.timeout_ms / 1000);
            if let Err(e) = auto_case(app, gid, user, "timeout", reason, secs).await {
                tracing::warn!(?e, "automod case failed");
            }
        }
    }
    // A failed timeout was logged as failed above, not as done.
    if failed.is_none() {
        let log = heat_log(
            am.strike.enabled,
            name,
            reason,
            (user.get(), ch.get()),
            &act,
        );
        if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
            tracing::warn!(?e, "automod log failed");
        }
    }
    let locked = strike_cap_lock(app, cfg, gid, user, act.strike).await;
    // The strike is saved: whatever failed since, this pass punished.
    match failed {
        // The failed timeout is what the caller must see; the lockdown error is only logged then.
        Some(e) => {
            if let Err(l) = locked {
                tracing::warn!(?l, "strike-cap lockdown failed");
            }
            Err(punished(e))
        }
        None => locked.map_err(punished),
    }
}

/// Marks an error that came after the punishment landed (or a strike was saved), so a retry must
/// not punish again.
#[derive(Debug)]
pub(crate) struct Punished;

impl std::fmt::Display for Punished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("after the automod punishment")
    }
}

fn punished(e: anyhow::Error) -> anyhow::Error {
    e.context(Punished)
}

/// The auto lockdown at a strike number, for every path that records a strike.
pub(crate) async fn strike_cap_lock(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    strike: u32,
) -> Result<()> {
    let lock = &cfg.automod.auto_lockdown;
    if lock.enabled
        && lock.strikes.is_some_and(|n| strike >= n)
        && lockdown::lock(app, gid, cfg.lockdown_prefs.kind()).await?
    {
        send_log(
            app,
            gid.get(),
            LogKind::Panic,
            Log::new("Server Locked", ALERT)
                .text(&format!("{YES} **The server has been locked down.**"))
                .user("Member", user.get())
                .line("Reason", &format!("Reached strike `{strike}`")),
        )
        .await?;
    }
    Ok(())
}

/// The tracked heat messages plus the triggering one (an edit is not tracked), as (channel, message).
fn with_msg(tracked: Option<Vec<(u64, u64)>>, here: Option<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut msgs = tracked.unwrap_or_default();
    if let Some(here) = here.filter(|h| !msgs.contains(h)) {
        msgs.push(here);
    }
    msgs
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

/// The part with the biggest heat contribution. On a tie the first wins: the instant filters come
/// first, so a scam link with its own Ban is not taken for the crowd's equal similarity part.
fn top_part(b: &Breakdown) -> Option<&'static str> {
    b.parts
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1).then(std::cmp::Ordering::Greater))
        .map(|p| p.0)
}

/// The filter behind the biggest heat contribution, with "Factor" text for it.
fn tripped_filter<'a>(
    b: &Breakdown,
    f: &'a FiltersCfg,
    bad_site: bool,
) -> Option<(&'a FilterCfg, &'static str)> {
    Some(match top_part(b)? {
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
        "nsfw_image" => (&f.nsfw_images, "Posting an NSFW image"),
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
            // The warn landed. The threshold's timeout is the punishment; the strikes only keep
            // count of it.
            let counted = async {
                if warn_actions(app, cfg, gid, uid).await? {
                    let mut st = store::load_strikes(&app.pool, gid.get(), uid.get()).await?;
                    let act = strike(&mut st, &cfg.automod.strike, now_ms());
                    store::save_strikes(&app.pool, gid.get(), uid.get(), &st).await?;
                    strike_cap_lock(app, cfg, gid, uid, act.strike).await?;
                }
                anyhow::Ok(())
            };
            counted.await.map_err(punished)?;
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
            if kind == "ban" {
                crate::scheduler::forget_jobs(app, gid, uid.get(), crate::scheduler::BANNED, None)
                    .await;
            }
            auto_case(app, gid, uid, kind, factor, timed)
                .await
                .map_err(punished)?;
        }
    }
    send_log(
        app,
        gid.get(),
        LogKind::AutoMod,
        filter_log(
            &m.author.name,
            action,
            factor,
            (m.channel_id.get(), &m.content),
            uid.get(),
        ),
    )
    .await
    .map_err(punished)
}

/// The automod log for a filter's own action.
pub(crate) fn filter_log(
    name: &str,
    action: FilterAction,
    factor: &str,
    (ch, content): (u64, &str),
    uid: u64,
) -> Log {
    Log::new(&action_title(name, action), action_color(action))
        .line("Factor", &format!("*{factor}*"))
        .line("Channel", &channel(ch))
        .line("Message", &format!("||{}||", clip(content, 500)))
        .user("Member", uid)
}

/// Automod log title: "*name has been warned!*".
fn action_title(name: &str, action: FilterAction) -> String {
    match action {
        FilterAction::Delete => format!("*{name}'s message has been deleted!*"),
        FilterAction::Warn => format!("*{name} has been warned!*"),
        FilterAction::Timeout => format!("*{name} has been timed out!*"),
        FilterAction::Kick => format!("*{name} has been kicked!*"),
        FilterAction::Ban => format!("*{name} has been banned!*"),
    }
}

/// The automod log for a heat timeout.
pub(crate) fn timeout_log(
    name: &str,
    reason: &str,
    (user, ch): (u64, u64),
    timeout_ms: i64,
    strike: u32,
) -> Log {
    Log::new(&format!("*{name} has been timed out!*"), ORANGE)
        .line("Reason", &format!("*{reason}*"))
        .user("Member", user)
        .line("Channel", &channel(ch))
        .line("Duration", &format!("`{}`", duration(timeout_ms / 1000)))
        .line("Strike", &format!("`{strike}`"))
}

/// The automod log for a heat strike: a timeout, or with the auto timeouts off only the strike.
fn heat_log(
    timed: bool,
    name: &str,
    reason: &str,
    (user, ch): (u64, u64),
    act: &tommybot_automod::strike::StrikeAction,
) -> Log {
    if timed {
        return timeout_log(name, reason, (user, ch), act.timeout_ms, act.strike);
    }
    Log::new(&format!("*{name} got a strike!*"), ORANGE)
        .line("Reason", &format!("*{reason}*"))
        .user("Member", user)
        .line("Channel", &channel(ch))
        .line("Strike", &format!("`{}`", act.strike))
}

fn action_color(action: FilterAction) -> u32 {
    match action {
        FilterAction::Kick | FilterAction::Ban => RED,
        _ => ORANGE,
    }
}

/// Guilds whose last Discord AutoMod rule sync failed: the local filter reads every word there.
static SYNC_FAILED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<u64>>> =
    std::sync::LazyLock::new(Default::default);

/// Blacklisted word hits the local filter counts. With "Use Discord AutoMod" on, Discord's rule
/// blocks what fits in it, so only the words and patterns left out of it count here, or all of
/// them while the sync is failing.
fn local_word_hits(guild: u64, f: &FiltersCfg, forms: &str) -> usize {
    let words = blacklist_words(f);
    let regex = &f.words.regex;
    if !f.words.use_discord_automod || SYNC_FAILED.lock().unwrap().contains(&guild) {
        return filters::word_hits(forms, &words) + regex.hits(forms);
    }
    let (w_ok, r_ok) = tommybot_automod::discord_api::on_discord(&words, regex.as_slice());
    let left: Vec<String> = words
        .into_iter()
        .zip(w_ok)
        .filter(|(w, ok)| !ok && !w.is_empty())
        .map(|(w, _)| w)
        .collect();
    let left_hits = if left.is_empty() {
        0
    } else {
        filters::word_hits(forms, &left)
    };
    left_hits
        + regex
            .matched(forms)
            .into_iter()
            .filter(|&i| !r_ok[i])
            .count()
}

/// Keeps the native Discord AutoMod keyword rule in step with the saved word lists.
/// Only runs while the guild opted in; turning it off is done from the rule list (`Api::set_rule_enabled`).
/// A failed sync alerts the mod log, and the local filter covers every word until one works.
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
    let Err(e) = run.await else {
        SYNC_FAILED.lock().unwrap().remove(&guild);
        return;
    };
    tracing::warn!(?e, guild, "syncing the Discord AutoMod rule failed");
    SYNC_FAILED.lock().unwrap().insert(guild);
    let log = Log::new("Discord AutoMod Sync Failed", RED)
        .line("Error", &crate::http_err::why(&e))
        .text("TommyBot's own word filter checks every blacklisted word until a sync works.");
    if let Err(e) = send_log(&app, guild, LogKind::AutoMod, log).await {
        tracing::warn!(?e, guild, "could not log the failed Discord AutoMod sync");
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
    // The caller still records the strike for a timeout when the log fails.
    if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
        tracing::warn!(?e, "warn action log failed");
    }
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
    fn with_msg_adds_the_trigger_once() {
        assert_eq!(with_msg(Some(vec![(1, 2)]), Some((1, 2))), [(1, 2)]);
        assert_eq!(with_msg(Some(vec![(1, 2)]), Some((1, 3))), [(1, 2), (1, 3)]);
        assert_eq!(with_msg(None, None), []);
    }

    #[test]
    fn disabled_filters_lose_their_weight() {
        let mut am = AutomodCfg::default();
        am.filters.emojis.enabled = true;
        am.filters.invites.enabled = true;
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
        // A tie with the crowd's similarity part pushed later: the instant filter wins.
        assert_eq!(
            pick(vec![("invites", 100.0), ("similarity", 100.0)]),
            Some(FilterAction::Kick)
        );
    }
}

#[cfg(test)]
#[path = "automod_tests.rs"]
pub(crate) mod more_tests;
