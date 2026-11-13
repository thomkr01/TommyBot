//! Postgres persistence (tables in crates/core/migrations/0200_automod.sql).
//! Timers (raid/panic end, lock expiry) go through the shared `scheduled_jobs` table.
use crate::{
    lockdown::{LockKind, Snapshot},
    strike::StrikeState,
};
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
        "INSERT INTO lock_snapshots (guild_id, kind, snapshot, state) VALUES ($1,$2,$3,'locking')
         ON CONFLICT (guild_id, kind) DO UPDATE SET snapshot=$3, created_at=now(), state='locking'",
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

/// When the active lock of `kind` was saved (unix secs), to tell one lock from a later lock of the
/// same kind. `None` when there is none.
pub async fn lock_created(db: &PgPool, guild: u64, kind: &LockKind) -> Result<Option<f64>> {
    Ok(sqlx::query_scalar(
        "SELECT extract(epoch from created_at)::float8 FROM lock_snapshots
         WHERE guild_id=$1 AND kind=$2 AND state<>'unlocking'",
    )
    .bind(guild as i64)
    .bind(format!("{:?}", Some(kind)))
    .fetch_optional(db)
    .await?)
}

/// Rewrites an active lock's snapshot and nothing else (not its state or age).
pub async fn update_lock(db: &PgPool, guild: u64, snap: &Snapshot) -> Result<()> {
    sqlx::query("UPDATE lock_snapshots SET snapshot=$3 WHERE guild_id=$1 AND kind=$2")
        .bind(guild as i64)
        .bind(lock_key(snap))
        .bind(sqlx::types::Json(snap))
        .execute(db)
        .await?;
    Ok(())
}

/// Every active lock of the guild, oldest first.
pub async fn locks(db: &PgPool, guild: u64) -> Result<Vec<Snapshot>> {
    let rows: Vec<(sqlx::types::Json<Snapshot>,)> =
        sqlx::query_as("SELECT snapshot FROM lock_snapshots WHERE guild_id=$1 AND state<>'unlocking' ORDER BY created_at")
            .bind(guild as i64)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s.0).collect())
}

/// Marks a saved lock as fully applied.
pub async fn lock_done(db: &PgPool, guild: u64, snap: &Snapshot) -> Result<()> {
    set_state(db, guild, snap, "locked").await.map(drop)
}

/// Claims one active lock for unlocking; false when another unlock got it first. The row stays
/// until [`drop_lock`], so an unlock cut off by a restart can be finished.
pub async fn take_lock(db: &PgPool, guild: u64, snap: &Snapshot) -> Result<bool> {
    set_state(db, guild, snap, "unlocking").await
}

async fn set_state(db: &PgPool, guild: u64, snap: &Snapshot, state: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE lock_snapshots SET state=$3 WHERE guild_id=$1 AND kind=$2 AND state<>'unlocking'",
    )
    .bind(guild as i64)
    .bind(lock_key(snap))
    .bind(state)
    .execute(db)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Deletes a lock whose unlock has run, unless it was locked again meanwhile.
pub async fn drop_lock(db: &PgPool, guild: u64, snap: &Snapshot) -> Result<()> {
    sqlx::query("DELETE FROM lock_snapshots WHERE guild_id=$1 AND kind=$2 AND state='unlocking'")
        .bind(guild as i64)
        .bind(lock_key(snap))
        .execute(db)
        .await?;
    Ok(())
}

/// Locks and unlocks a restart cut off, as (guild, snapshot, unlocking). Unlocking rows are handed
/// back as active locks, so the caller's unlock can claim them again.
pub async fn interrupted(db: &PgPool) -> Result<Vec<(u64, Snapshot, bool)>> {
    let rows: Vec<(i64, sqlx::types::Json<Snapshot>, bool)> = sqlx::query_as(
        "UPDATE lock_snapshots SET state='locked' WHERE state='unlocking'
         RETURNING guild_id, snapshot, true",
    )
    .fetch_all(db)
    .await?;
    let locking: Vec<(i64, sqlx::types::Json<Snapshot>, bool)> = sqlx::query_as(
        "SELECT guild_id, snapshot, false FROM lock_snapshots WHERE state='locking'",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .chain(locking)
        .map(|(g, s, u)| (g as u64, s.0, u))
        .collect())
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
