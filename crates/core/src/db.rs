use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, PgPool};

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;
    // An older binary after a rollback must start on a newer schema (D48): migrations stay additive.
    sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(&pool)
        .await?;
    if let Some(pw) = std::env::var("MCP_DB_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
    {
        set_mcp_password(&pool, &pw).await?;
    }
    Ok(pool)
}

/// Lets the MCP server's read-only role (migration 2700) log in. A migration cannot read .env.
pub async fn set_mcp_password(pool: &PgPool, pw: &str) -> Result<()> {
    let sql: String =
        sqlx::query_scalar("SELECT format('ALTER ROLE tommybot_mcp LOGIN PASSWORD %L', $1::text)")
            .bind(pw)
            .fetch_one(pool)
            .await?;
    sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await?;
    Ok(())
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
