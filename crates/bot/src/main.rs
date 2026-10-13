mod commands;
mod prefix;

use anyhow::Result;
use tommybot_core::{config::Config, db};
use twilight_gateway::{Event, Intents, Shard, ShardId, StreamExt as _};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = Config::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;

    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MODERATION
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT;
    let http = twilight_http::Client::new(cfg.discord_token.clone());
    let mut shard = Shard::new(ShardId::ONE, cfg.discord_token, intents);

    while let Some(event) = shard.next_event(twilight_gateway::EventTypeFlags::all()).await {
        match event {
            Ok(Event::MessageCreate(msg)) if !msg.author.bot => {
                let Some(text) = msg.content.strip_prefix(commands::PREFIX) else { continue };
                if let Some(parsed) = prefix::parse(text) {
                    let ctx = commands::Ctx { http: &http, pool: &pool };
                    if let Err(e) = commands::run(&ctx, &msg, parsed).await {
                        tracing::warn!(?e, "command failed");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(?e, "gateway error"),
        }
    }
    Ok(())
}
