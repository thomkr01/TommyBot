//! Audit-log driven anti-nuke: count watched actions per executor, quarantine and revert on a breach,
//! and enforce the quarantine hold.
use anyhow::{Context, Result};
use serde_json::json;
use std::{
    collections::HashMap,
    future::Future,
    time::{Duration, Instant},
};
use tommybot_antinuke::{
    gained_dangerous, has_dangerous, hold_violation, nuke_plan, plan_revert, Action, Inverse,
    LogEntry, NukeCase, NukeStep, Verdict,
};
use tommybot_automod::heat::{Breakdown, HeatCfg};
use tommybot_core::{
    cases::{self, NewCase},
    jobs, permits,
    sections::BackupsCfg,
};
use tommybot_logging::{
    embed::{ALERT, NO, YES},
    Log, LogKind,
};
use twilight_http::request::{AuditLogReason, Method, RequestBuilder};
use twilight_model::{
    gateway::payload::incoming::MemberUpdate,
    guild::{
        audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType as A},
        Permissions,
    },
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        marker::{GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App, GuildCfg},
    backup,
    events::send_log,
};

fn watched(e: &AuditLogEntry) -> Option<Action> {
    let changed = |f: fn(&AuditLogChange) -> bool| e.changes.iter().any(f);
    Some(match e.action_type {
        A::ChannelCreate => Action::ChannelCreate,
        A::ChannelDelete => Action::ChannelDelete,
        A::RoleCreate => Action::RoleCreate,
        A::RoleDelete => Action::RoleDelete,
        A::MemberBanAdd => Action::Ban,
        A::MemberKick => Action::Kick,
        A::WebhookCreate => Action::WebhookCreate,
        A::WebhookDelete => Action::WebhookDelete,
        A::MemberPrune => Action::Prune,
        A::BotAdd => Action::BotAdd,
        A::MemberUpdate
            if changed(|c| {
                matches!(
                    c,
                    AuditLogChange::CommunicationDisabledUntil { new: Some(_), .. }
                )
            }) =>
        {
            Action::Timeout
        }
        A::ChannelUpdate => Action::ChannelUpdate,
        A::RoleUpdate if changed(|c| matches!(c, AuditLogChange::Name { .. })) => {
            Action::RoleUpdate
        }
        A::EmojiDelete => Action::EmojiDelete,
        A::StickerDelete => Action::StickerDelete,
        A::AutoModerationRuleDelete => Action::AutoModRuleDelete,
        A::AutoModerationRuleUpdate => Action::AutoModRuleUpdate,
        A::MemberBanRemove => Action::Unban,
        _ => return None,
    })
}

fn inverse(action: Action, target: Option<u64>) -> Option<Inverse> {
    let id = target?;
    Some(match action {
        Action::ChannelCreate => Inverse::DeleteChannel { id },
        Action::RoleCreate => Inverse::DeleteRole { id },
        Action::WebhookCreate => Inverse::DeleteWebhook { id },
        Action::Ban => Inverse::Unban { user: id },
        Action::Kick => Inverse::Reinvite { user: id },
        _ => return None,
    })
}

/// The permit whitelist flag that exempts an action; the rest fall back to the blanket one.
fn whitelist_flag(a: Action) -> &'static str {
    match a {
        Action::Ban | Action::Kick => "wl_kick_ban",
        Action::ChannelCreate => "wl_channel_create",
        Action::ChannelDelete => "wl_channel_delete",
        Action::RoleCreate => "wl_role_create",
        Action::RoleDelete => "wl_role_delete",
        Action::WebhookCreate => "wl_webhook_create",
        _ => "antinuke",
    }
}

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (Some(gid), Some(actor)) = (e.guild_id, e.user_id) else {
        return Ok(());
    };
    if actor == app.me {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    if !cfg.antinuke.enabled {
        return Ok(());
    }
    if matches!(
        e.action_type,
        A::MemberRoleUpdate
            | A::RoleUpdate
            | A::ChannelOverwriteCreate
            | A::ChannelOverwriteUpdate
            | A::GuildUpdate
    ) {
        if e.action_type == A::MemberRoleUpdate {
            hold(app, e, actor).await?;
        }
        // A role rename still counts below.
        guard_perms(app, &cfg, e, actor).await?;
    }
    match e.action_type {
        A::MemberUpdate | A::MemberKick | A::MemberBanAdd if cfg.antinuke.quarantine_hold => {
            hold(app, e, actor).await?
        }
        A::RoleCreate => guard_perms(app, &cfg, e, actor).await?,
        _ => {}
    }
    let Some(action) = watched(e) else {
        return Ok(());
    };
    let Some((_, heat)) = cfg.antinuke.limit(action) else {
        return Ok(());
    };
    // Ticket bots open and close channels all day; the gateway event names the category first.
    let exempt_categories: Vec<u64> = cfg
        .antinuke
        .ticket_categories
        .iter()
        .chain(&cfg.panic.whitelisted_categories)
        .map(|c| c.get())
        .collect();
    if let (Action::ChannelCreate | Action::ChannelDelete, Some(t)) = (action, e.target_id) {
        if exempt_categories.is_empty() {
            app.channel_parents.lock().unwrap().remove(&t.get());
        } else if parent_of(app, t.get())
            .await
            .is_some_and(|p| exempt_categories.contains(&p))
        {
            return Ok(());
        }
    }
    // A webhook on the webhook whitelist is exempt whatever its creator may do.
    let exempt = app.whitelisted(gid, actor, whitelist_flag(action)).await?
        || (action == Action::WebhookCreate
            && e.target_id.is_some_and(|t| {
                cfg.whitelist
                    .spam
                    .webhooks
                    .iter()
                    .any(|w| w.get() == t.get())
            }));
    let inv = inverse(action, e.target_id.map(|t| t.get()));
    if let (true, Some(t), Action::ChannelCreate | Action::RoleCreate) =
        (exempt, e.target_id, action)
    {
        backup::track_created(app, gid, action, t.get()).await?;
    }
    if !exempt {
        sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,$4,$5)")
            .bind(gid.get() as i64)
            .bind(actor.get() as i64)
            .bind(format!("{action:?}"))
            .bind(e.target_id.map(|t| t.get() as i64))
            .bind(inv.as_ref().map(|i| json!(i)))
            .execute(&app.pool)
            .await?;
        if heat > 0
            && cfg.panic.heat_algorithm
            && add_heat(app, gid, actor, heat, cfg.panic.heat_decay)
        {
            crate::panic::note(app, gid, actor, true).await?;
        }
    }
    let verdict = {
        let mut an = app.antinuke.lock().unwrap();
        an.limits = cfg.antinuke.watched();
        an.on_action(gid.get(), actor.get(), action, exempt, Instant::now())
    };
    if let Verdict::Quarantine(c) = verdict {
        let why = format!(
            "{action:?} limit reached ({}/min, {}/hour)",
            c.minute, c.hour
        );
        quarantine_and_revert(app, gid, actor, &why).await?;
    }
    Ok(())
}

