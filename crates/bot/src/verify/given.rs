//! A verification role given by another bot (D58): while TommyBot's verification is on, the role
//! is taken back and nobody is punished, as the quarantine hold does for a join quarantine (#170).
//! Switching verification off lets another bot verify instead. A role that opens a channel the
//! verification roles open (an explicit View Channel overwrite, or Administrator) counts too.
//! Not covered: access through a role's base permissions in a channel @everyone can already see.

use crate::{app::App, app::GuildCfg, events::send_log};
use anyhow::Result;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tommybot_logging::{
    embed::{NO, ORANGE},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    guild::{
        audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType as A},
        Permissions,
    },
    id::{
        marker::{GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

/// Roles another bot gave a member, and TommyBot's own verifications, by (guild, member).
type Recent = Mutex<HashMap<(u64, u64), (Vec<Id<RoleMarker>>, Instant)>>;
static BOT_GIVEN: std::sync::LazyLock<Recent> = std::sync::LazyLock::new(Default::default);
static VERIFIED: std::sync::LazyLock<Recent> = std::sync::LazyLock::new(Default::default);
type Locks = Mutex<HashMap<(u64, u64), Arc<tokio::sync::Mutex<()>>>>;
static MEMBER_LOCKS: std::sync::LazyLock<Locks> = std::sync::LazyLock::new(Default::default);

/// How long a grant counts as recent, and how long a handler waits for a bot's other grants.
const RECENT: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_millis(1500);

fn note(map: &Recent, gid: Id<GuildMarker>, user: Id<UserMarker>, roles: &[Id<RoleMarker>]) {
    let mut all = map.lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|_, (_, at)| at.elapsed() < RECENT);
    let e = all
        .entry((gid.get(), user.get()))
        .or_insert_with(|| (vec![], Instant::now()));
    e.0.extend_from_slice(roles);
    e.1 = Instant::now();
}

fn recent(map: &Recent, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Vec<Id<RoleMarker>> {
    let all = map.lock().unwrap_or_else(|p| p.into_inner());
    all.get(&(gid.get(), user.get()))
        .filter(|(_, at)| at.elapsed() < RECENT)
        .map(|(r, _)| r.clone())
        .unwrap_or_default()
}

/// TommyBot verified this member: a bot's grant around the same time takes nothing away.
pub(crate) fn note_verified(gid: Id<GuildMarker>, user: Id<UserMarker>) {
    note(&VERIFIED, gid, user, &[]);
}

fn member_lock(gid: Id<GuildMarker>, user: Id<UserMarker>) -> Arc<tokio::sync::Mutex<()>> {
    let mut all = MEMBER_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    all.retain(|_, l| Arc::strong_count(l) > 1);
    all.entry((gid.get(), user.get())).or_default().clone()
}

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (A::MemberRoleUpdate, Some(gid), Some(actor), Some(target)) =
        (e.action_type, e.guild_id, e.user_id, e.target_id)
    else {
        return Ok(());
    };
    if actor == app.me {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let added: Vec<Id<RoleMarker>> = e
        .changes
        .iter()
        .filter_map(|c| match c {
            AuditLogChange::RoleAdded { new, .. } => Some(new),
            _ => None,
        })
        .flatten()
        .map(|r| r.id)
        .collect();
    if !cfg.verification.enabled || cfg.verification.roles.is_empty() || added.is_empty() {
        return Ok(());
    }
    let target: Id<UserMarker> = target.cast();
    if hold_takes_it(app, &cfg, gid, target, actor).await? {
        return Ok(());
    }
    // A failed lookup is tried once more below, like a failed removal.
    let bot = async { anyhow::Ok(app.http.user(actor).await?.model().await?.bot) };
    let bot = match bot.await {
        Ok(b) => b,
        Err(_) => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            match async { anyhow::Ok(app.http.user(actor).await?.model().await?.bot) }.await {
                Ok(b) => b,
                Err(e) => return failed(app, gid, target, actor, &e).await,
            }
        }
    };
    if !bot {
        return Ok(());
    }
    note(&BOT_GIVEN, gid, target, &added);
    // A bot may give two verification roles in two calls: wait for both, then judge them one
    // member at a time, so neither counts as proof that the member was verified already.
    tokio::time::sleep(SETTLE).await;
    let lock = member_lock(gid, target);
    let _one = lock.lock().await;
    let taken = match take_back(app, &cfg, gid, target, &added).await {
        Ok(t) => Ok(t),
        Err(e) => {
            tracing::warn!(?e, "taking back a verification role failed, trying again");
            tokio::time::sleep(Duration::from_secs(2)).await;
            take_back(app, &cfg, gid, target, &added).await
        }
    };
    match taken {
        Ok(t) if t.is_empty() => Ok(()),
        Ok(_) => {
            let log = Log::new("Verification", ORANGE)
                .user("Member", target.get())
                .user("Bot", actor.get())
                .text(&format!(
                    "{NO} **A verification role given by another bot was taken back**"
                ))
                .line(
                    "Why",
                    "TommyBot's verification is on, so members verify through it. Turn verification off to let another bot verify.",
                );
            if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
                tracing::warn!(?e, "verification role log failed");
            }
            Ok(())
        }
        Err(e) => failed(app, gid, target, actor, &e).await,
    }
}

