//! Panic mode (spec 4.2): runs the antinuke crate's plans when several accounts get quarantined at once.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tommybot_antinuke::{panic_end_plan, panic_plan, PanicStep};
use tommybot_automod::lockdown::LockKind;
use tommybot_core::{jobs, sections::PanicCfg};
use tommybot_logging::LogKind;
use twilight_model::{
    channel::permission_overwrite::PermissionOverwriteType as ModelOverwriteType,
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{App, PanicRun},
    backup,
    events::{send_log, send_log_pinging},
    lockdown,
};

/// Counts `user` towards panic mode (a quarantined nuker, or with `heat` an automod heat trip)
/// and starts it when enough distinct accounts hit the window. No-op while panic mode runs.
pub async fn note(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, heat: bool) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?.panic.clone();
    let (executors, tracker) = if heat {
        (
            if cfg.heat_algorithm {
                cfg.heat_executors
            } else {
                0
            },
            &app.panic_heat,
        )
    } else {
        (cfg.executors, &app.panic)
    };
    if executors == 0 || app.panic_runs.lock().unwrap().contains_key(&gid.get()) {
        return Ok(());
    }
    let tripped = {
        let mut t = tracker.lock().unwrap();
        t.window = Duration::from_secs(cfg.window_secs);
        t.executors = executors;
        t.record(gid.get(), user.get(), Instant::now())
    };
    if tripped {
        start(app, gid, &cfg).await?;
    }
    Ok(())
}

async fn put_everyone(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    allow: Permissions,
    deny: Permissions,
) -> Result<()> {
    let o = PermissionOverwrite {
        id: Id::new(gid.get()),
        kind: PermissionOverwriteType::Role,
        allow: Some(allow),
        deny: Some(deny),
    };
    app.http.update_channel_permission(channel, &o).await?;
    Ok(())
}

/// Lets @everyone keep sending in a whitelisted category while the server is locked. Returns the
/// overwrite it replaced, for [`unexempt`].
async fn exempt(app: &App, gid: Id<GuildMarker>, category: Id<ChannelMarker>) -> Result<Value> {
    let ch = app.http.channel(category).await?.model().await?;
    let old = ch
        .permission_overwrites
        .unwrap_or_default()
        .into_iter()
        .find(|o| o.kind == ModelOverwriteType::Role && o.id.get() == gid.get());
    let (allow, deny) = old
        .as_ref()
        .map_or((Permissions::empty(), Permissions::empty()), |o| {
            (o.allow, o.deny)
        });
    put_everyone(
        app,
        gid,
        category,
        allow | Permissions::SEND_MESSAGES,
        deny - Permissions::SEND_MESSAGES,
    )
    .await?;
    Ok(json!({
        "channel": category.get(), "allow": allow.bits(), "deny": deny.bits(), "had": old.is_some()
    }))
}

async fn unexempt(app: &App, gid: Id<GuildMarker>, saved: &Value) -> Result<()> {
    let channel = Id::new(
        saved["channel"]
            .as_u64()
            .context("saved overwrite has no channel")?,
    );
    if saved["had"].as_bool().unwrap_or(false) {
        let bits = |k: &str| Permissions::from_bits_truncate(saved[k].as_u64().unwrap_or(0));
        put_everyone(app, gid, channel, bits("allow"), bits("deny")).await
    } else {
        app.http
            .delete_channel_permission(channel)
            .role(Id::new(gid.get()))
            .await?;
        Ok(())
    }
}

pub async fn start(app: &App, gid: Id<GuildMarker>, cfg: &PanicCfg) -> Result<()> {
    app.panic_runs.lock().unwrap().insert(
        gid.get(),
        PanicRun {
            lock_mod_commands: cfg.lock_mod_commands,
        },
    );
    let mut exempted = vec![];
    for &c in &cfg.whitelisted_categories {
        match exempt(app, gid, c).await {
            Ok(saved) => exempted.push(saved),
            Err(e) => tracing::warn!(?e, channel = c.get(), "panic: exempting category failed"),
        }
    }
    let ends = crate::app::now_secs() + cfg.duration_secs as i64;
    let payload = json!({
        "lock_mod_commands": cfg.lock_mod_commands, "unlock": cfg.unlock_when_ending,
        "exempt": exempted
    });
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

/// Ends panic mode for a guild; runs from the `panic_end` job, whose payload says what to put back.
pub async fn end(app: &App, gid: Id<GuildMarker>, payload: &Value) -> Result<()> {
    app.panic_runs.lock().unwrap().remove(&gid.get());
    for saved in payload["exempt"].as_array().into_iter().flatten() {
        if let Err(e) = unexempt(app, gid, saved).await {
            tracing::warn!(?e, "panic: restoring category overwrite failed");
        }
    }
    for step in panic_end_plan(payload["unlock"].as_bool().unwrap_or(false)) {
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
