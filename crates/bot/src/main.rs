use anyhow::Result;
use tommybot_core::{config::Config, db};
use twilight_gateway::{Intents, Shard, ShardId, StreamExt as _};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = Config::from_env()?;
    let _pool = db::connect(&cfg.database_url).await?;

    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MODERATION
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, cfg.discord_token, intents);

    while let Some(event) = shard.next_event(twilight_gateway::EventTypeFlags::all()).await {
        match event {
            Ok(event) => tracing::debug!(kind = ?event.kind(), "event"),
            Err(e) => tracing::warn!(?e, "gateway error"),
        }
    }
    Ok(())
}
