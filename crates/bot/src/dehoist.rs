//! Auto-dehoist (names that start with symbols to sit on top of the member list get a nick
//! without them) and the rename step it shares with Join Gate's Rename action.
use anyhow::Result;
use std::time::Instant;
use tommybot_core::permits::{self, Level, Limit};
use tommybot_logging::{embed::ORANGE, Log, LogKind};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    guild::Member,
    id::{
        marker::{GuildMarker, RoleMarker, UserMarker},
        Id,
    },
    user::User,
};

use crate::{app::App, events::send_log};

/// Member edits share Discord's rate-limit bucket with timeouts and bans, so renames are capped
/// per guild; past the cap a name is caught on the member's next update.
const CAP: Limit = Limit {
    per_min: 10,
    per_hour: 600,
};

/// The last join step, for a joiner no earlier step acted on.
pub(crate) async fn on_join(app: &App, gid: Id<GuildMarker>, m: &Member) {
    let (user, nick) = (&m.user, m.nick.as_deref());
    if let Err(e) = check(app, gid, user, nick, &m.roles).await {
        tracing::warn!(?e, "auto-dehoist on join failed");
    }
}

/// Dehoists a member who joined or changed their name, when the setting is on.
pub(crate) async fn check(
    app: &App,
    gid: Id<GuildMarker>,
    user: &User,
    nick: Option<&str>,
    roles: &[Id<RoleMarker>],
) -> Result<()> {
    if user.bot || !app.cfg(gid.get()).await?.misc.auto_dehoist {
        return Ok(());
    }
    let name = nick.or(user.global_name.as_deref()).unwrap_or(&user.name);
    let key = (gid.get(), user.id.get());
    let Some(new) = crate::modview::dehoisted(name) else {
        app.dehoisted.lock().unwrap().remove(&key);
        return Ok(());
    };
    // During a raid or lockdown the member bucket is kept for punishments.
    if crate::lockdown::raid_or_lock(app, gid).await? {
        return Ok(());
    }
    // Quarantined members (also on the way in: rejoin, verification's quarantine on join) are left as is.
    if crate::antinuke::quarantined(app, gid, user.id).await? {
        return Ok(());
    }
    // Updates still carrying the old name (ours echoes back, others run alongside) rename once.
    {
        let mut last = app.dehoisted.lock().unwrap();
        if last.get(&key) == Some(&new) {
            return Ok(());
        }
        // shortcut: past the cap every entry is dropped; a repeat costs one extra rename.
        if last.len() > 10_000 {
            last.clear();
        }
        last.insert(key, new.clone());
    }
    let r = rename(app, gid, user.id, roles, (name, &new), "Auto-dehoist").await;
    if !matches!(r, Ok(None)) {
        app.dehoisted.lock().unwrap().remove(&key);
    }
    r.map(drop)
}

/// [`rename`]'s skip reason when the guild's cap for that caller is used up.
pub(crate) const CAPPED: &str = "rename cap reached";

/// Sets the nick and logs it in the Member log. `Ok(Some(why))` when skipped: the owner and Extra
/// Owners, members at or above the bot (Discord would refuse) and the rename cap. Each `reason`
/// has its own cap, so auto-dehoist cannot use up Join Gate's renames.
pub(crate) async fn rename(
    app: &App,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
    roles: &[Id<RoleMarker>],
    (old, new): (&str, &str),
    reason: &str,
) -> Result<Option<&'static str>> {
    let info = app.guild_info(gid).await?;
    if crate::hierarchy::top_position(&info.roles, roles) >= info.bot_top {
        return Ok(Some("their top role is not below the bot's"));
    }
    let ids: Vec<i64> = roles.iter().map(|r| r.get() as i64).collect();
    let g = gid.get() as i64;
    let level = permits::resolve(
        &app.pool,
        g,
        info.owner.get() as i64,
        uid.get() as i64,
        &ids,
        false,
    );
    if matches!(level.await?, Level::Owner | Level::ExtraOwner) {
        return Ok(Some("owner or Extra Owner"));
    }
    let allowed = app
        .rate_limiter
        .lock()
        .unwrap()
        .allow(g, reason, CAP, Instant::now());
    if !allowed {
        return Ok(Some(CAPPED));
    }
    app.http
        .update_guild_member(gid, uid)
        .nick(Some(new))
        .reason(reason)
        .await?;
    let log = Log::new("Member Renamed", ORANGE)
        .user("Member", uid.get())
        .line("Name", &format!("`{old}` → `{new}`"))
        .line("Reason", reason);
    if let Err(e) = send_log(app, gid.get(), LogKind::Member, log).await {
        tracing::warn!(?e, "rename log failed");
    }
    Ok(None)
}