/// The category of a created or deleted channel, as its gateway event noted it. Both events run
/// as their own tasks, so the audit entry can come first: a miss waits up to 0.5 s for the other.
async fn parent_of(app: &App, channel: u64) -> Option<u64> {
    for _ in 0..5 {
        if let Some(p) = app.channel_parents.lock().unwrap().remove(&channel) {
            return p;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Adds a staff action's heat %; true when the member just reached 100.
fn add_heat(app: &App, gid: Id<GuildMarker>, actor: Id<UserMarker>, heat: u32, decay: f64) -> bool {
    let cfg = HeatCfg {
        decay_per_sec: decay.clamp(0.0, 1.0),
        ..Default::default()
    };
    let b = Breakdown {
        parts: vec![("nuke", f64::from(heat))],
    };
    let mut map = app.nuke_heat.lock().unwrap();
    let s = map.entry((gid.get(), actor.get())).or_default();
    let hot = s.apply(&b, now_secs() * 1000, &cfg);
    if hot {
        s.value = 0.0;
    }
    hot
}

/// Strict mode, public role and channel permission guards, and the vanity guard.
/// Undoes the change where the bot can, then quarantines the actor.
async fn guard_perms(
    app: &App,
    cfg: &GuildCfg,
    e: &AuditLogEntry,
    actor: Id<UserMarker>,
) -> Result<()> {
    let (Some(gid), Some(target)) = (e.guild_id, e.target_id) else {
        return Ok(());
    };
    let an = &cfg.antinuke;
    let guarded = an.strict_mode
        || an.strict_member_roles
        || an.monitor_public_roles
        || an.monitor_channel_perms
        || an.vanity_guard
        || an.quarantine_role == Some(target.cast());
    if !guarded || app.whitelisted(gid, actor, "antinuke").await? {
        return Ok(());
    }
    let public = |id: u64| id == gid.get() || cfg.lockdown.main_roles.iter().any(|r| r.get() == id);
    let gained = |old: Option<Permissions>, new: Option<Permissions>| {
        let bits = |p: Option<Permissions>| p.map_or(0, |p| p.bits());
        gained_dangerous(bits(old), bits(new)).then(|| old.unwrap_or(Permissions::empty()))
    };
    let why = match e.action_type {
        A::RoleCreate if an.strict_mode => {
            if !e.changes.iter().any(|c| matches!(c, AuditLogChange::Permissions { new, .. } if gained(None, *new).is_some())) {
                return Ok(());
            }
            app.http
                .update_role(gid, target.cast())
                .permissions(Permissions::empty())
                .await?;
            "created a role with dangerous permissions"
        }
        A::RoleUpdate
            if an.strict_mode
                || (an.monitor_public_roles && public(target.get()))
                || an.quarantine_role == Some(target.cast()) =>
        {
            let Some(old) = e.changes.iter().find_map(|c| match c {
                AuditLogChange::Permissions { old, new } => gained(*old, *new),
                _ => None,
            }) else {
                return Ok(());
            };
            app.http
                .update_role(gid, target.cast())
                .permissions(old)
                .await?;
            "gave a role dangerous permissions"
        }
        A::MemberRoleUpdate if an.strict_member_roles => {
            let info = app.guild_info(gid).await?;
            let added: Vec<Id<RoleMarker>> = e
                .changes
                .iter()
                .filter_map(|c| match c {
                    AuditLogChange::RoleAdded { new, .. } => Some(new),
                    _ => None,
                })
                .flatten()
                .map(|r| r.id)
                .filter(|r| info.role_perms.get(r).is_some_and(|p| has_dangerous(*p)))
                .collect();
            if added.is_empty() {
                return Ok(());
            }
            for r in added {
                app.http
                    .remove_guild_member_role(gid, target.cast(), r)
                    .await?;
            }
            "gave a member a role with dangerous permissions"
        }
        A::ChannelOverwriteCreate | A::ChannelOverwriteUpdate if an.monitor_channel_perms => {
            let Some(role) = e
                .options
                .as_ref()
                .and_then(|o| o.id)
                .filter(|id| public(id.get()))
            else {
                return Ok(());
            };
            let Some(old) = e.changes.iter().find_map(|c| match c {
                AuditLogChange::Allow { old, new } => gained(*old, *new),
                _ => None,
            }) else {
                return Ok(());
            };
            let channel = target.cast();
            if e.action_type == A::ChannelOverwriteCreate {
                app.http
                    .delete_channel_permission(channel)
                    .role(role.cast())
                    .await?;
            } else {
                // Keep the overwrite's current denies; only the allow change is undone.
                let deny = app
                    .http
                    .channel(channel)
                    .await?
                    .model()
                    .await?
                    .permission_overwrites
                    .unwrap_or_default()
                    .into_iter()
                    .find(|o| o.id == role.cast())
                    .map(|o| o.deny);
                let o = PermissionOverwrite {
                    id: role.cast(),
                    kind: PermissionOverwriteType::Role,
                    allow: Some(old),
                    deny,
                };
                app.http.update_channel_permission(channel, &o).await?;
            }
            "gave @everyone or a main role dangerous channel permissions"
        }
        A::GuildUpdate if an.vanity_guard => {
            let Some(old) = e.changes.iter().find_map(|c| match c {
                AuditLogChange::VanityUrlCode { old, .. } => Some(old.clone()),
                _ => None,
            }) else {
                return Ok(());
            };
            // twilight has no vanity route, so this is a raw PATCH (needs Manage Server).
            if let Some(code) = old {
                let req = RequestBuilder::raw(Method::Patch, format!("guilds/{gid}/vanity-url"))
                    .json(&json!({ "code": code }))
                    .build()?;
                if let Err(err) = app.http.request::<()>(req).await {
                    tracing::warn!(?err, "could not restore the vanity URL");
                }
            }
            "changed the vanity URL"
        }
        _ => return Ok(()),
    };
    quarantine_and_revert(app, gid, actor, why).await
}

/// Roles of `member` that carry dangerous permissions and sit below the bot, so it can take them.
fn dangerous_below_bot(
    member: &[Id<RoleMarker>],
    perms: &HashMap<Id<RoleMarker>, u64>,
    positions: &HashMap<Id<RoleMarker>, i64>,
    bot_top: i64,
) -> Vec<Id<RoleMarker>> {
    member
        .iter()
        .copied()
        .filter(|r| perms.get(r).is_some_and(|p| has_dangerous(*p)))
        .filter(|r| positions.get(r).is_some_and(|p| *p < bot_top))
        .collect()
}

/// Misc "enforce permits": a member with no permit loses the roles that give them dangerous permissions.
pub async fn enforce_permits(app: &App, m: &MemberUpdate) -> Result<()> {
    let gid = m.guild_id;
    if m.user.bot || !app.cfg(gid.get()).await?.misc.enforce_permits {
        return Ok(());
    }
    let info = app.guild_info(gid).await?;
    let bad = dangerous_below_bot(&m.roles, &info.role_perms, &info.roles, info.bot_top);
    if bad.is_empty() {
        return Ok(());
    }
    let roles: Vec<i64> = m.roles.iter().map(|r| r.get() as i64).collect();
    let level = permits::resolve(
        &app.pool,
        gid.get() as i64,
        info.owner.get() as i64,
        m.user.id.get() as i64,
        &roles,
    )
    .await?;
    if level != permits::Level::None {
        return Ok(());
    }
    for &r in &bad {
        app.http.remove_guild_member_role(gid, m.user.id, r).await?;
    }
    let names: Vec<String> = bad.iter().map(|r| format!("<@&{r}>")).collect();
    let log = Log::new("Anti Nuke", ALERT)
        .user("Member", m.user.id.get())
        .text(&format!("{YES} **Dangerous roles removed**"))
        .line("Roles", &names.join(", "))
        .line("Reason", "holds dangerous permissions without a permit");
    send_log(app, gid.get(), LogKind::AntiNuke, log).await
}

/// Strips the member's roles (saving them) so their permissions are gone.
pub(crate) async fn quarantine(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> Result<()> {
    quarantine_because(app, gid, user, None).await
}

/// [`quarantine`] with an audit log reason.
pub(crate) async fn quarantine_because(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    reason: Option<&str>,
) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let member = app.http.guild_member(gid, user).await?.model().await?;
    // Read before the row is written, so a failed read leaves no quarantine row behind.
    let managed: Vec<_> = app
        .http
        .roles(gid)
        .await?
        .models()
        .await?
        .into_iter()
        .filter(|r| r.managed)
        .map(|r| r.id)
        .collect();
    let roles = quarantine_roles(cfg.antinuke.quarantine_role, &member.roles, &managed);
    let saved: Vec<i64> = member.roles.iter().map(|r| r.get() as i64).collect();
    // Keep the first saved list when quarantined twice. Saved before the roles go, so a crash
    // between the two never loses them.
    let fresh = sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(gid.get() as i64)
        .bind(user.get() as i64)
        .bind(&saved)
        .execute(&app.pool)
        .await?
        .rows_affected()
        == 1;
    let update = app.http.update_guild_member(gid, user).roles(&roles);
    let update = match reason {
        Some(r) => update.reason(r),
        None => update,
    };
    if let Err(e) = update.await {
        // Role too low or permission missing: they are not quarantined, so no row may say so.
        if fresh {
            sqlx::query("DELETE FROM quarantines WHERE guild_id = $1 AND user_id = $2")
                .bind(gid.get() as i64)
                .bind(user.get() as i64)
                .execute(&app.pool)
                .await?;
        }
        return Err(e.into());
    }
    record_quarantine(app, gid, user).await
}

/// The role list a quarantined member keeps: the quarantine role, plus the managed (bot or
/// integration) roles they hold, as Discord refuses a list that drops one.
fn quarantine_roles(
    quarantine: Option<Id<RoleMarker>>,
    member: &[Id<RoleMarker>],
    managed: &[Id<RoleMarker>],
) -> Vec<Id<RoleMarker>> {
    quarantine
        .into_iter()
        .chain(member.iter().copied().filter(|r| managed.contains(r)))
        .collect()
}

/// Logs the bot's quarantine so the revert center can undo it. Written after the role change
/// succeeded, so a failed quarantine never leaves an unquarantine to run.
async fn record_quarantine(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<()> {
    sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,$4,$5)")
        .bind(gid.get() as i64)
        .bind(app.me.get() as i64)
        .bind("quarantine")
        .bind(user.get() as i64)
        .bind(json!(Inverse::Unquarantine { user: user.get() }))
        .execute(&app.pool)
        .await?;
    Ok(())
}

/// Quarantine without a revert (a command-rate-limit breach, a deleted verification channel).
/// A bot also loses its managed role's permissions, as in a nuke.
pub(crate) async fn quarantine_actor(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
) -> Result<Contained> {
    let done = contain(app, gid, actor).await?;
    let case = open_case(app, gid, actor, why).await?;
    let log = Log::new("Anti Nuke", ALERT)
        .line("CASE", &format!("`{case}`"))
        .user("Responsible", actor.get())
        .text(&action_text(done))
        .line("Reason", why);
    send_log(app, gid.get(), LogKind::AntiNuke, log).await?;
    Ok(done)
}

/// Leaving does not end a quarantine: a member who rejoins while their quarantine row exists is
/// contained again. True when they were.
pub(crate) async fn requarantine_rejoin(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> Result<bool> {
    let held: Option<(i64,)> =
        sqlx::query_as("SELECT 1::bigint FROM quarantines WHERE guild_id=$1 AND user_id=$2")
            .bind(gid.get() as i64)
            .bind(user.get() as i64)
            .fetch_optional(&app.pool)
            .await?;
    if held.is_none() {
        return Ok(false);
    }
    let done = contain(app, gid, user).await?;
    let log = Log::new("Anti Nuke", ALERT)
        .user("Member", user.get())
        .text(&action_text(done))
        .line("Reason", "Rejoined the server while quarantined");
    if let Err(e) = send_log(app, gid.get(), LogKind::AntiNuke, log).await {
        tracing::warn!(?e, "rejoin quarantine log failed");
    }
    Ok(true)
}

async fn open_case(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
) -> Result<i32> {
    cases::create(
        &app.pool,
        NewCase {
            guild_id: gid.get() as i64,
            kind: "quarantine",
            user_id: actor.get() as i64,
            mod_id: app.me.get() as i64,
            reason: Some(why),
            points: 0,
            duration_s: None,
            proof: None,
            inverse: Some(json!({"op": "unquarantine", "user": actor.get()})),
        },
    )
    .await
}

/// Result of undoing an actor's logged actions.
pub(crate) struct RevertSummary {
    pub ops: usize,
    pub done: Vec<Inverse>,
    pub failed: usize,
    pub reinvite: Vec<u64>,
    /// Deleted channels and roles recreated from the latest backup.
    pub recreated: usize,
}

/// Undoes the actor's unreverted logged actions since `since_secs` (unix seconds) and marks them reverted.
pub(crate) async fn revert_actor(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    since_secs: i64,
    filters: Option<&BackupsCfg>,
) -> Result<RevertSummary> {
    let since = since_secs;
    // Only the rows read here are marked reverted at the end: the actor's newer actions are not undone yet.
    let rows: Vec<(i64, serde_json::Value, i64)> = sqlx::query_as(
        "SELECT id, inverse, EXTRACT(EPOCH FROM created_at)::BIGINT FROM action_log
         WHERE guild_id=$1 AND actor_id=$2 AND NOT reverted AND inverse IS NOT NULL AND created_at >= to_timestamp($3)",
    )
    .bind(gid.get() as i64)
    .bind(actor.get() as i64)
    .bind(since)
    .fetch_all(&app.pool)
    .await?;
    let mut ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let log: Vec<LogEntry> = rows
        .into_iter()
        .filter_map(|(_, inv, at)| {
            Some(LogEntry {
                actor: actor.get(),
                at,
                inverse: serde_json::from_value(inv).ok()?,
                reverted: false,
            })
        })
        .collect();
    let mut plan = plan_revert(&log, actor.get(), since);
    if let Some(f) = filters {
        plan.ops.retain(|op| match op {
            Inverse::DeleteRole { .. } => f.restore && f.delete_roles,
            Inverse::DeleteChannel { .. } => f.restore && f.delete_channels,
            Inverse::DeleteWebhook { .. } => f.restore && f.delete_webhooks,
            _ => true,
        });
    }
    let mut failed = 0;
    for op in &plan.ops {
        // Already undone (a revert cut off by a restart runs again) or the target is gone.
        match run_inverse(app, gid, op).await {
            Err(e) if !crate::http_err::gone_any(&e) => {
                tracing::warn!(?e, ?op, "revert step failed");
                failed += 1;
            }
            _ => {}
        }
    }
    // Deleted channels and roles have no logged inverse; they come back from the latest backup.
    let gone: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id, target_id FROM action_log WHERE guild_id=$1 AND actor_id=$2 AND NOT reverted
         AND action IN ('ChannelDelete','RoleDelete') AND target_id IS NOT NULL AND created_at >= to_timestamp($3)",
    )
    .bind(gid.get() as i64)
    .bind(actor.get() as i64)
    .bind(since)
    .fetch_all(&app.pool)
    .await?;
    ids.extend(gone.iter().map(|g| g.0));
    let gone: std::collections::HashSet<u64> = gone.into_iter().map(|(_, t)| t as u64).collect();
    let recreated = if gone.is_empty() {
        0
    } else {
        backup::recreate(app, gid, &gone, filters)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(?e, "recreating deleted objects failed");
                0
            })
    };
    sqlx::query("UPDATE action_log SET reverted = true WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&app.pool)
        .await?;
    Ok(RevertSummary {
        ops: plan.ops.len(),
        done: plan.ops,
        failed,
        reinvite: plan.reinvite,
        recreated,
    })
}

/// How a nuker was dealt with, for the report and the nuke case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    /// Auto quarantine is off.
    Off,
    Failed,
    Quarantined,
    /// A bot that could not be quarantined.
    Kicked,
}

/// What [`contain`] did; `bot_role` is set for a bot with its own managed role: true when that
/// role's permissions were cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Contained {
    pub applied: Applied,
    pub bot_role: Option<bool>,
}

