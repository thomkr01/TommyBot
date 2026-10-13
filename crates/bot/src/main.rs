mod antinuke;
mod app;
mod automod;
mod backup;
mod commands;
mod events;
mod hierarchy;
mod lockdown;
mod modcmds;
mod panic;
mod prefix;
mod reply;
mod scheduler;
mod slash;
mod targets;
mod verify;
mod welcome;

use anyhow::Result;
use std::sync::Arc;
use tommybot_core::{config::Config, db};
use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = Config::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    let http = twilight_http::Client::new(cfg.discord_token.clone());
    let me = http.current_user().await?.model().await?.id;
    let app = Arc::new(app::App::new(
        http,
        twilight_http::Client::new(cfg.discord_token.clone()),
        pool,
        tommybot_automod::discord_api::Api::new(&cfg.discord_token),
        me,
    ));
    tokio::spawn({
        let app = app.clone();
        async move {
            // Without the listener, saved config only applies after a restart.
            if let Err(e) = app.listen_config().await {
                tracing::error!(?e, "config listener stopped");
            }
        }
    });

    if let Err(e) = panic::restore(&app).await {
        tracing::warn!(?e, "restoring panic mode failed");
    }
    tokio::spawn(scheduler::run(app.clone()));
    tokio::spawn(backup::auto_backup(app.clone()));

    if let Err(e) = slash::register(&app).await {
        tracing::warn!(?e, "slash command registration failed");
    }

    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MODERATION
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, cfg.discord_token, intents);

    while let Some(event) = shard.next_event(EventTypeFlags::all()).await {
        match event {
            Ok(event) => {
                tokio::spawn(events::handle(app.clone(), event));
            }
            Err(e) => tracing::warn!(?e, "gateway error"),
        }
    }
    Ok(())
}
