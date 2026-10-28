//! Polls `scheduled_jobs` and runs what is due; failures are logged, never fatal.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};
use tommybot_automod::lockdown::kind_from;
use tommybot_core::jobs::{self, Job};
use tommybot_logging::{
    embed::{DONE, YES},
    Log, LogKind,
};
use twilight_model::id::{marker::UserMarker, Id};

use crate::{
    app::{now_secs, App},
    backup, commands,
    events::send_log,
    lockdown, metrics, panic, verify,
};

fn user(payload: &Value) -> Result<Id<UserMarker>> {
    Ok(Id::new(
        payload["user"]
            .as_u64()
            .context("job payload has no user")?,
    ))
}

async fn dispatch(app: &App, job: &Job) -> Result<()> {
    let gid = Id::new(job.guild_id as u64);
    match job.kind.as_str() {
        "unban" => {
            app.http.delete_ban(gid, user(&job.payload)?).await?;
        }
        "lock" => {
            let p = &job.payload;
            let channel = p["channel"]
                .as_u64()
                .or_else(|| p["channel"].as_str()?.parse().ok());
            let kind = kind_from(p["kind"].as_str().unwrap_or_default(), channel)
                .context("job payload has no valid lock kind")?;
            if let (true, Some(d)) = (
                lockdown::lock(app, gid, kind.clone()).await?,
                p["duration_s"].as_i64(),
            ) {
                jobs::replace(
                    &app.pool,
                    job.guild_id,
                    "unlock",
                    serde_json::json!({ "lock": kind }),
                    now_secs() + d,
                )
                .await?;
            }
        }
        // Timed unlocks name their lock; older jobs and the dashboard's lift everything.
        "unlock" => {
            let which = serde_json::from_value(job.payload["lock"].clone()).ok();
            lockdown::unlock(app, gid, which.as_ref()).await?;
        }
        "timeout_renew" => {
            let until = job.payload["until_secs"]
                .as_i64()
                .context("job payload has no until_secs")?;
            commands::timeout_until(app, gid, user(&job.payload)?, until, "Timeout renewal")
                .await?;
        }
        "panic_end" => panic::end(app, gid, &job.payload).await?,
        "panic_start" => {
            if !app.panic_runs.lock().unwrap().contains_key(&gid.get()) {
                let cfg = app.cfg(gid.get()).await?.panic.clone();
                panic::start(app, gid, &cfg).await?;
            }
        }
        "backup_create" => {
            let id = backup::save(app, gid).await?;
            send_log(
                app,
                gid.get(),
                LogKind::Server,
                Log::new("Backup Created", DONE).line("Id", &format!("`{id}`")),
            )
            .await?;
        }
        "backup_load" => {
            let id = job.payload["id"]
                .as_i64()
                .context("job payload has no id")?;
            let text = backup::load(app, gid, id, true).await?;
            let log = Log::new("Backup Loaded", DONE)
                .line("Id", &format!("`{id}`"))
                .text(&format!("{YES} **{text}**"));
            send_log(app, gid.get(), LogKind::Server, log).await?;
        }
        "delete_message" => crate::reply::run_delete(app, &job.payload).await?,
        "verify_deadline" => verify::deadline(app, gid, user(&job.payload)?).await?,
        "verify_grant" => verify::web_grant(app, gid, user(&job.payload)?).await?,
        "raid_end" => {
            let caught: i64 = sqlx::query_scalar(
                "select count(*) from raid_log where guild_id = $1 and raid_ms = $2",
            )
            .bind(gid.get() as i64)
            .bind(job.payload["started_ms"].as_i64().unwrap_or(0))
            .fetch_one(&app.pool)
            .await?;
            send_log(
                app,
                gid.get(),
                LogKind::JoinRaid,
                Log::new("🕵 Join Raid Report!", DONE).text(&format!(
                    "{YES} `{caught}` users have been caught by the join raid."
                )),
            )
            .await?
        }
        "restore_main_roles" => crate::timeout_roles::restore(app, gid, &job.payload).await?,
        other => tracing::warn!(kind = other, "unknown job kind"),
    }
    Ok(())
}

/// Runs one claim round for the light or heavy kinds; failures are logged, never fatal.
async fn round(app: &App, heavy: bool) {
    match jobs::claim_due(&app.pool, 50, heavy).await {
        Ok(due) => {
            for job in due {
                let done = match dispatch(app, &job).await {
                    Ok(()) => {
                        metrics::JOBS_DONE.fetch_add(1, Relaxed);
                        true
                    }
                    Err(e) => {
                        tracing::warn!(?e, id = job.id, kind = %job.kind, "job failed");
                        metrics::JOBS_FAILED.fetch_add(1, Relaxed);
                        // Otherwise the lease expires and it runs again.
                        job.attempts >= jobs::MAX_ATTEMPTS
                    }
                };
                if done {
                    if let Err(e) = jobs::finish(&app.pool, job.id).await {
                        tracing::warn!(?e, id = job.id, "finishing job failed");
                    }
                }
            }
        }
        Err(e) => tracing::warn!(?e, "claiming jobs failed"),
    }
}

pub async fn run(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        round(&app, false).await;
        if let Err(e) = jobs::expire_cases(&app.pool).await {
            tracing::warn!(?e, "case expiry failed");
        }
    }
}

/// The restore worker: backup loads and panic starts run here, on their own task and HTTP
/// client, so a long restore never holds up the timers in [`run`].
pub async fn run_restore(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        round(&app, true).await;
    }
}
