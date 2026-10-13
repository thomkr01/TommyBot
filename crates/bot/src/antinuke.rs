//! Audit-log driven anti-nuke: count watched actions per executor, quarantine and revert on a breach,
//! and enforce the quarantine hold.
use anyhow::Result;
use serde_json::json;
use std::time::Instant;
use tommybot_antinuke::{hold_violation, plan_revert, Action, Inverse, LogEntry, Verdict};
use tommybot_core::cases::{self, NewCase};
use tommybot_logging::LogKind;
use twilight_model::{
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    id::{marker::UserMarker, Id},
};

use crate::{app::App, events::send_log};

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

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (Some(gid), Some(actor)) = (e.guild_id, e.user_id) else {
        return Ok(());
    };
    if actor == app.me {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    if e.action_type == A::MemberRoleUpdate {
        return hold(app, e, actor).await;
    }
    let Some(action) = watched(e.action_type) else {
        return Ok(());
    };
    if cfg.antinuke.limits.is_empty() {
        return Ok(());
    }
    // shortcut: only Owner/EO/TA are exempt, permit-level anti-nuke whitelists are not read yet.
    let owner = app.http.guild(gid).await?.model().await?.owner_id;
    let exempt = app.level(gid, owner, actor).await?.immune();
    let inv = inverse(action, e.target_id.map(|t| t.get()));
    if !exempt {
        sqlx::query("INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse) VALUES ($1,$2,$3,$4,$5)")
            .bind(gid.get() as i64)
            .bind(actor.get() as i64)
            .bind(format!("{action:?}"))
            .bind(e.target_id.map(|t| t.get() as i64))
            .bind(inv.as_ref().map(|i| json!(i)))
            .execute(&app.pool)
            .await?;
    }
    let verdict = {
        let mut an = app.antinuke.lock().unwrap();
        an.limits = cfg.antinuke.limits.clone();
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

/// Strips the member's roles (saving them) so their permissions are gone.
async fn quarantine(
    app: &App,
    gid: Id<twilight_model::id::marker::GuildMarker>,
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
    Ok(())
}

async fn quarantine_and_revert(
    app: &App,
    gid: Id<twilight_model::id::marker::GuildMarker>,
    actor: Id<UserMarker>,
    why: &str,
) -> Result<()> {
    quarantine(app, gid, actor).await?;
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64
        - 3600;
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
    sqlx::query("UPDATE action_log SET reverted = true WHERE guild_id=$1 AND actor_id=$2 AND created_at >= to_timestamp($3)")
        .bind(gid.get() as i64)
        .bind(actor.get() as i64)
        .bind(since)
        .execute(&app.pool)
        .await?;
    let case = cases::create(
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
    .await?;
    let reinvite: Vec<String> = plan.reinvite.iter().map(|u| format!("<@{u}>")).collect();
    let text = format!(
        "**Anti-nuke** case #{case}: <@{actor}> quarantined, {why}. Reverted {}/{} actions. Kicked, re-invite: {}",
        plan.ops.len() - failed,
        plan.ops.len(),
        if reinvite.is_empty() { "none".into() } else { reinvite.join(" ") }
    );
    send_log(app, gid.get(), LogKind::AntiNuke, &text).await
}

async fn run_inverse(
    app: &App,
    gid: Id<twilight_model::id::marker::GuildMarker>,
    op: &Inverse,
) -> Result<()> {
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
        // shortcut: no code records these two yet, and Reinvite is reported instead.
        Inverse::RestoreRoles { .. } | Inverse::Unquarantine { .. } | Inverse::Reinvite { .. } => {}
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
    // shortcut: quarantine-whitelisted permits are not read, only Owner/EO/TA may touch the held member.
    let owner = app.http.guild(gid).await?.model().await?.owner_id;
    let whitelisted = app.level(gid, owner, actor).await?.immune();
    if !hold_violation(actor.get(), target.get(), true, whitelisted) {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let roles: Vec<_> = cfg.antinuke.quarantine_role.into_iter().collect();
    app.http
        .update_guild_member(gid, target)
        .roles(&roles)
        .await?;
    quarantine_and_revert(app, gid, actor, "changed the roles of a quarantined member").await
}