impl Contained {
    /// Quarantined or kicked: the nuker can do no more.
    fn stopped(&self) -> bool {
        matches!(self.applied, Applied::Quarantined | Applied::Kicked)
    }
}

const BOT_ROLE_REASON: &str = "Anti-nuke: this bot nuked the server";

/// Quarantines a nuker. A bot first loses its managed role's permissions (the role cannot be
/// taken off it) and is kicked only when that or the quarantine fails.
async fn contain(app: &App, gid: Id<GuildMarker>, actor: Id<UserMarker>) -> Result<Contained> {
    let member = app.http.guild_member(gid, actor).await?.model().await?;
    if !member.user.bot {
        quarantine(app, gid, actor).await?;
        return Ok(Contained {
            applied: Applied::Quarantined,
            bot_role: None,
        });
    }
    let role = bot_role(app, gid, actor).await;
    let found = role.as_ref().ok().copied().flatten();
    let done = contain_bot(
        role,
        |(id, _)| async move {
            app.http
                .update_role(gid, id)
                .permissions(Permissions::empty())
                .reason(BOT_ROLE_REASON)
                .await
                .map(drop)
                .map_err(Into::into)
        },
        || quarantine(app, gid, actor),
        || async move {
            app.http
                .remove_guild_member(gid, actor)
                .reason(BOT_ROLE_REASON)
                .await
                .map(drop)
                .map_err(Into::into)
        },
    )
    .await?;
    // Logged only once the bot stays: a kick deletes its managed role, leaving nothing to restore.
    if let (Applied::Quarantined, Some(true), Some((id, perms))) =
        (done.applied, done.bot_role, found)
    {
        // The bot is already contained; a missing revert row must not report it as free.
        if let Err(e) = record_bot_role(app, gid, id, perms).await {
            tracing::warn!(?e, "logging the cleared bot role failed");
        }
    }
    Ok(done)
}

