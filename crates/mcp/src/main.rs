use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _log = tommybot_core::log::init(
        "mcp",
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    );
    let token = std::env::var("MCP_TOKEN").context("MCP_TOKEN not set")?;
    anyhow::ensure!(
        token.len() >= 32,
        "MCP_TOKEN must be at least 32 characters"
    );
    let url = std::env::var("MCP_DATABASE_URL").context("MCP_DATABASE_URL not set")?;
    // Not core::db::connect: this role is read-only and must not run migrations.
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect_lazy(&url)?;
    let st = tommybot_mcp::State {
        db,
        token,
        log_dir: std::env::var("LOG_READ_DIR").unwrap_or_else(|_| "/logs".into()),
        oauth: oauth_from_env()?,
        writes: writes_from_env()?,
    };
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("mcp listening on {bind}");
    axum::serve(listener, tommybot_mcp::router(st)).await?;
    Ok(())
}

/// OAuth for claude.ai, on only when every setting is there.
fn oauth_from_env() -> anyhow::Result<Option<tommybot_mcp::oauth::OAuth>> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let (Some(secret), Some(base), Some(owner_id), Some(client_id), Some(client_secret)) = (
        var("MCP_OAUTH_SECRET"),
        var("MCP_PUBLIC_URL"),
        var("MCP_OWNER_ID"),
        var("DISCORD_CLIENT_ID"),
        var("DISCORD_CLIENT_SECRET"),
    ) else {
        tracing::info!(
            "oauth off (MCP_OAUTH_SECRET, MCP_PUBLIC_URL, MCP_OWNER_ID or Discord client not set)"
        );
        return Ok(None);
    };
    anyhow::ensure!(
        secret.len() >= 32,
        "MCP_OAUTH_SECRET must be at least 32 characters"
    );
    anyhow::ensure!(
        owner_id.parse::<u64>().is_ok(),
        "MCP_OWNER_ID must be a Discord user id"
    );
    Ok(Some(tommybot_mcp::oauth::OAuth {
        secret,
        base: base.trim_end_matches('/').to_string(),
        owner_id,
        client_id,
        client_secret,
        discord_api: var("DISCORD_API_BASE")
            .unwrap_or_else(|| "https://discord.com/api/v10".into()),
        http: reqwest::Client::new(),
    }))
}

/// Writes through the dashboard, on only with `MCP_WRITE_TOKEN` (the same value the dashboard has).
fn writes_from_env() -> anyhow::Result<Option<tommybot_mcp::writes::Writes>> {
    let Some(token) = std::env::var("MCP_WRITE_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        tracing::info!("writes off (MCP_WRITE_TOKEN not set)");
        return Ok(None);
    };
    anyhow::ensure!(
        token.len() >= 32,
        "MCP_WRITE_TOKEN must be at least 32 characters"
    );
    Ok(Some(tommybot_mcp::writes::Writes {
        base: std::env::var("DASHBOARD_INTERNAL_URL")
            .unwrap_or_else(|_| "http://dashboard:8080".into()),
        token,
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?,
    }))
}
