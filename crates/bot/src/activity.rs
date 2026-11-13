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
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{App, GuildCfg},
    automod::{auto_case, blacklist_words, enabled_weights, now_ms},
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

fn word_hit(f: &FiltersCfg, text: &str) -> bool {
    f.words.filter.enabled
        && filters::word_hits(text, &blacklist_words(f)) + f.words.regex.hits(text) > 0
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
    if !fresh || !am.strike.enabled {
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
    timeout(&app.http, gid, user, secs, "Automod").await?;
    auto_case(app, gid, user, "timeout", reason, Some(secs)).await?;
    let log = Log::new("*A member has been timed out!*", ORANGE)
        .line("Reason", &format!("*{reason}*"))
        .user("Member", user.get())
        .line("Duration", &format!("`{}`", duration(secs)))
        .line("Strike", &format!("`{}`", act.strike));
    send_log(app, gid.get(), LogKind::AutoMod, log).await
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
    } && !app.whitelisted(gid, user, "words").await?;
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
    let name = t.name.as_deref().unwrap_or_default();
    let w = enabled_weights(am);
    if word_hit(&am.filters, name) && !app.whitelisted(gid, owner, "words").await? {
        crate::http_err::best_effort(app.http.delete_channel(t.id).await, "automod thread delete");
        let log = Log::new("Thread deleted", RED)
            .line("Reason", "*Blacklisted words in the thread name*")
            .user("Member", owner.get())
            .line("Channel", &channel(parent.get()))
            .line("Name", &format!("`{}`", name.replace('`', "'")));
        send_log(app, gid.get(), LogKind::AutoMod, log).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
