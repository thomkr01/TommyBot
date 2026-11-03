mod activity;
mod antinuke;
mod app;
mod appeals;
mod automod;
mod backup;
mod catch;
mod commands;
mod confirm;
mod events;
mod general;
mod hierarchy;
mod http_err;
mod info;
mod invites;
mod joingate;
mod lockdown;
mod metrics;
mod modcmds;
mod modslash;
mod modview;
mod native;
mod panic;
mod prefix;
mod previews;
mod purge;
mod qr;
mod reply;
mod reports;
mod scheduler;
mod settings;
mod setup;
mod slash;
mod targets;
mod tickets;
mod timeout_roles;
mod verify;
mod webhooks;
mod welcome;
mod whitelist;

use anyhow::Result;
use std::sync::Arc;
use tommybot_core::{config::Config, db};
use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};

/// Loads the ids of the application emojis that `tools/emojis.mjs upload` uploaded.
async fn load_emojis(http: &twilight_http::Client) -> Result<()> {
    let app = http.current_user_application().await?.model().await?.id;
    let list = http.get_application_emojis(app).await?.model().await?;
    tommybot_logging::emoji::load(list.items.into_iter().map(|e| (e.name, e.id.get())));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = Config::from_env()?;
    let pool = db::connect(&cfg.database_url).await?;
    let http = twilight_http::Client::new(cfg.discord_token.clone());
    let me = http.current_user().await?.model().await?.id;
    if let Err(e) = load_emojis(&http).await {
        tracing::warn!(?e, "custom emojis not loaded, using Unicode fallbacks");
    }
    if let Err(e) = previews::publish(&pool).await {
        tracing::warn!(?e, "saving the dashboard previews failed");
    }
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
            // Without the listener, saved config only applies after a restart, so it never gives up.
            loop {
                if let Err(e) = app.clone().listen_config().await {
                    tracing::error!(?e, "config listener stopped, retrying");
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    });

    if let Err(e) = panic::restore(&app).await {
        tracing::warn!(?e, "restoring panic mode failed");
    }
    tokio::spawn({
        let app = app.clone();
        async move {
            if let Err(e) = lockdown::resume(&app).await {
                tracing::warn!(?e, "resuming interrupted locks failed");
            }
        }
    });
    metrics::STARTED.store(app::now_secs() as u64, std::sync::atomic::Ordering::Relaxed);
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
        | Intents::MESSAGE_CONTENT
        | Intents::GUILD_VOICE_STATES
        | Intents::GUILD_MESSAGE_REACTIONS
        | Intents::DIRECT_MESSAGES;
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
                if let Some(d) = shard.latency().average() {
                    metrics::LATENCY_MS
                        .store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
                }
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
