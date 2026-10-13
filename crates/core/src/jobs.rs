//! Restart-safe timers on `scheduled_jobs`: one row per pending action, claimed by deleting it.
use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub guild_id: i64,
    pub kind: String,
    pub payload: Value,
}

pub async fn enqueue(
    pool: &PgPool,
    guild: i64,
    kind: &str,
    payload: Value,
    run_at_secs: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO scheduled_jobs (guild_id, kind, payload, run_at)
         VALUES ($1, $2, $3, to_timestamp($4::float8))",
    )
    .bind(guild)
    .bind(kind)
    .bind(payload)
    .bind(run_at_secs as f64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes and returns due jobs; SKIP LOCKED keeps two instances from claiming the same row.
// shortcut: a crash after the claim loses that job, move to a lease column if jobs must never be lost.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<Job>> {
    Ok(sqlx::query_as(
        "WITH c AS (
             DELETE FROM scheduled_jobs WHERE id IN (
                 SELECT id FROM scheduled_jobs WHERE run_at <= now()
                 ORDER BY run_at FOR UPDATE SKIP LOCKED LIMIT $1)
             RETURNING id, guild_id, kind, payload, run_at)
         SELECT id, guild_id, kind, payload FROM c ORDER BY run_at",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Deactivates cases whose expiry passed; returns how many.
pub async fn expire_cases(pool: &PgPool) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE cases SET active = false
         WHERE active AND expires_at IS NOT NULL AND expires_at < now()",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// Skips (None) without DATABASE_URL. Other tests share the tables, so each uses its own guild.
    async fn setup(offset: i64) -> Option<(PgPool, i64)> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.unwrap();
        let gid = 5_000_000_000 + std::process::id() as i64 * 10 + offset;
        clean(&pool, gid).await;
        Some((pool, gid))
    }

    async fn clean(pool: &PgPool, gid: i64) {
        for t in ["scheduled_jobs", "cases"] {
            sqlx::query(&format!("DELETE FROM {t} WHERE guild_id = $1"))
                .bind(gid)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn claims_in_order_and_never_twice() {
        let Some((pool, gid)) = setup(1).await else {
            return;
        };
        let n = now();
        enqueue(&pool, gid, "b", Value::Null, n - 10).await.unwrap();
        enqueue(&pool, gid, "a", Value::Null, n - 20).await.unwrap();
        enqueue(&pool, gid, "later", Value::Null, n + 3600)
            .await
            .unwrap();
        let got = claim_due(&pool, 1000).await.unwrap();
        let kinds: Vec<_> = got
            .iter()
            .filter(|j| j.guild_id == gid)
            .map(|j| j.kind.as_str())
            .collect();
        assert_eq!(kinds, ["a", "b"]);
        let (left,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM scheduled_jobs WHERE guild_id=$1")
                .bind(gid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 1);
        clean(&pool, gid).await;

        // Same test on purpose: claim_due takes every guild's rows, so parallel tests would steal from each other.
        for i in 0..20 {
            enqueue(&pool, gid, "k", Value::from(i), now() - 5)
                .await
                .unwrap();
        }
        let (a, b) = tokio::join!(claim_due(&pool, 10), claim_due(&pool, 10));
        let mut ids: Vec<i64> = [a.unwrap(), b.unwrap()]
            .into_iter()
            .flatten()
            .filter(|j| j.guild_id == gid)
            .map(|j| j.id)
            .collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total);
        clean(&pool, gid).await;
    }

    #[tokio::test]
    async fn expiry_flips_only_expired_cases() {
        let Some((pool, gid)) = setup(3).await else {
            return;
        };
        for (no, expiry) in [
            (1, "now() - interval '1 hour'"),
            (2, "now() + interval '1 hour'"),
            (3, "NULL"),
        ] {
            sqlx::query(&format!(
                "INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, expires_at)
                 VALUES ($1, $2, 'warn', 1, 1, {expiry})"
            ))
            .bind(gid)
            .bind(no)
            .execute(&pool)
            .await
            .unwrap();
        }
        expire_cases(&pool).await.unwrap();
        let rows: Vec<(i32, bool)> =
            sqlx::query_as("SELECT case_no, active FROM cases WHERE guild_id=$1 ORDER BY case_no")
                .bind(gid)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, [(1, false), (2, true), (3, true)]);
        clean(&pool, gid).await;
    }
}
