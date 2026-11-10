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
    };
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8090".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("mcp listening on {bind}");
    axum::serve(listener, tommybot_mcp::router(st)).await?;
    Ok(())
}
