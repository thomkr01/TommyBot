//! Panic mode (spec 4.2): runs the antinuke crate's plans when several accounts get quarantined at once.
use anyhow::Result;
use std::time::{Duration, Instant};
use tommybot_antinuke::{panic_end_plan, panic_plan, PanicStep};
use tommybot_automod::lockdown::LockKind;
use tommybot_core::sections::PanicCfg;
use tommybot_logging::LogKind;
use twilight_model::id::{marker::GuildMarker, Id};

use crate::{
    app::{App, PanicRun},
    events::{send_log, send_log_pinging},
    lockdown,
};

pub async fn start(app: &App, gid: Id<GuildMarker>, cfg: &PanicCfg) -> Result<()> {
    app.panic_runs.lock().unwrap().insert(
        gid.get(),
        PanicRun {
            ends: Instant::now() + Duration::from_secs(cfg.duration_secs),
            lock_mod_commands: cfg.lock_mod_commands,
            unlock_when_ending: cfg.unlock_when_ending,
        },
    );
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
            // shortcut: backup restore is not built yet, panic mode stops at the lock.
            PanicStep::LoadLatestBackup | PanicStep::UnlockServer => {}
        }
    }
    send_log(app, gid.get(), LogKind::Panic, "Panic mode: server locked").await
}

/// Ends panic runs whose time is up. Called from the event loop, so an idle guild ends late.
pub async fn end_due(app: &App) -> Result<()> {
    let due: Vec<(u64, bool)> = {
        let mut runs = app.panic_runs.lock().unwrap();
        let now = Instant::now();
        let ids: Vec<u64> = runs.iter().filter(|(_, r)| r.ends <= now).map(|(g, _)| *g).collect();
        ids.into_iter().filter_map(|g| runs.remove(&g).map(|r| (g, r.unlock_when_ending))).collect()
    };
    for (g, unlock) in due {
        let gid = Id::new(g);
        for step in panic_end_plan(unlock) {
            if step == PanicStep::UnlockServer {
                lockdown::unlock(app, gid).await?;
            }
        }
        send_log(app, g, LogKind::Panic, "Panic mode ended").await?;
    }
    Ok(())
}
