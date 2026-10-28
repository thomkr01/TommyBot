//! Automod persistence against a real Postgres. Skipped unless DATABASE_URL is set.
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use tommybot_automod::{
    lockdown::{LockKind, Snapshot},
    store,
    strike::StrikeState,
};
use tommybot_core::db;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(db::connect(&url).await.expect("connect + migrate"))
}

fn gid(offset: i64) -> i64 {
    7_000_000_000 + std::process::id() as i64 + offset
}

async fn cleanup(p: &PgPool, g: i64) {
    for t in ["automod_strikes", "lock_snapshots", "scheduled_jobs"] {
        let sql = format!("DELETE FROM {t} WHERE guild_id = $1");
        sqlx::query(&sql).bind(g).execute(p).await.unwrap();
    }
}

#[tokio::test]
async fn strikes_round_trip() {
    let Some(p) = pool().await else { return };
    let g = gid(0);
    cleanup(&p, g).await;
    let s = StrikeState {
        strikes: 3,
        last_strike_ms: 1_700_000_000_000,
        last_cap_duration_ms: 60_000,
        cap_end_ms: 1_700_000_060_000,
    };
    store::save_strikes(&p, g as u64, 1, &s).await.unwrap();
    assert_eq!(store::load_strikes(&p, g as u64, 1).await.unwrap(), s);

    // Second save upserts the same row.
    let s2 = StrikeState { strikes: 4, ..s };
    store::save_strikes(&p, g as u64, 1, &s2).await.unwrap();
    assert_eq!(store::load_strikes(&p, g as u64, 1).await.unwrap(), s2);

    // Unknown user is the default state.
    assert_eq!(
        store::load_strikes(&p, g as u64, 2).await.unwrap(),
        StrikeState::default()
    );
    cleanup(&p, g).await;
}

#[tokio::test]
async fn lock_snapshot_round_trip_then_gone() {
    let Some(p) = pool().await else { return };
    let g = gid(1);
    cleanup(&p, g).await;
    let snap = Snapshot {
        kind: Some(LockKind::Channel(55)),
        role_perms: vec![(1, 2)],
        channel_denies: vec![(55, 3)],
        join_lock: true,
        invites_paused: false,
        ..Default::default()
    };
    store::save_lock(&p, g as u64, &snap).await.unwrap();
    assert_eq!(
        store::locks(&p, g as u64).await.unwrap(),
        vec![snap.clone()]
    );
    assert!(store::take_lock(&p, g as u64, &snap).await.unwrap());
    assert!(!store::take_lock(&p, g as u64, &snap).await.unwrap());
    assert!(store::locks(&p, g as u64).await.unwrap().is_empty());
    cleanup(&p, g).await;
}

#[tokio::test]
async fn schedule_inserts_job() {
    let Some(p) = pool().await else { return };
    let g = gid(2);
    cleanup(&p, g).await;
    store::schedule(
        &p,
        g as u64,
        "lock_expiry",
        json!({"guild": g}),
        Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let (kind, payload): (String, serde_json::Value) =
        sqlx::query_as("SELECT kind, payload FROM scheduled_jobs WHERE guild_id = $1")
            .bind(g)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(kind, "lock_expiry");
    assert_eq!(payload, json!({"guild": g}));
    cleanup(&p, g).await;
}