/// A bot loses its managed role's permissions, then is quarantined. A failed step, or a failed
/// lookup of its role, ends in a kick.
async fn contain_bot<R, C, Q, K>(
    role: Result<Option<R>>,
    clear: impl FnOnce(R) -> C,
    quarantine: impl FnOnce() -> Q,
    kick: impl FnOnce() -> K,
) -> Result<Contained>
where
    C: Future<Output = Result<()>>,
    Q: Future<Output = Result<()>>,
    K: Future<Output = Result<()>>,
{
    let mut bot_role = None;
    let steps: Result<()> = async {
        if let Some(r) = role? {
            bot_role = Some(false);
            clear(r).await?;
            bot_role = Some(true);
        }
        quarantine().await
    }
    .await;
    let applied = match steps {
        Ok(()) => Applied::Quarantined,
        Err(e) => {
            tracing::warn!(?e, "containing a bot failed, kicking it");
            kick().await?;
            Applied::Kicked
        }
    };
    Ok(Contained { applied, bot_role })
}

/// The bot's own managed role (Discord tags it with the bot's id) and its permissions.
async fn bot_role(
    app: &App,
    gid: Id<GuildMarker>,
    bot: Id<UserMarker>,
) -> Result<Option<(Id<RoleMarker>, u64)>> {
    let roles = app.http.roles(gid).await?.models().await?;
    Ok(roles
        .into_iter()
        .find(|r| r.tags.as_ref().and_then(|t| t.bot_id) == Some(bot))
        .map(|r| (r.id, r.permissions.bits())))
}

