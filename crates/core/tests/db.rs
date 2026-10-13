//! SQL-path tests against a real Postgres. Skipped unless DATABASE_URL is set.
use serde_json::{json, Value};
use sqlx::PgPool;
use tommybot_core::{
    cases::{self, NewCase},
    db,
    permits::{self, Level},
};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(db::connect(&url).await.expect("connect + migrate"))
}

/// Unique per test process and per test (via `offset`).
fn gid(offset: i64) -> i64 {
    7_000_000_000 + std::process::id() as i64 + offset
}

async fn cleanup(p: &PgPool, g: i64) {
    for t in [
        "cases",
        "action_log",
        "extra_owners",
        "trusted_admins",
        "permit_roles",
        "permits",
        "quarantines",
    ] {
        let sql = format!("DELETE FROM {t} WHERE guild_id = $1");
        sqlx::query(&sql).bind(g).execute(p).await.unwrap();
    }
}

fn case(
    g: i64,
    kind: &'static str,
    user: i64,
    points: i32,
    inverse: Option<Value>,
) -> NewCase<'static> {
    NewCase {
        guild_id: g,
        kind,
        user_id: user,
        mod_id: 1,
        reason: None,
        points,
        duration_s: None,
        inverse,
    }
}

#[tokio::test]
async fn case_numbers_are_per_guild() {
    let Some(p) = pool().await else { return };
    let g = gid(0);
    cleanup(&p, g).await;
    assert_eq!(
        cases::create(&p, case(g, "ban", 2, 0, None)).await.unwrap(),
        1
    );
    assert_eq!(
        cases::create(&p, case(g, "kick", 2, 0, None))
            .await
            .unwrap(),
        2
    );
    cleanup(&p, g).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_case_numbers_are_distinct() {
    let Some(p) = pool().await else { return };
    let g = gid(1);
    cleanup(&p, g).await;
    let (a, b, c, d) = tokio::join!(
        cases::create(&p, case(g, "ban", 2, 0, None)),
        cases::create(&p, case(g, "ban", 3, 0, None)),
        cases::create(&p, case(g, "ban", 4, 0, None)),
        cases::create(&p, case(g, "ban", 5, 0, None)),
    );
    let mut nums = vec![a.unwrap(), b.unwrap(), c.unwrap(), d.unwrap()];
    nums.sort();
    assert_eq!(nums, vec![1, 2, 3, 4]);
    cleanup(&p, g).await;
}

#[tokio::test]
async fn create_writes_action_log_with_inverse() {
    let Some(p) = pool().await else { return };
    let g = gid(2);
    cleanup(&p, g).await;
    let inv = json!({"op": "unban", "user": 42});
    let n = cases::create(&p, case(g, "ban", 42, 0, Some(inv.clone())))
        .await
        .unwrap();
    let row: (String, Option<i64>, Option<Value>, Option<i32>, i64, bool) = sqlx::query_as(
        "SELECT action, target_id, inverse, case_no, actor_id, reverted FROM action_log WHERE guild_id = $1",
    )
    .bind(g)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(
        row,
        ("ban".to_owned(), Some(42), Some(inv), Some(n), 1, false)
    );
    cleanup(&p, g).await;
}

#[tokio::test]
async fn warn_points_sum_and_reset() {
    let Some(p) = pool().await else { return };
    let g = gid(3);
    cleanup(&p, g).await;
    cases::create(&p, case(g, "warn", 9, 2, None))
        .await
        .unwrap();
    cases::create(&p, case(g, "warn", 9, 3, None))
        .await
        .unwrap();
    cases::create(&p, case(g, "kick", 9, 9, None))
        .await
        .unwrap(); // not a warn
    cases::create(&p, case(g, "warn", 8, 7, None))
        .await
        .unwrap(); // other user
    assert_eq!(cases::warn_points(&p, g, 9).await.unwrap(), 5);

    cases::reset_warns(&p, g, 9).await.unwrap();
    assert_eq!(cases::warn_points(&p, g, 9).await.unwrap(), 0);
    assert_eq!(cases::warn_points(&p, g, 8).await.unwrap(), 7);
    cleanup(&p, g).await;
}

#[tokio::test]
async fn expired_warns_not_counted() {
    let Some(p) = pool().await else { return };
    let g = gid(4);
    cleanup(&p, g).await;
    sqlx::query(
        "INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, points, expires_at)
         VALUES ($1, 100, 'warn', 9, 1, 4, now() - interval '1 day')",
    )
    .bind(g)
    .execute(&p)
    .await
    .unwrap();
    assert_eq!(cases::warn_points(&p, g, 9).await.unwrap(), 0);
    cases::create(&p, case(g, "warn", 9, 2, None))
        .await
        .unwrap();
    assert_eq!(cases::warn_points(&p, g, 9).await.unwrap(), 2);
    cleanup(&p, g).await;
}

async fn permit(p: &PgPool, g: i64, name: &str, rank: i32, perms: &[&str], role: i64) {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO permits (guild_id, name, rank, perms) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(g)
    .bind(name)
    .bind(rank)
    .bind(perms)
    .fetch_one(p)
    .await
    .unwrap();
    sqlx::query("INSERT INTO permit_roles (permit_id, guild_id, role_id) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(g)
        .bind(role)
        .execute(p)
        .await
        .unwrap();
}

#[tokio::test]
async fn permits_resolve_levels() {
    let Some(p) = pool().await else { return };
    let g = gid(5);
    cleanup(&p, g).await;
    let owner = 11;
    sqlx::query("INSERT INTO extra_owners (guild_id, user_id) VALUES ($1, 12)")
        .bind(g)
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO trusted_admins (guild_id, user_id) VALUES ($1, 13)")
        .bind(g)
        .execute(&p)
        .await
        .unwrap();
    permit(&p, g, "mod", 5, &["ban", "kick"], 100).await;
    permit(&p, g, "low", 1, &["kick"], 201).await;
    permit(&p, g, "high", 4, &["ban"], 202).await;

    assert_eq!(
        permits::resolve(&p, g, owner, owner, &[]).await.unwrap(),
        Level::Owner
    );
    assert_eq!(
        permits::resolve(&p, g, owner, 12, &[]).await.unwrap(),
        Level::ExtraOwner
    );
    assert_eq!(
        permits::resolve(&p, g, owner, 13, &[]).await.unwrap(),
        Level::TrustedAdmin
    );

    let Level::Custom { rank, perms, .. } = permits::resolve(&p, g, owner, 14, &[100, 999])
        .await
        .unwrap()
    else {
        panic!("expected Custom");
    };
    assert_eq!(
        (rank, perms),
        (5, vec!["ban".to_owned(), "kick".to_owned()])
    );

    let Level::Custom { rank, perms, .. } = permits::resolve(&p, g, owner, 15, &[201, 202])
        .await
        .unwrap()
    else {
        panic!("expected Custom");
    };
    assert_eq!((rank, perms), (4, vec!["ban".to_owned()]));

    assert_eq!(
        permits::resolve(&p, g, owner, 16, &[999]).await.unwrap(),
        Level::None
    );
    assert_eq!(
        permits::resolve(&p, g, owner, 16, &[]).await.unwrap(),
        Level::None
    );
    cleanup(&p, g).await;
}

/// Mirrors the SQL in crates/bot/src/antinuke.rs (`quarantine_and_revert` and `quarantine`). Keep in sync.
#[tokio::test]
async fn antinuke_revert_and_quarantine_sql() {
    let Some(p) = pool().await else { return };
    let g = gid(6);
    let actor = 21_i64;
    cleanup(&p, g).await;
    sqlx::query(
        "INSERT INTO action_log (guild_id, actor_id, action, target_id, inverse, reverted, created_at) VALUES
         ($1, $2, 'ban', 5, '{\"op\":\"unban\",\"user\":5}', false, now()),
         ($1, $2, 'ban', 6, '{\"op\":\"unban\",\"user\":6}', false, now() - interval '2 hours'),
         ($1, $2, 'ban', 7, '{\"op\":\"unban\",\"user\":7}', true, now()),
         ($1, $2, 'kick', 8, NULL, false, now())",
    )
    .bind(g)
    .bind(actor)
    .execute(&p)
    .await
    .unwrap();

    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 3600;
    let rows: Vec<(Value, i64)> = sqlx::query_as(
        "SELECT inverse, EXTRACT(EPOCH FROM created_at)::BIGINT FROM action_log
         WHERE guild_id=$1 AND actor_id=$2 AND NOT reverted AND inverse IS NOT NULL AND created_at >= to_timestamp($3)",
    )
    .bind(g)
    .bind(actor)
    .bind(since)
    .fetch_all(&p)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only the recent, unreverted, inverse-bearing row"
    );
    assert_eq!(rows[0].0, json!({"op": "unban", "user": 5}));
    assert!(
        (rows[0].1 - (since + 3600)).abs() < 60,
        "epoch close to now, got {}",
        rows[0].1
    );

    sqlx::query("UPDATE action_log SET reverted = true WHERE guild_id=$1 AND actor_id=$2 AND created_at >= to_timestamp($3)")
        .bind(g)
        .bind(actor)
        .bind(since)
        .execute(&p)
        .await
        .unwrap();
    let (reverted,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM action_log WHERE guild_id=$1 AND reverted")
            .bind(g)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(
        reverted, 3,
        "recent rows reverted, the 2h-old one untouched"
    );

    let first: Vec<i64> = vec![10, 11];
    let ins = "INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING";
    let r1 = sqlx::query(ins)
        .bind(g)
        .bind(actor)
        .bind(&first)
        .execute(&p)
        .await
        .unwrap();
    let r2 = sqlx::query(ins)
        .bind(g)
        .bind(actor)
        .bind(vec![99_i64])
        .execute(&p)
        .await
        .unwrap();
    assert_eq!((r1.rows_affected(), r2.rows_affected()), (1, 0));
    let (saved,): (Vec<i64>,) =
        sqlx::query_as("SELECT saved_roles FROM quarantines WHERE guild_id=$1 AND user_id=$2")
            .bind(g)
            .bind(actor)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(saved, first);
    cleanup(&p, g).await;
}
