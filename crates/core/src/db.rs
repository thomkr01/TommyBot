use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, PgPool};

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Deletes every row stored for `guild` (migration 1501); the bot then falls back to defaults.
pub async fn purge_guild(pool: &PgPool, guild: i64) -> Result<()> {
    sqlx::query("SELECT purge_guild($1)")
        .bind(guild)
        .execute(pool)
        .await?;
    Ok(())
}

/// Purges every guild the bot left more than `days` ago and returns their ids.
pub async fn purge_left(pool: &PgPool, days: i32) -> Result<Vec<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT guild_id FROM guild_left WHERE left_at < now() - make_interval(days => $1)",
    )
    .bind(days)
    .fetch_all(pool)
    .await?;
    for &g in &ids {
        purge_guild(pool, g).await?;
    }
    Ok(ids)
}