/// Logs the bot role's old permissions, so the revert center can put them back.
// shortcut: twilight drops permission bits newer than it knows; read the raw string if Discord adds some.
async fn record_bot_role(
    app: &App,
    gid: Id<GuildMarker>,
    role: Id<RoleMarker>,
    perms: u64,
) -> Result<()> {
    sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,$4,$5)")
        .bind(gid.get() as i64)
        .bind(app.me.get() as i64)
        .bind("bot_role_cleared")
        .bind(role.get() as i64)
        .bind(json!(Inverse::RolePerms { id: role.get(), perms }))
        .execute(&app.pool)
        .await?;
    Ok(())
}

/// A restart mid-revert would leave the nuke half undone: the `nuke_revert` guard job runs it again
/// after a restart, and is dropped once this run is over.
async fn quarantine_and_revert(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
) -> Result<()> {
    let key = (gid.get(), actor.get());
    if !claim_nuke_run(app, key) {
        // Every event past the limit lands here while the first run is busy; its action is
        // logged, and the running revert picks it up before it ends.
        return Ok(());
    }
    let r = async {
        let since = now_secs() - 3600;
        let payload = json!({"actor": actor.get(), "why": why, "since": since});
        let guard = jobs::enqueue(
            &app.pool,
            gid.get() as i64,
            "nuke_revert",
            payload,
            now_secs() + 120,
        )
        .await?;
        // An actor an earlier run already quarantined only has late audit entries left: revert
        // them without a second case and owner alert.
        let full = !is_quarantined(app, gid, actor).await?;
        let r = nuke_run(app, gid, actor, why, since, full).await;
        jobs::finish(&app.pool, guard).await?;
        r
    }
    .await;
    app.nuke_runs.lock().unwrap().remove(&key);
    r
}

/// The `nuke_revert` guard job after a restart: the cut-off run again, under the same claim, so a
/// live run started meanwhile is not doubled.
pub(crate) async fn resume_nuke(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
    since: i64,
) -> Result<()> {
    let key = (gid.get(), actor.get());
    if !claim_nuke_run(app, key) {
        return Ok(());
    }
    let r = nuke_run(app, gid, actor, why, since, true).await;
    app.nuke_runs.lock().unwrap().remove(&key);
    r
}

/// True while a quarantine and revert for this actor runs in this process.
pub(crate) fn nuke_running(app: &App, gid: u64, actor: u64) -> bool {
    app.nuke_runs.lock().unwrap().contains_key(&(gid, actor))
}

async fn is_quarantined(app: &App, gid: Id<GuildMarker>, actor: Id<UserMarker>) -> Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM quarantines WHERE guild_id=$1 AND user_id=$2")
            .bind(gid.get() as i64)
            .bind(actor.get() as i64)
            .fetch_optional(&app.pool)
            .await?
            .is_some(),
    )
}

/// One claimed run: the full contain, revert, report and panic, or only the revert, then again
/// for every action that came in meanwhile.
async fn nuke_run(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
    since: i64,
    full: bool,
) -> Result<()> {
    let mut first = full;
    loop {
        let r = if first {
            revert_and_report(app, gid, actor, why, since).await
        } else {
            let backups = app.cfg(gid.get()).await?.backups.clone();
            revert_actor(app, gid, actor, since, Some(&backups))
                .await
                .map(|s| {
                    tracing::info!(
                        ops = s.ops,
                        recreated = s.recreated,
                        "reverted the nuker's later actions"
                    )
                })
        };
        if r.is_err() || !nuke_rerun(app, (gid.get(), actor.get())) {
            return r;
        }
        first = false;
    }
}

/// True when no quarantine and revert runs for this (guild, actor) yet; otherwise asks the running
/// one to revert once more.
fn claim_nuke_run(app: &App, key: (u64, u64)) -> bool {
    claim_run(&mut app.nuke_runs.lock().unwrap(), key)
}

/// True when actions came in during the run (and clears that); false ends the run and frees the
/// claim in the same step, so a later action starts a new run instead of being missed.
fn nuke_rerun(app: &App, key: (u64, u64)) -> bool {
    rerun(&mut app.nuke_runs.lock().unwrap(), key)
}

fn claim_run(runs: &mut HashMap<(u64, u64), bool>, key: (u64, u64)) -> bool {
    match runs.get_mut(&key) {
        Some(again) => {
            *again = true;
            false
        }
        None => {
            runs.insert(key, false);
            true
        }
    }
}

