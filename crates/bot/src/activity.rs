//! Automod for what members do besides writing messages: reaction spam and new threads.
use anyhow::Result;
use tommybot_automod::{filters, heat::Breakdown, store, strike::strike, window::Window};
use tommybot_core::sections::FiltersCfg;
use tommybot_logging::{
    embed::{channel, duration, ORANGE, RED},
    Log, LogKind,
};
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::{
    channel::{message::EmojiReactionType, Channel},
    gateway::payload::incoming::ReactionAdd,
    guild::audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType},
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{App, GuildCfg},
    automod::{
        action_failed, auto_case, blacklist_words, enabled_weights, now_ms, strike_cap_lock,
    },
    commands::timeout,
    events::send_log,
};

/// Reactions per member, across messages, before each further one is removed and heats.
const REACTION_LIMIT: usize = 15;
const REACTION_SPAN_MS: i64 = 10_000;

/// Counts one reaction; true when it is over the limit.
fn reaction_burst(w: &mut Window, now: i64) -> bool {
    w.push(now, REACTION_SPAN_MS) > REACTION_LIMIT
}

/// Cured like message text, so lookalike letters and hidden characters do not get a word past.
fn word_hit(f: &FiltersCfg, text: &str) -> bool {
    let forms = filters::word_forms(text);
    f.words.filter.enabled
        && filters::word_hits(&forms, &blacklist_words(f)) + f.words.regex.hits(&forms) > 0
}

/// Word-whitelisted, or holding the automod permit that exempts their messages too.
async fn words_exempt(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
    Ok(app.whitelisted(gid, user, "words").await? || app.whitelisted(gid, user, "automod").await?)
}

/// Spam-whitelisted like a message would be: spam whitelist (of the parent channel in a thread),
/// dangerous permissions, or the automod permit.
async fn spam_exempt(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    roles: &[Id<RoleMarker>],
    ch: Id<ChannelMarker>,
) -> Result<bool> {
    let (parent, category) = app.channel_scope(gid, ch).await.unwrap_or((ch, None));
    Ok(cfg
        .whitelist
        .spam
        .covers(user, roles, parent, category, None)
        || cfg.whitelist.partner_channels.contains(&parent)
        || app.staff_by_perms(cfg, gid, roles).await
        || app.whitelisted(gid, user, "automod").await?)
}

/// Adds heat for something that is not a message; a trip is a strike timeout, as for messages.
async fn heat_up(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    points: f64,
    reason: &str,
) -> Result<()> {
    let am = &cfg.automod;
    let now = now_ms();
    let key = (gid.get(), user.get());
    let fresh = {
        let mut map = app.heat.lock().unwrap();
        let (s, _) = map.entry(key).or_default();
        let b = Breakdown {
            parts: vec![("regular", points)],
        };
        s.apply(&b, now, &enabled_weights(am)) && s.claim(now)
    };
    if !fresh {
        return Ok(());
    }
    crate::panic::note_heat(app, gid, user).await;
    let mut st = store::load_strikes(&app.pool, gid.get(), user.get()).await?;
    let act = strike(&mut st, &am.strike, now);
    store::save_strikes(&app.pool, gid.get(), user.get(), &st).await?;
    if act.reset_heat {
        if let Some((s, _)) = app.heat.lock().unwrap().get_mut(&key) {
            s.value = 0.0;
        }
    }
    let secs = act.timeout_ms / 1000;
    // As for messages: auto timeouts off still records the strike and its strike-cap lockdown.
    if !am.strike.enabled {
        return strike_cap_lock(app, cfg, gid, user, act.strike).await;
    }
    // As for messages: a failed timeout, case or log never skips the strike-cap lockdown.
    let timed = timeout(&app.http, gid, user, secs, "Automod").await;
    match &timed {
        Err(e) => action_failed(app, gid, user.get(), "Timeout", e).await,
        Ok(()) => {
            if let Err(e) = auto_case(app, gid, user, "timeout", reason, Some(secs)).await {
                tracing::warn!(?e, "automod case failed");
            }
            let log = Log::new("*A member has been timed out!*", ORANGE)
                .line("Reason", &format!("*{reason}*"))
                .user("Member", user.get())
                .line("Duration", &format!("`{}`", duration(secs)))
                .line("Strike", &format!("`{}`", act.strike));
            if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
                tracing::warn!(?e, "automod log failed");
            }
        }
    }
    strike_cap_lock(app, cfg, gid, user, act.strike).await?;
    timed
}

