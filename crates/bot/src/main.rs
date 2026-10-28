mod antinuke;
mod app;
mod appeals;
mod automod;
mod backup;
mod catch;
mod commands;
mod events;
mod hierarchy;
mod lockdown;
mod metrics;
mod modcmds;
mod panic;
mod prefix;
mod reply;
mod reports;
mod scheduler;
mod settings;
mod slash;
mod targets;
mod tickets;
mod timeout_roles;
mod verify;
mod welcome;
mod whitelist;

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
    tokio::spawn({
        let app = app.clone();
        async move {
            loop {
                app.sites.refresh().await;
                tokio::time::sleep(tommybot_automod::sites::REFRESH).await;
            }
        }
    });
    tokio::spawn(scheduler::run_restore(app.clone()));
    if let Ok(addr) = std::env::var("METRICS_ADDR") {
        tokio::spawn({
            let app = app.clone();
            async move {
                if let Err(e) = metrics::serve(app, addr).await {
                    tracing::error!(?e, "metrics server stopped");
                }
            }
        });
    }
    tokio::spawn(events::flush_logs(app.clone()));
    tokio::spawn(backup::auto_backup(app.clone()));

    if let Err(e) = slash::register(&app).await {
        tracing::warn!(?e, "slash command registration failed");
    }

    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MODERATION
        | Intents::AUTO_MODERATION_EXECUTION
        | Intents::GUILD_MESSAGES
        | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, cfg.discord_token, intents);

    let stop = shutdown_signal();
    tokio::pin!(stop);
    loop {
        let event = tokio::select! {
            _ = &mut stop => break,
            e = shard.next_event(EventTypeFlags::all()) => e,
        };
        match event {
            Some(Ok(event)) => {
                tokio::spawn(events::handle(app.clone(), event));
            }
            Some(Err(e)) => {
                tracing::warn!(?e, "gateway error");
                metrics::GATEWAY_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            None => break,
        }
    }
    tracing::info!("shutting down");
    events::shutdown(&app).await;
    Ok(())
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what `docker stop` sends).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