fn rerun(runs: &mut HashMap<(u64, u64), bool>, key: (u64, u64)) -> bool {
    if runs.get(&key) == Some(&true) {
        runs.insert(key, false);
        true
    } else {
        runs.remove(&key);
        false
    }
}

pub(crate) async fn revert_and_report(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
    since: i64,
) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let mut done = Contained {
        applied: Applied::Off,
        bot_role: None,
    };
    let mut sum = None;
    for step in nuke_plan(cfg.antinuke.auto_quarantine) {
        match step {
            // A failed quarantine (bot role too low) must not stop the revert.
            NukeStep::Contain => {
                done = contain(app, gid, actor).await.unwrap_or_else(|e| {
                    tracing::warn!(?e, "quarantine failed");
                    Contained {
                        applied: Applied::Failed,
                        bot_role: None,
                    }
                })
            }
            NukeStep::Revert => {
                sum = Some(revert_actor(app, gid, actor, since, Some(&cfg.backups)).await?)
            }
            NukeStep::Report => {
                let sum = sum.as_ref().context("the revert runs before the report")?;
                // A failed report must not keep panic mode from starting.
                if let Err(e) = report(app, gid, actor, why, done, sum).await {
                    tracing::warn!(?e, "anti-nuke report failed");
                }
            }
            NukeStep::Panic => crate::panic::note(app, gid, actor, false).await?,
        }
    }
    Ok(())
}

async fn report(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
    done: Contained,
    sum: &RevertSummary,
) -> Result<()> {
    let case = open_case(app, gid, actor, why).await?;
    let nuke = record_nuke(app, gid, actor, &sum.done, done.stopped()).await?;
    crate::raiders::nuked(app, gid, actor, (case, &nuke, sum.ops), done.stopped()).await;
    let log = report_log((case, &nuke), actor.get(), done, why, sum);
    send_log(app, gid.get(), LogKind::AntiNuke, log).await?;
    crate::reply::alert_owner(app, gid, "A nuke attempt was stopped").await;
    Ok(())
}

/// What happened to the nuker, as the log's action line.
fn action_text(done: Contained) -> String {
    match (done.applied, done.bot_role) {
        (Applied::Quarantined, Some(true)) => format!("{YES} **Quarantined**, bot role cleared"),
        (Applied::Quarantined, _) => format!("{YES} **Quarantined**"),
        (Applied::Kicked, _) => format!("{YES} **Kicked** (the bot could not be quarantined)"),
        (Applied::Failed, _) => {
            format!("{NO} **NOT quarantined** (check that the bot's role is on top)")
        }
        (Applied::Off, _) => format!("{NO} **Not quarantined** (auto quarantine is off)"),
    }
}

