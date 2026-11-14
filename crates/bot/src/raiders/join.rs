//! What a listed user meets in a server: the join check, the alert and its buttons' log lines.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tommybot_automod::join::Action;
use tommybot_core::{
    cases, permits,
    sections::{RaiderAction, RaiderListCfg},
};
use tommybot_logging::{
    embed::{self, ORANGE, RED},
    Log, LogKind,
};
use twilight_model::{
    channel::message::component::{ActionRow, ButtonStyle, Component},
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use super::{action_name, cat_name, BUTTON, PREFIX, UNVERIFIED_ALERT_ONLY};
use crate::{
    antinuke::Applied,
    app::{App, GuildCfg},
    commands, http_err, joingate,
    owner::live_owner,
    reply::md,
};

/// A confirmed, unexpired entry.
pub(super) struct Entry {
    pub(super) category: String,
    pub(super) note: String,
    pub(super) verified: bool,
}

/// The user's confirmed, unexpired entry, whatever the server acts on.
pub(super) async fn entry(app: &App, user: Id<UserMarker>) -> Result<Option<Entry>> {
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT category, public_note, verified FROM raiders WHERE user_id = $1 AND status = 'confirmed'
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(user.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    Ok(row.map(|(category, note, verified)| Entry {
        category,
        note,
        verified,
    }))
}

/// The entry when the list is on here and acts on its category.
pub(super) async fn listed(
    app: &App,
    c: &RaiderListCfg,
    user: Id<UserMarker>,
) -> Result<Option<Entry>> {
    if !c.enabled {
        return Ok(None);
    }
    let acts = |cat: &str| c.categories.iter().any(|k| cat_name(*k) == cat);
    Ok(entry(app, user).await?.filter(|e| acts(&e.category)))
}

/// What the list does to a listed member who joins: an automatic listing the bot owner has not
/// verified yet only alerts.
fn join_action(c: &RaiderListCfg, e: &Entry) -> RaiderAction {
    if UNVERIFIED_ALERT_ONLY && !e.verified {
        RaiderAction::Alert
    } else {
        c.action
    }
}

/// Staff let the user back in here (unban, unquarantine, Allow).
pub(crate) async fn allowed(app: &App, gid: Id<GuildMarker>, user: u64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM raider_allow WHERE guild_id = $1 AND user_id = $2)",
    )
    .bind(gid.get() as i64)
    .bind(user as i64)
    .fetch_one(&app.pool)
    .await?)
}

/// Never acted on here: the bot, the bot owner, the owner, extra owners, trusted admins, the
/// anti-nuke whitelist, and whoever staff let back in. `member` (role ids, bot account) when the
/// caller has it already.
pub(super) async fn exempt(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    member: Option<(&[i64], bool)>,
) -> Result<bool> {
    if allowed(app, gid, user.get()).await? {
        return Ok(true);
    }
    let owner = live_owner(app, gid)
        .await?
        .context("the server is not visible")?;
    let level = match member {
        Some((r, bot)) => {
            let (g, o, u) = (gid.get() as i64, owner.get() as i64, user.get() as i64);
            permits::resolve(&app.pool, g, o, u, r, bot).await?
        }
        None => app.level(gid, owner, user).await?,
    };
    Ok(exempt_why(&level, user, app.me, app.bot_owner).is_some())
}

