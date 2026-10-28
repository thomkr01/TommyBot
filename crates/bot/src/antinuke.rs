//! Audit-log driven anti-nuke: count watched actions per executor, quarantine and revert on a breach,
//! and enforce the quarantine hold.
use anyhow::Result;
use serde_json::json;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tommybot_antinuke::{
    gained_dangerous, has_dangerous, hold_violation, plan_revert, Action, Inverse, LogEntry,
    NukeCase, Verdict,
};
use tommybot_automod::heat::{Breakdown, HeatCfg};
use tommybot_core::{
    cases::{self, NewCase},
    permits,
};
use tommybot_logging::{
    embed::{ALERT, NO, YES},
    Log, LogKind,
};
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

fn watched(t: A) -> Option<Action> {
    Some(match t {
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
        return guard_perms(app, &cfg, e, actor).await;
    }
    let Some(action) = watched(e.action_type) else {
        return Ok(());
    };
    let Some((_, heat)) = cfg.antinuke.limit(action) else {
        return Ok(());
    };
    // Ticket bots open and close channels all day; the gateway event names the category first.
    if matches!(action, Action::ChannelCreate | Action::ChannelDelete)
        && e.target_id
            .and_then(|t| app.channel_parents.lock().unwrap().remove(&t.get()))
            .is_some_and(|p| cfg.antinuke.ticket_categories.iter().any(|c| c.get() == p))
    {
        return Ok(());
    }
    let exempt = app.whitelisted(gid, actor, whitelist_flag(action)).await?;
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
        || an.vanity_guard;
    if !guarded || app.whitelisted(gid, actor, "antinuke").await? {
        return Ok(());
    }
    let public = |id: u64| id == gid.get() || cfg.lockdown.main_roles.iter().any(|r| r.get() == id);
    let gained = |old: Option<Permissions>, new: Option<Permissions>| {
        let bits = |p: Option<Permissions>| p.map_or(0, |p| p.bits());
        gained_dangerous(bits(old), bits(new)).then(|| old.unwrap_or(Permissions::empty()))
    };
    let why = match e.action_type {
        A::RoleUpdate if an.strict_mode || (an.monitor_public_roles && public(target.get())) => {
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
            let changed = e
                .changes
                .iter()
                .any(|c| matches!(c, AuditLogChange::VanityUrlCode { .. }));
            if !changed {
                return Ok(());
            }
            "changed the vanity URL (bots cannot set it back, do it by hand)"
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
    let cfg = app.cfg(gid.get()).await?;
    let member = app.http.guild_member(gid, user).await?.model().await?;
    let saved: Vec<i64> = member.roles.iter().map(|r| r.get() as i64).collect();
    // Keep the first saved list when quarantined twice.
    sqlx::query("INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(gid.get() as i64)
        .bind(user.get() as i64)
        .bind(&saved)
        .execute(&app.pool)
        .await?;
    let roles: Vec<_> = cfg.antinuke.quarantine_role.into_iter().collect();
    app.http
        .update_guild_member(gid, user)
        .roles(&roles)
        .await?;
    record_quarantine(app, gid, user).await
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

/// Quarantine without a revert, for a command-rate-limit breach.
pub(crate) async fn quarantine_actor(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
) -> Result<()> {
    quarantine(app, gid, actor).await?;
    let case = open_case(app, gid, actor, why).await?;
    let log = Log::new("Anti Nuke", ALERT)
        .line("CASE", &format!("`{case}`"))
        .user("Responsible", actor.get())
        .text(&format!("{YES} **Quarantined**"))
        .line("Reason", why);
    send_log(app, gid.get(), LogKind::AntiNuke, log).await
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
) -> Result<RevertSummary> {
    let since = since_secs;
    let rows: Vec<(serde_json::Value, i64)> = sqlx::query_as(
        "SELECT inverse, EXTRACT(EPOCH FROM created_at)::BIGINT FROM action_log
         WHERE guild_id=$1 AND actor_id=$2 AND NOT reverted AND inverse IS NOT NULL AND created_at >= to_timestamp($3)",
    )
    .bind(gid.get() as i64)
    .bind(actor.get() as i64)
    .bind(since)
    .fetch_all(&app.pool)
    .await?;
    let log: Vec<LogEntry> = rows
        .into_iter()
        .filter_map(|(inv, at)| {
            Some(LogEntry {
                actor: actor.get(),
                at,
                inverse: serde_json::from_value(inv).ok()?,
                reverted: false,
            })
        })
        .collect();
    let plan = plan_revert(&log, actor.get(), since);
    let mut failed = 0;
    for op in &plan.ops {
        if let Err(e) = run_inverse(app, gid, op).await {
            tracing::warn!(?e, ?op, "revert step failed");
            failed += 1;
        }
    }
    // Deleted channels and roles have no logged inverse; they come back from the latest backup.
    let gone: Vec<(i64,)> = sqlx::query_as(
        "SELECT target_id FROM action_log WHERE guild_id=$1 AND actor_id=$2 AND NOT reverted
         AND action IN ('ChannelDelete','RoleDelete') AND target_id IS NOT NULL AND created_at >= to_timestamp($3)",
    )
    .bind(gid.get() as i64)
    .bind(actor.get() as i64)
    .bind(since)
    .fetch_all(&app.pool)
    .await?;
    let gone: std::collections::HashSet<u64> = gone.into_iter().map(|(id,)| id as u64).collect();
    let recreated = if gone.is_empty() {
        0
    } else {
        backup::recreate(app, gid, &gone).await.unwrap_or_else(|e| {
            tracing::warn!(?e, "recreating deleted objects failed");
            0
        })
    };
    sqlx::query("UPDATE action_log SET reverted = true WHERE guild_id=$1 AND actor_id=$2 AND created_at >= to_timestamp($3)")
        .bind(gid.get() as i64)
        .bind(actor.get() as i64)
        .bind(since)
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

/// Quarantines a nuking member; a nuking bot is kicked, as its managed role cannot be removed.
async fn contain(app: &App, gid: Id<GuildMarker>, actor: Id<UserMarker>) -> Result<()> {
    let member = app.http.guild_member(gid, actor).await?.model().await?;
    if member.user.bot {
        app.http.remove_guild_member(gid, actor).await?;
        return Ok(());
    }
    quarantine(app, gid, actor).await
}

async fn quarantine_and_revert(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
) -> Result<()> {
    let auto = app.cfg(gid.get()).await?.antinuke.auto_quarantine;
    // A failed quarantine (bot role too low) must not stop the revert.
    let quarantined = auto
        && contain(app, gid, actor)
            .await
            .inspect_err(|e| tracing::warn!(?e, "quarantine failed"))
            .is_ok();
    let sum = revert_actor(app, gid, actor, now_secs() - 3600).await?;
    let case = open_case(app, gid, actor, why).await?;
    let nuke = record_nuke(app, gid, actor, &sum.done).await?;
    let reinvite: Vec<String> = sum.reinvite.iter().map(|u| format!("<@{u}>")).collect();
    let action = if quarantined {
        format!("{YES} **Quarantined** (bots are kicked)")
    } else if auto {
        format!("{NO} **NOT quarantined** (check that the bot's role is on top)")
    } else {
        format!("{NO} **Not quarantined** (auto quarantine is off)")
    };
    let reinvite = if reinvite.is_empty() {
        "None".into()
    } else {
        reinvite.join(" ")
    };
    let log = Log::new("Anti Nuke Report:", ALERT)
        .line("CASE", &format!("`{case}` (nuke `{nuke}`)"))
        .user("Responsible", actor.get())
        .text(&action)
        .line("Reason", why)
        .section("Revert Report:")
        .line(
            "Reverted",
            &format!("`{}/{}`", sum.ops - sum.failed, sum.ops),
        )
        .line("Recreated", &format!("`{}`", sum.recreated))
        .line("Kicked, re-invite", &reinvite);
    send_log(app, gid.get(), LogKind::AntiNuke, log).await?;
    if !auto {
        return Ok(());
    }
    crate::panic::note(app, gid, actor, false).await
}

/// A quiet gap this long starts a new nuke case instead of extending the last one.
const NUKE_GAP: Duration = Duration::from_secs(600);

/// Adds the quarantined actor and their reverted ops to the guild's open nuke case and saves it.
async fn record_nuke(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    ops: &[Inverse],
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
        entry.0.add(actor.get(), ops.iter().cloned());
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
            crate::commands::timeout_until(app, gid, Id::new(*user), *until).await?;
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
    if saved.is_none() {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let whitelisted = cfg.antinuke.quarantine_whitelist.contains(&actor)
        || app.whitelisted(gid, actor, "quarantine").await?;
    if !hold_violation(actor.get(), target.get(), true, whitelisted) {
        return Ok(());
    }
    let roles: Vec<_> = cfg.antinuke.quarantine_role.into_iter().collect();
    app.http
        .update_guild_member(gid, target)
        .roles(&roles)
        .await?;
    quarantine_and_revert(app, gid, actor, "changed the roles of a quarantined member").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dangerous_roles_below_the_bot_are_taken() {
        let id = |n| Id::<RoleMarker>::new(n);
        let perms = HashMap::from([(id(1), 1 << 3), (id(2), 1 << 10), (id(3), 1 << 3)]);
        let positions = HashMap::from([(id(1), 2), (id(2), 3), (id(3), 9)]);
        let got = dangerous_below_bot(&[id(1), id(2), id(3), id(4)], &perms, &positions, 5);
        assert_eq!(got, vec![id(1)]);
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
        let first = record_nuke(&app, gid, Id::new(10), &[Inverse::DeleteChannel { id: 5 }])
            .await
            .unwrap();
        let second = record_nuke(&app, gid, Id::new(11), &[Inverse::Unban { user: 7 }])
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
        assert_eq!(data["quarantined"], serde_json::json!([10, 11]));
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