pub async fn on_reaction(app: &App, r: &ReactionAdd) -> Result<()> {
    let (Some(gid), Some(member)) = (r.guild_id, r.member.as_ref()) else {
        return Ok(());
    };
    if member.user.bot {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let am = &cfg.automod;
    if !am.enabled {
        return Ok(());
    }
    let user = r.user_id;
    let bad_name = match &r.emoji {
        EmojiReactionType::Custom { name: Some(n), .. } => word_hit(&am.filters, n),
        _ => false,
    } && !words_exempt(app, gid, user).await?;
    let burst = am.anti_spam && {
        let mut map = app.reactions.lock().unwrap();
        // shortcut: past the cap every window is dropped; a spammer only gets 15 free reactions again.
        if map.len() > 100_000 {
            map.clear();
        }
        reaction_burst(map.entry((gid.get(), user.get())).or_default(), now_ms())
    };
    if !bad_name
        && (!burst || spam_exempt(app, &cfg, gid, user, &member.roles, r.channel_id).await?)
    {
        return Ok(());
    }
    let emoji = match &r.emoji {
        EmojiReactionType::Custom { id, name, .. } => RequestReactionType::Custom {
            id: *id,
            name: name.as_deref(),
        },
        EmojiReactionType::Unicode { name } => RequestReactionType::Unicode { name },
    };
    crate::http_err::best_effort(
        app.http
            .delete_reaction(r.channel_id, r.message_id, &emoji, user)
            .await,
        "automod reaction delete",
    );
    let w = enabled_weights(am);
    let (points, reason) = if bad_name {
        (w.per_blacklisted_word, "Blacklisted words in a reaction")
    } else {
        (w.base, "Reaction spam")
    };
    heat_up(app, &cfg, gid, user, points, reason).await
}

/// Thread events: remember the parent; a new thread with a blacklisted name is deleted, and
/// every new thread heats its owner like a message.
pub async fn on_thread(app: &App, t: &Channel) -> Result<()> {
    let (Some(gid), Some(parent)) = (t.guild_id, t.parent_id) else {
        return Ok(());
    };
    app.note_thread(t.id, parent);
    // Joining an existing private thread also sends a create.
    let Some(owner) = t.owner_id.filter(|_| t.newly_created == Some(true)) else {
        return Ok(());
    };
    if owner == app.me {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let am = &cfg.automod;
    if !am.enabled {
        return Ok(());
    }
    let w = enabled_weights(am);
    if thread_name_hit(app, &cfg, gid, (t, parent, owner), true).await? {
        return heat_up(
            app,
            &cfg,
            gid,
            owner,
            w.per_blacklisted_word,
            "Blacklisted words",
        )
        .await;
    }
    let roles = crate::http_err::member(&app.http, gid, owner)
        .await?
        .map(|m| m.roles)
        .unwrap_or_default();
    if !am.anti_spam || spam_exempt(app, &cfg, gid, owner, &roles, t.id).await? {
        return Ok(());
    }
    heat_up(app, &cfg, gid, owner, w.base, "Thread spam").await
}

/// A rename can add a word after the create passed. An update names no actor and also comes for a
/// lock, tags or pins, so the thread is deleted only when the audit log shows its owner renamed it;
/// otherwise (a mod's rename, a word added to the list later) it is only logged.
pub async fn on_thread_update(app: &App, t: &Channel) -> Result<()> {
    let (Some(gid), Some(parent)) = (t.guild_id, t.parent_id) else {
        return Ok(());
    };
    app.note_thread(t.id, parent);
    // Archiving is no rename, and renaming an archived thread opens it again.
    let archived = t.thread_metadata.as_ref().is_some_and(|m| m.archived);
    let Some(owner) = t.owner_id.filter(|&o| o != app.me && !archived) else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    if cfg.automod.enabled {
        thread_name_hit(app, &cfg, gid, (t, parent, owner), false).await?;
    }
    Ok(())
}

/// Logs a thread whose name has a blacklisted word, and deletes it when `delete`; true on a hit.
async fn thread_name_hit(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    (t, parent, owner): (&Channel, Id<ChannelMarker>, Id<UserMarker>),
    delete: bool,
) -> Result<bool> {
    let name = t.name.as_deref().unwrap_or_default();
    if !word_hit(&cfg.automod.filters, name) || words_exempt(app, gid, owner).await? {
        return Ok(false);
    }
    // shortcut: in memory, so a restart logs a still flagged name once more on its next update.
    static FLAGGED: std::sync::LazyLock<crate::automod::Seen> =
        std::sync::LazyLock::new(Default::default);
    let named = (t.id.get(), {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        h.finish()
    });
    // A lock, tags or pins send the same name again: logged once.
    if !delete && !FLAGGED.first(named) {
        return Ok(true);
    }
    let delete = delete || renamed_by_owner(app, gid, t.id, name, owner).await;
    let title = if delete {
        crate::http_err::best_effort(app.http.delete_channel(t.id).await, "automod thread delete");
        "Thread deleted"
    } else {
        "Thread name flagged"
    };
    let log = Log::new(title, RED)
        .line("Reason", "*Blacklisted words in the thread name*")
        .user("Member", owner.get())
        .line("Channel", &channel(parent.get()))
        .line("Name", &format!("`{}`", name.replace('`', "'")));
    send_log(app, gid.get(), LogKind::AutoMod, log).await?;
    Ok(true)
}

/// The audit log shows `owner` renamed `thread` to `name` within the last minute. Tried again
/// shortly, since the entry can come after the event; a failed lookup counts as no.
async fn renamed_by_owner(
    app: &App,
    gid: Id<GuildMarker>,
    thread: Id<ChannelMarker>,
    name: &str,
    owner: Id<UserMarker>,
) -> bool {
    for wait in [0, 1_500] {
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        let log = app
            .http
            .audit_log(gid)
            .action_type(AuditLogEventType::ThreadUpdate)
            .limit(10)
            .await;
        let Ok(log) = (match log {
            Ok(r) => r.model().await.map_err(anyhow::Error::from),
            Err(e) => Err(e.into()),
        }) else {
            return false;
        };
        if owner_renamed(&log.entries, (thread, name, owner), now_ms()) {
            return true;
        }
    }
    false
}

fn owner_renamed(
    entries: &[AuditLogEntry],
    (thread, name, owner): (Id<ChannelMarker>, &str, Id<UserMarker>),
    now: i64,
) -> bool {
    entries.iter().any(|e| {
        // Snowflake time: an older rename to this name is no proof of this update.
        let at = (e.id.get() >> 22) as i64 + 1_420_070_400_000;
        e.target_id.is_some_and(|t| t.get() == thread.get())
            && e.user_id == Some(owner)
            && now - at < 60_000
            && e.changes.iter().any(|c| {
                matches!(c, AuditLogChange::Name { new: Some(n), old }
                    if n == name && old.as_deref() != Some(name))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bansync::tests::T;
    use serde_json::json;

    /// A reaction or thread heat strike reaches the strike-cap lockdown too, also when the timeout
    /// fails (the mock has no member PATCH).
    #[tokio::test]
    async fn heat_strikes_reach_the_strike_cap_lockdown() {
        let Some(t) = T::new(97).await else { return };
        let gid = t.g(0);
        let cfg =
            json!({"automod": {"enabled": true, "auto_lockdown": {"enabled": true, "strikes": 1}}});
        t.server_config(0, t.owner(), cfg).await;
        let cfg = t.app.cfg(gid.get()).await.unwrap();
        let r = heat_up(
            &t.app,
            &cfg,
            gid,
            Id::new(t.base + 5),
            1_000.0,
            "Reaction spam",
        )
        .await;
        assert!(r.is_err());
        assert!(t.calls("PATCH") >= 1, "timeout tried");
        assert!(t.calls("/roles") >= 1, "lockdown planned");
        t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.cleanup().await;
    }

    /// With the auto timeouts off a reaction or thread heat trip still records the strike and
    /// reaches the strike-cap lockdown, as a message does, but times nobody out.
    #[tokio::test]
    async fn strikes_off_still_records_the_strike() {
        let Some(t) = T::new(25).await else { return };
        let gid = t.g(0);
        let cfg = json!({"automod": {"enabled": true, "strike": {"enabled": false},
            "auto_lockdown": {"enabled": true, "strikes": 1}}});
        t.server_config(0, t.owner(), cfg).await;
        let cfg = t.app.cfg(gid.get()).await.unwrap();
        let user = t.base + 5;
        let _ = heat_up(&t.app, &cfg, gid, Id::new(user), 1_000.0, "Reaction spam").await;
        let st = store::load_strikes(&t.app.pool, gid.get(), user)
            .await
            .unwrap();
        assert_eq!(st.strikes, 1, "strike recorded");
        assert_eq!(
            t.calls(&format!("PATCH /guilds/{gid}/members/{user}")),
            0,
            "nobody timed out"
        );
        assert!(t.calls("/roles") >= 1, "strike-cap lockdown planned");
        t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.cleanup().await;
    }

    /// A bad emoji name from an automod-permit holder is left alone, as their messages are; another
    /// member's reaction is removed.
    #[tokio::test]
    async fn an_automod_permit_covers_reaction_names() {
        let Some(t) = T::new(99).await else { return };
        let gid = t.g(0);
        let cfg = json!({"automod": {"enabled": true,
            "filters": {"words": {"filter": {"enabled": true}, "strict": ["scam"]}}}});
        t.server_config(0, t.owner(), cfg).await;
        t.exec("INSERT INTO permits (id, guild_id, name, perms, whitelist) VALUES ($1, $1, 'Trusted', '{}', '{automod}')")
            .await;
        t.exec("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $1, 556)")
            .await;
        let (holder, other) = (t.base + 5, t.base + 6);
        t.set(|m| {
            m.members.insert((gid.get(), holder), vec![556]);
            m.members.insert((gid.get(), other), vec![]);
        });
        let react = |user: u64, roles: Vec<&str>| -> ReactionAdd {
            ReactionAdd(serde_json::from_value(json!({
                "burst": false, "burst_colors": [], "channel_id": "7", "message_id": "100",
                "guild_id": gid.to_string(), "user_id": user.to_string(),
                "emoji": {"id": "5", "name": "scam"},
                "member": {"deaf": false, "mute": false, "flags": 0, "pending": false, "roles": roles,
                    "joined_at": "2026-01-01T00:00:00.000000+00:00",
                    "user": {"id": user.to_string(), "username": "x", "discriminator": "0", "avatar": null}},
            }))
            .unwrap())
        };
        on_reaction(&t.app, &react(holder, vec!["556"]))
            .await
            .unwrap();
        assert_eq!(t.calls("/reactions/"), 0, "permit holder: reaction kept");
        assert_eq!(t.calls("PATCH"), 0, "permit holder: no timeout");
        let _ = on_reaction(&t.app, &react(other, vec![])).await;
        assert!(
            t.calls("/reactions/") >= 1,
            "another member's reaction is removed"
        );
        t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.cleanup().await;
    }

    /// A renamed thread with a blacklisted word is logged, never deleted.
    #[tokio::test]
    async fn a_renamed_thread_is_only_logged() {
        let Some(t) = T::new(98).await else { return };
        let gid = t.g(0);
        let cfg = json!({"logging": {"fallback": "55"}, "automod": {"enabled": true,
            "filters": {"words": {"filter": {"enabled": true}, "strict": ["scam"]}}}});
        t.server_config(0, t.owner(), cfg).await;
        let thread = |name: &str| -> Channel {
            serde_json::from_value(json!({
                "id": "40", "type": 11, "guild_id": gid.to_string(), "parent_id": "7",
                "owner_id": (t.base + 5).to_string(), "name": name,
                "thread_metadata": {"archived": false, "auto_archive_duration": 60,
                    "archive_timestamp": "2026-01-01T00:00:00.000000+00:00", "locked": false},
            }))
            .unwrap()
        };
        on_thread_update(&t.app, &thread("hello")).await.unwrap();
        assert_eq!(t.calls("/channels/55"), 0, "a clean name is not logged");
        // The mock refuses the log channel lookup, so the update reports the failed log.
        let _ = on_thread_update(&t.app, &thread("ｓｃａｍ here")).await;
        let logged = t.calls("/channels/55");
        assert!(logged >= 1, "logged");
        assert_eq!(t.calls("DELETE"), 0, "not deleted");
        // A later update (lock, tags, pins) with the same name logs nothing new.
        let _ = on_thread_update(&t.app, &thread("ｓｃａｍ here")).await;
        assert_eq!(t.calls("/channels/55"), logged);
        t.cleanup().await;
    }

    /// A rename to a blacklisted name is deleted when the audit log shows the owner did it just now;
    /// a mod's rename, or an old entry, is only logged.
    #[tokio::test]
    async fn an_owner_s_rename_deletes_the_thread() {
        let Some(t) = T::new(20).await else { return };
        let gid = t.g(0);
        let cfg = json!({"automod": {"enabled": true,
            "filters": {"words": {"filter": {"enabled": true}, "strict": ["scam"]}}}});
        t.server_config(0, t.owner(), cfg).await;
        let (owner, modr) = (t.base + 5, t.base + 6);
        let thread = |id: u64, name: &str| -> Channel {
            serde_json::from_value(json!({
                "id": id.to_string(), "type": 11, "guild_id": gid.to_string(), "parent_id": "7",
                "owner_id": owner.to_string(), "name": name,
                "thread_metadata": {"archived": false, "auto_archive_duration": 60,
                    "archive_timestamp": "2026-01-01T00:00:00.000000+00:00", "locked": false},
            }))
            .unwrap()
        };
        let entry = |thread: u64, by: u64, ago_ms: i64| {
            let id = ((now_ms() - ago_ms - 1_420_070_400_000) as u64) << 22;
            json!({"id": id.to_string(), "action_type": 111, "target_id": thread.to_string(),
                "user_id": by.to_string(), "guild_id": gid.to_string(),
                "changes": [{"key": "name", "old_value": "hello", "new_value": "scam here"}]})
        };
        t.set(|m| {
            m.audit = vec![
                entry(41, owner, 0),
                entry(42, modr, 0),
                entry(43, owner, 600_000),
            ]
        });
        let _ = on_thread_update(&t.app, &thread(42, "scam here")).await;
        let _ = on_thread_update(&t.app, &thread(43, "scam here")).await;
        assert_eq!(
            t.calls("DELETE /channels/"),
            0,
            "a mod's or an old rename is kept"
        );
        let _ = on_thread_update(&t.app, &thread(41, "scam here")).await;
        assert_eq!(
            t.calls("DELETE /channels/41"),
            1,
            "the owner's rename is deleted"
        );
        t.cleanup().await;
    }

    #[test]
    fn names_are_cured_before_the_word_filter() {
        let mut f = FiltersCfg::default();
        f.words.filter.enabled = true;
        f.words.strict = vec!["scam".into()];
        assert!(word_hit(&f, "ｓｃａｍ"));
        assert!(word_hit(&f, "s\u{200b}cam"));
        assert!(!word_hit(&f, "scan"));
    }

    #[test]
    fn reactions_burst_past_fifteen_in_ten_seconds() {
        let mut w = Window::default();
        assert!((0..15).all(|i| !reaction_burst(&mut w, i * 100)));
        assert!(reaction_burst(&mut w, 1_600));
        // Once the old ones age out the member may react again.
        let mut w = Window::default();
        (0..15).for_each(|i| {
            reaction_burst(&mut w, i);
        });
        assert!(!reaction_burst(&mut w, 20_000));
    }
}
