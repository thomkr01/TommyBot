//! Misc "Remove Main Roles on TO": a timed-out member loses the main roles, and gets them back
//! when the timeout ends or is lifted.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tommybot_core::jobs;
use twilight_http::request::AuditLogReason;
use twilight_model::{
    gateway::payload::incoming::MemberUpdate,
    id::{marker::GuildMarker, Id},
};

use crate::app::{now_secs, App};

const KIND: &str = "restore_main_roles";

pub async fn on_member_update(app: &App, m: &MemberUpdate) -> Result<()> {
    let cfg = app.cfg(m.guild_id.get()).await?;
    if !cfg.misc.remove_main_roles_on_timeout {
        return Ok(());
    }
    let main = &cfg.lockdown.main_roles;
    let g = m.guild_id.get() as i64;
    let until = m
        .communication_disabled_until
        .map(|t| t.as_secs())
        .filter(|&t| t > now_secs());
    if let Some(until) = until {
        let held: Vec<_> = main.iter().filter(|r| m.roles.contains(r)).collect();
        if held.is_empty() {
            return Ok(());
        }
        for r in &held {
            app.http
                .remove_guild_member_role(m.guild_id, m.user.id, **r)
                .reason("Timed out: main roles removed")
                .await?;
        }
        let roles: Vec<u64> = held.iter().map(|r| r.get()).collect();
        let job = json!({"user": m.user.id.get(), "roles": roles});
        return jobs::enqueue(&app.pool, g, KIND, job, until).await;
    }
    if main.iter().all(|r| m.roles.contains(r)) {
        return Ok(());
    }
    // Timeout lifted early: restore now instead of at the old end.
    let early: Vec<Value> = sqlx::query_scalar(
        "DELETE FROM scheduled_jobs WHERE guild_id = $1 AND kind = $2 AND payload->>'user' = $3
         RETURNING payload",
    )
    .bind(g)
    .bind(KIND)
    .bind(m.user.id.get().to_string())
    .fetch_all(&app.pool)
    .await?;
    for p in &early {
        restore(app, m.guild_id, p).await?;
    }
    Ok(())
}

/// Gives the removed roles back, or waits again if the timeout was extended.
pub async fn restore(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(p["user"].as_u64().context("job payload has no user")?);
    let member = app.http.guild_member(gid, user).await?.model().await?;
    if let Some(t) = member
        .communication_disabled_until
        .map(|t| t.as_secs())
        .filter(|&t| t > now_secs())
    {
        return jobs::enqueue(&app.pool, gid.get() as i64, KIND, p.clone(), t).await;
    }
    for r in p["roles"].as_array().into_iter().flatten() {
        let Some(r) = r.as_u64() else { continue };
        app.http
            .add_guild_member_role(gid, user, Id::new(r))
            .reason("Timeout ended: main roles restored")
            .await?;
    }
    Ok(())
}