/// The quarantine hold takes back every role a held member is given, except from a Quarantine
/// Whitelist actor: that grant is judged here like any other.
async fn hold_takes_it(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    actor: Id<UserMarker>,
) -> Result<bool> {
    if !cfg.antinuke.enabled || !crate::antinuke::quarantined(app, gid, target).await? {
        return Ok(false);
    }
    let listed = cfg.antinuke.quarantine_whitelist.contains(&actor)
        || app.whitelisted(gid, actor, "quarantine").await?;
    Ok(!listed)
}

/// Removes what the bot gave an unverified member that verification would give: a verification
/// role or a role opening the same channels. Returns what was taken.
async fn take_back(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    added: &[Id<RoleMarker>],
) -> Result<Vec<Id<RoleMarker>>> {
    let v = &cfg.verification;
    if !recent(&VERIFIED, gid, target).is_empty() {
        return Ok(vec![]);
    }
    let Some(member) = crate::http_err::member(&app.http, gid, target).await? else {
        return Ok(vec![]);
    };
    // Verified already: holds a verification role no bot gave just now.
    let bot_given = recent(&BOT_GIVEN, gid, target);
    if member
        .roles
        .iter()
        .any(|r| v.roles.contains(r) && !bot_given.contains(r))
    {
        return Ok(vec![]);
    }
    let held: Vec<Id<RoleMarker>> = added
        .iter()
        .copied()
        .filter(|r| member.roles.contains(r))
        .collect();
    let mut taken: Vec<Id<RoleMarker>> = held
        .iter()
        .copied()
        .filter(|r| v.roles.contains(r))
        .collect();
    let others: Vec<Id<RoleMarker>> = held.into_iter().filter(|r| !v.roles.contains(r)).collect();
    if !others.is_empty() {
        taken.extend(opens_verified_channels(app, gid, &v.roles, &others).await?);
    }
    for r in &taken {
        app.http
            .remove_guild_member_role(gid, target, *r)
            .reason("Verification role given by another bot")
            .await?;
    }
    Ok(taken)
}

/// The roles among `roles` that open what verification opens: a View Channel allow in a channel
/// where a verification role has one, or Administrator (as held before a role lock).
async fn opens_verified_channels(
    app: &App,
    gid: Id<GuildMarker>,
    verified: &[Id<RoleMarker>],
    roles: &[Id<RoleMarker>],
) -> Result<Vec<Id<RoleMarker>>> {
    let perms = crate::lockdown::unlocked_role_perms(app, gid).await?;
    let channels = app.http.guild_channels(gid).await?.models().await?;
    let views = |c: &twilight_model::channel::Channel, r: Id<RoleMarker>| {
        c.permission_overwrites.iter().flatten().any(|o| {
            o.id == r.cast() && o.allow.contains(Permissions::VIEW_CHANNEL)
        })
    };
    let gated: Vec<_> = channels
        .iter()
        .filter(|c| verified.iter().any(|v| views(c, *v)))
        .collect();
    Ok(roles
        .iter()
        .copied()
        .filter(|r| {
            perms
                .get(r)
                .is_some_and(|p| p & Permissions::ADMINISTRATOR.bits() != 0)
                || gated.iter().any(|c| views(c, *r))
        })
        .collect())
}

/// Lookup or removal failed twice: staff hear that the role may have stayed (D12b rule).
async fn failed(
    app: &App,
    gid: Id<GuildMarker>,
    target: Id<UserMarker>,
    actor: Id<UserMarker>,
    e: &anyhow::Error,
) -> Result<()> {
    tracing::warn!(?e, "taking back a verification role failed twice");
    let log = Log::new("Verification", ORANGE)
        .user("Member", target.get())
        .user("Bot", actor.get())
        .text(&format!(
            "{NO} **Could not take back a verification role given by another bot**"
        ))
        .line("Error", &crate::http_err::why(e))
        .line("Effect", "The member may skip verification. Check their roles.");
    if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
        tracing::warn!(?e, "verification role failure log failed");
    }
    Ok(())
}
