//! Cases and the action log (undo data for /revert, spec 2.5.2).
use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;

use crate::models::{GuildId, UserId};

pub struct NewCase<'a> {
    pub guild_id: GuildId,
    pub kind: &'a str,
    pub user_id: UserId,
    pub mod_id: UserId,
    pub reason: Option<&'a str>,
    pub points: i32,
    pub duration_s: Option<i64>,
    /// Inverse operation, e.g. `{"op":"unban","user":123}`; stored in action_log.
    pub inverse: Option<Value>,
}

/// Inserts the case and its action_log row atomically; returns the per-guild case number.
pub async fn create(pool: &PgPool, c: NewCase<'_>) -> Result<i32> {
    let mut tx = pool.begin().await?;
    // Serialize case numbering per guild.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(c.guild_id)
        .execute(&mut *tx)
        .await?;
    let (case_no,): (i32,) = sqlx::query_as(
        "INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, reason, points, duration_s, expires_at)
         VALUES ($1, (SELECT COALESCE(MAX(case_no), 0) + 1 FROM cases WHERE guild_id = $1),
                 $2, $3, $4, $5, $6, $7,
                 CASE WHEN $2 IN ('warn') THEN now() + interval '1 month' END)
         RETURNING case_no",
    )
    .bind(c.guild_id)
    .bind(c.kind)
    .bind(c.user_id)
    .bind(c.mod_id)
    .bind(c.reason)
    .bind(c.points)
    .bind(c.duration_s)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse, case_no)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(c.guild_id)
    .bind(c.mod_id)
    .bind(c.kind)
    .bind(c.user_id)
    .bind(c.inverse)
    .bind(case_no)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(case_no)
}

/// Active, unexpired warn points for a user.
pub async fn warn_points(pool: &PgPool, guild_id: GuildId, user_id: UserId) -> Result<i32> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(points), 0) FROM cases
         WHERE guild_id = $1 AND user_id = $2 AND kind = 'warn' AND active AND expires_at > now()",
    )
    .bind(guild_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(n as i32)
}

/// Warns reset after a threshold punishment is applied (spec 4.9).
pub async fn reset_warns(pool: &PgPool, guild_id: GuildId, user_id: UserId) -> Result<()> {
    sqlx::query(
        "UPDATE cases SET active = false WHERE guild_id = $1 AND user_id = $2 AND kind = 'warn'",
    )
    .bind(guild_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}
