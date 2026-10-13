//! Panic mode (spec 4.2): runs the antinuke crate's plans when several accounts get quarantined at once.
use anyhow::Result;
use serde_json::json;
use tommybot_antinuke::{panic_end_plan, panic_plan, PanicStep};
use tommybot_automod::lockdown::LockKind;
use tommybot_core::{jobs, sections::PanicCfg};
use tommybot_logging::LogKind;
use twilight_model::id::{marker::GuildMarker, Id};

use crate::{
    app::{App, PanicRun},
    backup,
    events::{send_log, send_log_pinging},
    lockdown,
};

pub async fn start(app: &App, gid: Id<GuildMarker>, cfg: &PanicCfg) -> Result<()> {
    app.panic_runs.lock().unwrap().insert(
        gid.get(),
        PanicRun {
            lock_mod_commands: cfg.lock_mod_commands,
        },
    );
    let ends = crate::app::now_secs() + cfg.duration_secs as i64;
    let payload =
        json!({ "lock_mod_commands": cfg.lock_mod_commands, "unlock": cfg.unlock_when_ending });
    jobs::replace(&app.pool, gid.get() as i64, "panic_end", payload, ends).await?;
    for step in panic_plan(cfg.lock_mod_commands) {
        match step {
            PanicStep::LockServer => {
                lockdown::lock(app, gid, LockKind::Server).await?;
            }
            PanicStep::PingWarnedRoles => {
                let text = "**Panic mode** started: several accounts were quarantined at once.";
                send_log_pinging(app, gid.get(), LogKind::Panic, text, &cfg.warned_roles).await?;
            }
            // Mod commands are gated by `panic_runs`, and the involved accounts were quarantined
            // by the verdicts that tripped panic mode.
            PanicStep::LockModCommands | PanicStep::QuarantineInvolved => {}
            PanicStep::LoadLatestBackup => {
                let text = match backup::restore_latest(app, gid).await {
                    Ok(Some(n)) => format!("Panic mode: restored latest backup ({n} changes)"),
                    Ok(None) => "Panic mode: no backup to restore".to_string(),
                    Err(e) => format!("Panic mode: backup restore failed: {e}"),
                };
                send_log(app, gid.get(), LogKind::Panic, &text).await?;
            }
            PanicStep::UnlockServer => {}
        }
    }
    send_log(app, gid.get(), LogKind::Panic, "Panic mode: server locked").await
}

/// Ends panic mode for a guild; runs from the `panic_end` job.
pub async fn end(app: &App, gid: Id<GuildMarker>, unlock: bool) -> Result<()> {
    app.panic_runs.lock().unwrap().remove(&gid.get());
    for step in panic_end_plan(unlock) {
        if step == PanicStep::UnlockServer {
            lockdown::unlock(app, gid).await?;
        }
    }
    send_log(app, gid.get(), LogKind::Panic, "Panic mode ended").await
}

/// Re-arms panic mode after a restart: the pending `panic_end` jobs say which guilds are still in it.
pub async fn restore(app: &App) -> Result<()> {
    for (guild, payload) in jobs::pending(&app.pool, "panic_end").await? {
        app.panic_runs.lock().unwrap().insert(
            guild as u64,
            PanicRun {
                lock_mod_commands: payload["lock_mod_commands"].as_bool().unwrap_or(false),
            },
        );
    }
    Ok(())
}
