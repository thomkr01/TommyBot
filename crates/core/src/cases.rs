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
    /// Attachment URL from the slash `proof:` option.
    pub proof: Option<&'a str>,
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
        "INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, reason, points, duration_s, proof, expires_at)
         VALUES ($1, (SELECT COALESCE(MAX(case_no), 0) + 1 FROM cases WHERE guild_id = $1),
                 $2, $3, $4, $5, $6, $7, $8,
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
    .bind(c.proof)
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
    record(
        &mut tx,
        ModAction {
            guild_id: c.guild_id,
            mod_id: c.mod_id,
            kind: c.kind,
            source: "bot",
            target_id: Some(c.user_id),
            case_no: Some(case_no),
            audit_id: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(case_no)
}

/// One staff action for moderator stats (`mod_actions`, append-only).
pub struct ModAction<'a> {
    pub guild_id: GuildId,
    pub mod_id: UserId,
    pub kind: &'a str,
    /// `bot` (a case), `discord` (done in the Discord app), `appeal` or `dashboard`.
    pub source: &'a str,
    pub target_id: Option<UserId>,
    pub case_no: Option<i32>,
    /// The Discord audit log entry, so a replayed gateway event counts once.
    pub audit_id: Option<i64>,
}

/// Records a staff action, inside the caller's transaction when it has one.
pub async fn record(c: &mut sqlx::PgConnection, a: ModAction<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO mod_actions (guild_id, mod_id, kind, source, target_id, case_no, audit_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
    )
    .bind(a.guild_id)
    .bind(a.mod_id)
    .bind(a.kind)
    .bind(a.source)
    .bind(a.target_id)
    .bind(a.case_no)
    .bind(a.audit_id)
    .execute(c)
    .await?;
    Ok(())
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