/// The log after a nuke is stopped and reverted.
pub(crate) fn report_log(
    (case, nuke): (i32, &str),
    actor: u64,
    done: Contained,
    why: &str,
    sum: &RevertSummary,
) -> Log {
    let action = action_text(done);
    let reinvite = if sum.reinvite.is_empty() {
        "None".into()
    } else {
        sum.reinvite
            .iter()
            .map(|u| format!("<@{u}>"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mark = |b: bool| if b { YES } else { NO }.to_string();
    let mut log = Log::new("Anti Nuke Report:", ALERT)
        .line("CASE", &format!("`{case}` (nuke `{nuke}`)"))
        .user("Responsible", actor)
        .text(&action)
        .line("Reason", why)
        .section("More Details:")
        .line("Action Applied", &mark(done.stopped()));
    if let Some(cleared) = done.bot_role {
        log = log.line("Bot Role Cleansing", &mark(cleared));
    }
    log.section("Revert Report:")
        .line(
            "Reverted",
            &format!("`{}/{}`", sum.ops - sum.failed, sum.ops),
        )
        .line("Recreated", &format!("`{}`", sum.recreated))
        .line("Kicked, re-invite", &reinvite)
}

/// A quiet gap this long starts a new nuke case instead of extending the last one.
const NUKE_GAP: Duration = Duration::from_secs(600);

/// Adds the quarantined actor and their reverted ops to the guild's open nuke case and saves it.
async fn record_nuke(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    ops: &[Inverse],
    contained: bool,
) -> Result<String> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64
        ^ gid.get();
    let case = {
        let mut open = app.nukes.lock().unwrap();
        let entry = open
            .entry(gid.get())
            .or_insert_with(|| (NukeCase::new(seed), Instant::now()));
        if entry.1.elapsed() > NUKE_GAP {
            *entry = (NukeCase::new(seed), Instant::now());
        }
        entry.1 = Instant::now();
        entry.0.add(actor.get(), ops.iter().cloned(), contained);
        entry.0.clone()
    };
    sqlx::query(
        "INSERT INTO nuke_incidents (guild_id, case_id, data) VALUES ($1,$2,$3)
         ON CONFLICT (guild_id, case_id) DO UPDATE SET data = $3",
    )
    .bind(gid.get() as i64)
    .bind(&case.id)
    .bind(sqlx::types::Json(&case))
    .execute(&app.pool)
    .await?;
    Ok(case.id)
}

pub(crate) async fn run_inverse(app: &App, gid: Id<GuildMarker>, op: &Inverse) -> Result<()> {
    match op {
        Inverse::Unban { user } => {
            app.http.delete_ban(gid, Id::new(*user)).await?;
        }
        Inverse::DeleteChannel { id } => {
            app.http.delete_channel(Id::new(*id)).await?;
        }
        Inverse::DeleteRole { id } => {
            app.http.delete_role(gid, Id::new(*id)).await?;
        }
        Inverse::DeleteWebhook { id } => {
            app.http.delete_webhook(Id::new(*id)).await?;
        }
        Inverse::RestoreRoles { user, roles } => {
            let roles: Vec<Id<RoleMarker>> = roles.iter().map(|r| Id::new(*r)).collect();
            app.http
                .update_guild_member(gid, Id::new(*user))
                .roles(&roles)
                .await?;
        }
        Inverse::Unquarantine { user } => {
            let saved: Option<(Vec<i64>,)> = sqlx::query_as(
                "SELECT saved_roles FROM quarantines WHERE guild_id=$1 AND user_id=$2",
            )
            .bind(gid.get() as i64)
            .bind(*user as i64)
            .fetch_optional(&app.pool)
            .await?;
            if let Some((saved,)) = saved {
                let roles: Vec<Id<RoleMarker>> = saved.iter().map(|r| Id::new(*r as u64)).collect();
                app.http
                    .update_guild_member(gid, Id::new(*user))
                    .roles(&roles)
                    .await?;
                sqlx::query("DELETE FROM quarantines WHERE guild_id=$1 AND user_id=$2")
                    .bind(gid.get() as i64)
                    .bind(*user as i64)
                    .execute(&app.pool)
                    .await?;
            }
        }
        Inverse::Ban { user } => {
            app.http.create_ban(gid, Id::new(*user)).await?;
        }
        Inverse::Timeout { user, until } => {
            crate::commands::timeout_until(app, gid, Id::new(*user), *until, "Restored timeout")
                .await?;
        }
        Inverse::RolePerms { id, perms } => {
            app.http
                .update_role(gid, Id::new(*id))
                .permissions(Permissions::from_bits_truncate(*perms))
                .await?;
        }
        // Discord cannot undo a kick, so this is reported for re-invite, not executed.
        Inverse::Reinvite { .. } => {}
    }
    Ok(())
}

/// Someone changed a quarantined member's roles: quarantine them too and undo the change.
async fn hold(app: &App, e: &AuditLogEntry, actor: Id<UserMarker>) -> Result<()> {
    let (Some(gid), Some(target)) = (e.guild_id, e.target_id) else {
        return Ok(());
    };
    let target = target.cast::<UserMarker>();
    let saved: Option<(Vec<i64>,)> =
        sqlx::query_as("SELECT saved_roles FROM quarantines WHERE guild_id=$1 AND user_id=$2")
            .bind(gid.get() as i64)
            .bind(target.get() as i64)
            .fetch_optional(&app.pool)
            .await?;
    let Some((saved,)) = saved else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    let whitelisted = cfg.antinuke.quarantine_whitelist.contains(&actor)
        || app.whitelisted(gid, actor, "quarantine").await?;
    if !hold_violation(actor.get(), target.get(), true, whitelisted) {
        return Ok(());
    }
    // A join quarantine (verification, nothing saved) keeps the joiner held, but an autorole bot
    // or staff touching them is normal work, not a nuke (Thomas, 11-11).
    if saved.is_empty() {
        if e.action_type == A::MemberRoleUpdate {
            let roles: Vec<_> = cfg.antinuke.quarantine_role.into_iter().collect();
            app.http
                .update_guild_member(gid, target)
                .roles(&roles)
                .await?;
        }
        return Ok(());
    }
    if e.action_type != A::MemberRoleUpdate {
        return quarantine_and_revert(app, gid, actor, "touched a quarantined member").await;
    }
    let roles: Vec<_> = cfg.antinuke.quarantine_role.into_iter().collect();
    app.http
        .update_guild_member(gid, target)
        .roles(&roles)
        .await?;
    quarantine_and_revert(app, gid, actor, "changed the roles of a quarantined member").await
}

/// Whether anti-nuke quarantined `actor` or put them in a nuke case since `since` (unix secs), so
/// Ban Sync does not spread what a hacked staff account did. With `None`, any nuke case or the
/// quarantine of someone who could ban, kick or quarantine (`Level::has("quarantine")`) counts:
/// a quarantine on join or by `t!quarantine` hits members a raider can provoke. A nuke case keeps
/// the time it opened, so cases opened up to [`NUKE_GAP`] earlier count too.
pub(crate) async fn contained_since(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Option<Id<UserMarker>>,
    since: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM quarantines q WHERE q.guild_id = $1
                          AND q.created_at >= to_timestamp($3::float8)
                          AND CASE WHEN $2::bigint IS NULL THEN
                                EXISTS (SELECT 1 FROM extra_owners WHERE guild_id = $1 AND user_id = q.user_id)
                                OR EXISTS (SELECT 1 FROM trusted_admins WHERE guild_id = $1 AND user_id = q.user_id)
                                OR EXISTS (SELECT 1 FROM permit_roles r JOIN permits p ON p.id = r.permit_id
                                           WHERE r.guild_id = $1 AND r.role_id = ANY(q.saved_roles)
                                             AND p.perms && ARRAY['ban', 'kick', 'quarantine'])
                              ELSE q.user_id = $2 END)
             OR EXISTS (SELECT 1 FROM nuke_incidents WHERE guild_id = $1
                          AND created_at >= to_timestamp($3::float8) - make_interval(secs => $4)
                          AND ($2::bigint IS NULL OR data->'quarantined' @> to_jsonb($2::bigint)
                               OR data->'failed' @> to_jsonb($2::bigint)))",
    )
    .bind(gid.get() as i64)
    .bind(actor.map(|a| a.get() as i64))
    .bind(since as f64)
    .bind(NUKE_GAP.as_secs_f64())
    .fetch_one(&app.pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_nuke_run_per_actor_and_late_actions_rerun_it() {
        let mut runs = HashMap::new();
        let k = (1, 2);
        assert!(claim_run(&mut runs, k));
        assert!(!claim_run(&mut runs, k)); // a second event while it runs
        assert!(claim_run(&mut runs, (1, 3))); // another actor runs on its own
        assert!(rerun(&mut runs, k)); // that event is reverted once more
        assert!(!rerun(&mut runs, k)); // nothing new: the claim is freed
        assert!(claim_run(&mut runs, k)); // so a later action starts a new run
    }

    #[test]
    fn quarantine_keeps_only_the_quarantine_role_and_managed_roles() {
        let id = |n| Id::<RoleMarker>::new(n);
        let got = quarantine_roles(Some(id(9)), &[id(1), id(2), id(3)], &[id(2), id(7)]);
        assert_eq!(got, vec![id(9), id(2)]);
        assert!(quarantine_roles(None, &[id(1)], &[]).is_empty());
    }

    /// Runs the bot steps with fake calls; `fail` names the call that errors.
    async fn bot_run(role: Result<Option<u8>>, fail: &str) -> (Contained, Vec<&'static str>) {
        let calls = std::cell::RefCell::new(vec![]);
        let call = |name: &'static str| {
            calls.borrow_mut().push(name);
            if name == fail {
                anyhow::bail!("{name} failed")
            }
            Ok(())
        };
        let done = contain_bot(
            role,
            |_| std::future::ready(call("clear")),
            || std::future::ready(call("quarantine")),
            || std::future::ready(call("kick")),
        )
        .await
        .unwrap();
        (done, calls.into_inner())
    }

    #[tokio::test]
    async fn a_bot_is_kicked_only_when_clearing_or_quarantine_fails() {
        let c = |applied, bot_role| Contained { applied, bot_role };
        assert_eq!(
            bot_run(Ok(Some(1)), "").await,
            (
                c(Applied::Quarantined, Some(true)),
                vec!["clear", "quarantine"]
            )
        );
        assert_eq!(
            bot_run(Ok(None), "").await,
            (c(Applied::Quarantined, None), vec!["quarantine"])
        );
        assert_eq!(
            bot_run(Ok(Some(1)), "clear").await,
            (c(Applied::Kicked, Some(false)), vec!["clear", "kick"])
        );
        assert_eq!(
            bot_run(Ok(Some(1)), "quarantine").await,
            (
                c(Applied::Kicked, Some(true)),
                vec!["clear", "quarantine", "kick"]
            )
        );
        // The bot's role could not be read: it may still hold it, so it is kicked.
        assert_eq!(
            bot_run(Err(anyhow::anyhow!("rate limited")), "").await,
            (c(Applied::Kicked, None), vec!["kick"])
        );
    }

    #[test]
    fn only_dangerous_roles_below_the_bot_are_taken() {
        let id = |n| Id::<RoleMarker>::new(n);
        let perms = HashMap::from([(id(1), 1 << 3), (id(2), 1 << 10), (id(3), 1 << 3)]);
        let positions = HashMap::from([(id(1), 2), (id(2), 3), (id(3), 9)]);
        let got = dangerous_below_bot(&[id(1), id(2), id(3), id(4)], &perms, &positions, 5);
        assert_eq!(got, vec![id(1)]);
    }

    #[test]
    fn timeouts_and_role_renames_are_watched_other_updates_not() {
        let entry = |t: u16, changes: serde_json::Value| -> AuditLogEntry {
            serde_json::from_value(json!({
                "action_type": t, "changes": changes, "id": "1", "user_id": "2", "target_id": "3"
            }))
            .unwrap()
        };
        let timeout = json!([{"key": "communication_disabled_until", "new_value": "2030-01-01T00:00:00+00:00"}]);
        let lifted = json!([{"key": "communication_disabled_until", "old_value": "2030-01-01T00:00:00+00:00"}]);
        let rename = json!([{"key": "name", "old_value": "a", "new_value": "b"}]);
        assert_eq!(watched(&entry(24, timeout)), Some(Action::Timeout));
        assert_eq!(watched(&entry(24, lifted)), None);
        assert_eq!(watched(&entry(31, rename)), Some(Action::RoleUpdate));
        assert_eq!(watched(&entry(31, json!([]))), None);
        assert_eq!(watched(&entry(23, json!([]))), Some(Action::Unban));
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres. A row the revert did not read
    /// (here one without an inverse, as an action logged while it ran) stays unreverted.
    #[tokio::test]
    async fn revert_marks_only_the_rows_it_read() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        );
        let gid = Id::new(6_400_000_000 + std::process::id() as u64);
        let actor = Id::new(42);
        for (action, inv) in [
            ("Kick", Some(json!(Inverse::Reinvite { user: 7 }))),
            ("ChannelUpdate", None),
        ] {
            sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,7,$4)")
                .bind(gid.get() as i64)
                .bind(actor.get() as i64)
                .bind(action)
                .bind(inv)
                .execute(&app.pool)
                .await
                .unwrap();
        }
        let sum = revert_actor(&app, gid, actor, now_secs() - 60, None)
            .await
            .unwrap();
        assert_eq!(sum.reinvite, vec![7]);
        let left: Vec<(String,)> = sqlx::query_as(
            "SELECT action FROM action_log WHERE guild_id=$1 AND NOT reverted ORDER BY id",
        )
        .bind(gid.get() as i64)
        .fetch_all(&app.pool)
        .await
        .unwrap();
        assert_eq!(left, vec![("ChannelUpdate".to_string(),)]);
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn nuke_case_grows_and_is_saved() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        );
        let gid = Id::new(6_000_000_000 + std::process::id() as u64);
        let first = record_nuke(
            &app,
            gid,
            Id::new(10),
            &[Inverse::DeleteChannel { id: 5 }],
            true,
        )
        .await
        .unwrap();
        let second = record_nuke(&app, gid, Id::new(11), &[Inverse::Unban { user: 7 }], false)
            .await
            .unwrap();
        assert_eq!(first, second);
        let (data,): (serde_json::Value,) =
            sqlx::query_as("SELECT data FROM nuke_incidents WHERE guild_id=$1 AND case_id=$2")
                .bind(gid.get() as i64)
                .bind(&first)
                .fetch_one(&app.pool)
                .await
                .unwrap();
        assert_eq!(data["quarantined"], serde_json::json!([10]));
        assert_eq!(data["failed"], serde_json::json!([11]));
        assert_eq!(data["reverted"].as_array().unwrap().len(), 2);
        sqlx::query("DELETE FROM nuke_incidents WHERE guild_id=$1")
            .bind(gid.get() as i64)
            .execute(&app.pool)
            .await
            .unwrap();
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn quarantine_record_inverse_roundtrips() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        );
        let gid = Id::new(6_100_000_000 + std::process::id() as u64);
        record_quarantine(&app, gid, Id::new(42)).await.unwrap();
        let (actor, inv): (i64, serde_json::Value) = sqlx::query_as(
            "SELECT actor_id, inverse FROM action_log WHERE guild_id=$1 AND action='quarantine'",
        )
        .bind(gid.get() as i64)
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(actor, 1);
        assert_eq!(
            serde_json::from_value::<Inverse>(inv).unwrap(),
            Inverse::Unquarantine { user: 42 }
        );
        sqlx::query("DELETE FROM action_log WHERE guild_id=$1")
            .bind(gid.get() as i64)
            .execute(&app.pool)
            .await
            .unwrap();
    }
}
