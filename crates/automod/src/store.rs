//! Postgres persistence (tables in crates/core/migrations/0200_automod.sql).
//! Timers (raid/panic end, lock expiry) go through the shared `scheduled_jobs` table.
use crate::{lockdown::Snapshot, strike::StrikeState};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

type Result<T> = std::result::Result<T, sqlx::Error>;

pub async fn load_strikes(db: &PgPool, guild: u64, user: u64) -> Result<StrikeState> {
    let row = sqlx::query(
        "SELECT strikes, last_strike_ms, last_cap_duration_ms, cap_end_ms FROM automod_strikes WHERE guild_id=$1 AND user_id=$2",
    )
    .bind(guild as i64)
    .bind(user as i64)
    .fetch_optional(db)
    .await?;
    Ok(row.map_or_else(StrikeState::default, |r| StrikeState {
        strikes: r.get::<i32, _>(0) as u32,
        last_strike_ms: r.get(1),
        last_cap_duration_ms: r.get(2),
        cap_end_ms: r.get(3),
    }))
}

pub async fn save_strikes(db: &PgPool, guild: u64, user: u64, s: &StrikeState) -> Result<()> {
    sqlx::query(
        "INSERT INTO automod_strikes (guild_id, user_id, strikes, last_strike_ms, last_cap_duration_ms, cap_end_ms)
         VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET strikes=$3, last_strike_ms=$4, last_cap_duration_ms=$5, cap_end_ms=$6",
    )
    .bind(guild as i64)
    .bind(user as i64)
    .bind(s.strikes as i32)
    .bind(s.last_strike_ms)
    .bind(s.last_cap_duration_ms)
    .bind(s.cap_end_ms)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn save_lock(db: &PgPool, guild: u64, snap: &Snapshot) -> Result<()> {
    sqlx::query(
        "INSERT INTO lock_snapshots (guild_id, kind, snapshot) VALUES ($1,$2,$3)
         ON CONFLICT (guild_id, kind) DO UPDATE SET snapshot=$3, created_at=now()",
    )
    .bind(guild as i64)
    .bind(lock_key(snap))
    .bind(sqlx::types::Json(snap))
    .execute(db)
    .await?;
    Ok(())
}

fn lock_key(snap: &Snapshot) -> String {
    format!("{:?}", snap.kind)
}

/// Every active lock of the guild, oldest first.
pub async fn locks(db: &PgPool, guild: u64) -> Result<Vec<Snapshot>> {
    let rows: Vec<(sqlx::types::Json<Snapshot>,)> =
        sqlx::query_as("SELECT snapshot FROM lock_snapshots WHERE guild_id=$1 ORDER BY created_at")
            .bind(guild as i64)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s.0).collect())
}

/// Removes one active lock; false when another unlock got it first.
pub async fn take_lock(db: &PgPool, guild: u64, snap: &Snapshot) -> Result<bool> {
    let done = sqlx::query("DELETE FROM lock_snapshots WHERE guild_id=$1 AND kind=$2")
        .bind(guild as i64)
        .bind(lock_key(snap))
        .execute(db)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Queue a restart-safe timer, e.g. kind `lock_expiry`, `raid_end`, `panic_end`.
pub async fn schedule(
    db: &PgPool,
    guild: u64,
    kind: &str,
    payload: serde_json::Value,
    run_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO scheduled_jobs (guild_id, kind, payload, run_at) VALUES ($1,$2,$3,$4)",
    )
    .bind(guild as i64)
    .bind(kind)
    .bind(payload)
    .bind(run_at)
    .execute(db)
    .await?;
    Ok(())
}
