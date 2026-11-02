//! Polls `scheduled_jobs` and runs what is due; failures are logged, never fatal.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};
use tommybot_automod::lockdown::kind_from;
use tommybot_core::jobs::{self, Job};
use tommybot_logging::emoji::ARROW;
use tommybot_logging::{
    embed::{DONE, RED, YES},
    Log, LogKind,
};
use twilight_model::id::{marker::UserMarker, Id};

use crate::{
    app::App,
    backup, commands,
    events::{send_log, send_log_link},
    http_err, lockdown, metrics, panic, verify,
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
            lockdown::lock_for(app, gid, kind, p["duration_s"].as_i64()).await?;
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
            let id = backup::save(app, gid, job.payload["by"].as_u64()).await?;
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
            let text = backup::load(app, gid, &id.to_string(), true).await?;
            let log = Log::new("Backup Loaded", DONE)
                .line("Id", &format!("`{id}`"))
                .text(&format!("{YES} **{text}**"));
            send_log(app, gid.get(), LogKind::Server, log).await?;
        }
        "delete_message" => crate::reply::run_delete(app, &job.payload).await?,
        "verify_deadline" => verify::deadline(app, gid, user(&job.payload)?).await?,
        "verify_grant" => {
            let same_ip: Vec<u64> = job.payload["same_ip"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str()?.parse().ok())
                .collect();
            verify::web_grant(app, gid, user(&job.payload)?, &same_ip).await?
        }
        "raid_end" => {
            let caught: i64 = sqlx::query_scalar(
                "select count(*) from raid_log where guild_id = $1 and raid_ms = $2",
            )
            .bind(gid.get() as i64)
            .bind(job.payload["started_ms"].as_i64().unwrap_or(0))
            .fetch_one(&app.pool)
            .await?;
            let mut log = Log::new("🕵 Join Raid Report!", DONE).text(&format!(
                "{YES} `{caught}` users have been caught by the join raid."
            ));
            let code = job.payload["code"].as_str().unwrap_or_default();
            match crate::invites::raid_url(code).filter(|_| caught > 0) {
                Some(url) => {
                    log = log.text(&format!("{ARROW} `{code}`"));
                    send_log_link(
                        app,
                        gid.get(),
                        LogKind::JoinRaid,
                        log,
                        ("Full Raid Report ↗", url),
                    )
                    .await?
                }
                None => send_log(app, gid.get(), LogKind::JoinRaid, log).await?,
            }
        }
        "nuke_revert" => {
            let p = &job.payload;
            let actor = Id::new(p["actor"].as_u64().context("job payload has no actor")?);
            let why = p["why"].as_str().unwrap_or_default();
            let since = p["since"].as_i64().context("job payload has no since")?;
            crate::antinuke::revert_and_report(app, gid, actor, why, since).await?
        }
        "restore_main_roles" => crate::timeout_roles::restore(app, gid, &job.payload).await?,
        other => tracing::warn!(kind = other, "unknown job kind"),
    }
    Ok(())
}

const ONE_TARGET: [&str; 3] = ["unban", "timeout_renew", "delete_message"];

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
                    // One-target jobs whose member left, ban was lifted by hand or message is gone:
                    // nothing left to do, and retrying would only fail again.
                    Err(e) if ONE_TARGET.contains(&job.kind.as_str()) && http_err::gone_any(&e) => {
                        tracing::info!(id = job.id, kind = %job.kind, "job target is gone");
                        true
                    }
                    Err(e) => {
                        tracing::warn!(?e, id = job.id, kind = %job.kind, "job failed");
                        metrics::JOBS_FAILED.fetch_add(1, Relaxed);
                        // Otherwise the lease expires and it runs again.
                        let last = job.attempts >= jobs::MAX_ATTEMPTS;
                        if last {
                            give_up_log(app, &job, &e).await;
                        }
                        last
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

/// A dropped job (a temp ban that never ends, a lock that stays) must be visible on Discord, not
/// only in the container log.
async fn give_up_log(app: &App, job: &Job, e: &anyhow::Error) {
    let log = Log::new("Scheduled Action Failed", RED)
        .line("Action", &format!("`{}`", job.kind))
        .line("Error", &crate::http_err::why(e))
        .text(&format!(
            "Gave up after {} tries, do it by hand.",
            job.attempts
        ));
    if let Err(e) = send_log(app, job.guild_id as u64, LogKind::Server, log).await {
        tracing::warn!(?e, id = job.id, "could not log the failed job");
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
