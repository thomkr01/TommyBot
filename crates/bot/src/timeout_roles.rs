//! Misc "Remove Main Roles on TO": a timed-out member loses the main roles, and gets them back
//! when the timeout ends or is lifted.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tommybot_core::jobs;
use twilight_http::request::AuditLogReason;
use twilight_model::{
    gateway::payload::incoming::MemberUpdate,
    id::{marker::GuildMarker, Id},
    util::Timestamp,
};

use crate::app::{now_secs, App};

pub(crate) const KIND: &str = "restore_main_roles";

/// The end of a timeout that has not run out yet.
fn active(until: Option<Timestamp>) -> Option<i64> {
    until.map(|t| t.as_secs()).filter(|&t| t > now_secs())
}

pub async fn on_member_update(app: &App, m: &MemberUpdate) -> Result<()> {
    let cfg = app.cfg(m.guild_id.get()).await?;
    if !cfg.misc.remove_main_roles_on_timeout {
        return Ok(());
    }
    let main = &cfg.lockdown.main_roles;
    let g = m.guild_id.get() as i64;
    if let Some(until) = active(m.communication_disabled_until) {
        let held: Vec<_> = main.iter().filter(|r| m.roles.contains(r)).collect();
        if held.is_empty() {
            return Ok(());
        }
        // Queued first, so a restart halfway never loses the roles for good.
        let roles: Vec<u64> = held.iter().map(|r| r.get()).collect();
        // joined_at: a rejoin (Discord keeps the timeout) must not get them back unverified.
        let joined = m.joined_at.map(|t| t.as_micros());
        let job = json!({"user": m.user.id.get(), "roles": roles, "joined_at": joined});
        jobs::enqueue(&app.pool, g, KIND, job, until).await?;
        for r in &held {
            app.http
                .remove_guild_member_role(m.guild_id, m.user.id, **r)
                .reason("Timed out: main roles removed")
                .await?;
        }
        return Ok(());
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

/// Gives the removed roles back, or waits again if the timeout was extended. Nothing for a member
/// who left, is quarantined (the quarantine holds their roles) or held for verification, nor for a
/// job whose membership (`joined_at`, µs) is over.
pub async fn restore(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(p["user"].as_u64().context("job payload has no user")?);
    let Some(member) = crate::http_err::member(&app.http, gid, user).await? else {
        return Ok(());
    };
    let joined = member.joined_at.map(|t| t.as_micros());
    if p["joined_at"].as_i64().is_some_and(|j| Some(j) != joined)
        || crate::antinuke::quarantined(app, gid, user).await?
        || crate::sticky::pending(&app.pool, gid.get() as i64, user.get() as i64).await?
    {
        return Ok(());
    }
    if let Some(t) = active(member.communication_disabled_until) {
        return jobs::enqueue(&app.pool, gid.get() as i64, KIND, p.clone(), t)
            .await
            .map(drop);
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