/// On join, right after the join lock: true when the list punished the member, so the rest of
/// the join path (and Join Raid) does not punish them a second time. Errors are logged.
pub async fn on_join(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> bool {
    match enforce(app, cfg, gid, user).await {
        Ok(done) => done,
        Err(e) => {
            tracing::warn!(?e, %gid, %user, "raider list: join check failed");
            false
        }
    }
}

async fn enforce(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> Result<bool> {
    let c = &cfg.raider_list;
    let Some(entry) = listed(app, c, user).await? else {
        return Ok(false);
    };
    if exempt(app, gid, user, None).await? {
        return Ok(false);
    }
    let action = join_action(c, &entry);
    // Written before the action, so a later lift finds every ban the list may have made; a
    // quarantine older than this row is not the list's.
    sqlx::query(
        "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, $3)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET action = excluded.action, case_no = NULL,
             created_at = CASE WHEN raider_actions.action = excluded.action
                               THEN raider_actions.created_at ELSE now() END",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(action_name(action))
    .execute(&app.pool)
    .await?;
    let why = format!("{PREFIX} {}", entry.category);
    let gate = joingate::Detail::default();
    let u = user.get();
    let (kind, inverse) = match action {
        RaiderAction::Alert => {
            alert(app, gid, user, &entry).await;
            return Ok(false);
        }
        RaiderAction::Kick => {
            joingate::punish(app, cfg, gid, u, Action::Kick, &why, gate).await?;
            ("kick", None)
        }
        RaiderAction::Ban => {
            joingate::punish(app, cfg, gid, u, Action::Ban, &why, gate).await?;
            ("ban", Some(json!({"op": "unban", "user": u})))
        }
        RaiderAction::Quarantine => {
            // Through contain(), so a listed bot also loses its managed role's permissions.
            let real = crate::antinuke::Kind::Real;
            let done = crate::antinuke::contain(app, gid, user, Some(&why), real).await;
            let failed = done.as_ref().err().map(http_err::why);
            let kicked = done.as_ref().is_ok_and(|d| d.applied == Applied::Kicked);
            let label = if kicked { "Kick" } else { "Quarantine" };
            joingate::record(app, gid, u, (label, why.as_str(), gate), failed.as_deref()).await;
            let log = action_log(u, &entry.category, label, failed.as_deref());
            if let Err(e) = crate::events::send_log(app, gid.get(), LogKind::JoinGate, log).await {
                tracing::warn!(?e, "raider list action log failed");
            }
            done?;
            if kicked {
                ("kick", None)
            } else {
                crate::reply::queue_quarantine_dm(app, gid, user).await;
                ("quarantine", Some(json!({"op": "unquarantine", "user": u})))
            }
        }
    };
    let n = bot_case(app, (gid, user), kind, &why, inverse).await?;
    sqlx::query("UPDATE raider_actions SET case_no = $3 WHERE guild_id = $1 AND user_id = $2")
        .bind(gid.get() as i64)
        .bind(u as i64)
        .bind(n)
        .execute(&app.pool)
        .await?;
    commands::mod_log(app, gid, (n, kind), (app.me, user), None, Some(&why)).await;
    Ok(true)
}

/// A case with the bot as moderator.
pub(super) async fn bot_case(
    app: &App,
    (gid, user): (Id<GuildMarker>, Id<UserMarker>),
    kind: &str,
    why: &str,
    inverse: Option<Value>,
) -> Result<i32> {
    let case = cases::NewCase {
        guild_id: gid.get() as i64,
        kind,
        user_id: user.get() as i64,
        mod_id: app.me.get() as i64,
        reason: Some(why),
        points: 0,
        duration_s: None,
        proof: None,
        inverse,
    };
    cases::create(&app.pool, case).await
}

/// The Join Gate log line of a quarantine by the list (kick and ban use the Join Gate's own log).
pub(crate) fn action_log(user: u64, category: &str, action: &str, failed: Option<&str>) -> Log {
    let log = Log::new(
        "Raider List action",
        if failed.is_some() { RED } else { ORANGE },
    )
    .user("Member", user)
    .line("Action", &format!("`{action}`"))
    .line("Category", category);
    match failed {
        Some(e) => log.text(&format!("**Failed:** {e}")),
        None => log.text("This account is on TommyBot's Raider List."),
    }
}

/// The mod log alert for a listed member, with what staff may do about it.
pub(crate) fn alert_log(user: u64, category: &str, note: &str) -> Log {
    let mut log = Log::new("Raider List match", ORANGE)
        .user("Member", user)
        .line("Category", category);
    if !note.is_empty() {
        log = log.line("Reason", &md(&embed::clip(note, 200)));
    }
    log.text("This account is on TommyBot's Raider List of raider accounts. Ban, kick or quarantine them, or press **Allow** so the list never acts on them here.")
}

pub(crate) fn alert_rows(user: u64) -> Vec<Component> {
    let b = |label: &str, action: &str, style| {
        embed::button(label, &format!("{BUTTON}{action}:{user}"), style)
    };
    vec![Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            b("Ban", "ban", ButtonStyle::Danger),
            b("Kick", "kick", ButtonStyle::Danger),
            b("Quarantine", "quarantine", ButtonStyle::Secondary),
            b("Allow", "allow", ButtonStyle::Success),
        ],
    })]
}

async fn alert(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, e: &Entry) {
    let log = alert_log(user.get(), &e.category, &e.note);
    let rows = alert_rows(user.get());
    let sent = crate::events::send_log_rows(app, gid.get(), LogKind::ModCommand, log, &rows);
    if let Err(e) = sent.await {
        tracing::warn!(?e, "raider list alert failed");
    }
}

/// Alerts once for a listed member already in the server.
pub(super) async fn alert_member(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    (roles, bot): (&[i64], bool),
    entry: &Entry,
) -> Result<()> {
    if exempt(app, gid, user, Some((roles, bot))).await? {
        return Ok(());
    }
    let fresh = sqlx::query(
        "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, 'alert') ON CONFLICT DO NOTHING",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1;
    if fresh {
        alert(app, gid, user, entry).await;
    }
    Ok(())
}

/// Why the Raider List never acts on a user in this server, if it does not.
pub(crate) fn exempt_why(
    level: &tommybot_core::permits::Level,
    user: Id<UserMarker>,
    me: Id<UserMarker>,
    bot_owner: Option<Id<UserMarker>>,
) -> Option<&'static str> {
    if user == me {
        return Some("the bot itself");
    }
    if Some(user) == bot_owner {
        return Some("the bot owner");
    }
    use tommybot_core::permits::Level;
    match level {
        Level::Owner => Some("server owner"),
        Level::ExtraOwner => Some("extra owner"),
        Level::TrustedAdmin => Some("trusted admin"),
        l if l.whitelisted("antinuke") => Some("on the anti-nuke whitelist"),
        _ => None,
    }
}
