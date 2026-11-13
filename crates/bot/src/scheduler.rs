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
    embed::{DONE, RED, YES},
    Log, LogKind,
};
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

use crate::{
    app::{now_secs, App},
    backup, commands,
    events::{send_log, send_log_link},
    http_err,
    joingate::RAID_REPORT,
    lockdown, metrics, panic, verify,
};

fn user(payload: &Value) -> Result<Id<UserMarker>> {
    Ok(Id::new(
        payload["user"]
            .as_u64()
            .context("job payload has no user")?,
    ))
}

/// What a ban ends: an older temp ban's unban, and a timeout's renewal (which would retry hourly).
pub(crate) const BANNED: &[&str] = &["unban", "timeout_renew"];

/// Drops the pending `kinds` jobs an earlier temp ban, long timeout or raid punishment of `user`
/// queued, other than `keep`, so they cannot lift, stretch or redo what was done since. Only
/// logged on failure: the action itself is already done.
pub(crate) async fn forget_jobs(
    app: &App,
    gid: Id<GuildMarker>,
    user: u64,
    kinds: &[&str],
    keep: Option<i64>,
) {
    if let Err(e) = jobs::cancel_user(&app.pool, gid.get() as i64, kinds, user, keep).await {
        tracing::warn!(?e, ?kinds, user, "dropping an earlier pending job failed");
    }
}

async fn dispatch(app: &App, job: &Job) -> Result<()> {
    // The dashboard's Raider List decisions name no server: the job fans out to each one.
    if job.guild_id == 0 {
        return crate::raiders::fan_out(app, job).await;
    }
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
            let r =
                commands::timeout_until(app, gid, user(&job.payload)?, until, "Timeout renewal");
            match r.await {
                // Not in the server: Discord keeps a timeout for a rejoin, so try hourly until it ends.
                Err(e) if http_err::code(&e) == Some(10007) && until > now_secs() => {
                    let p = job.payload.clone();
                    jobs::enqueue(
                        &app.pool,
                        job.guild_id,
                        "timeout_renew",
                        p,
                        now_secs() + 3600,
                    )
                    .await?;
                }
                r => r?,
            }
        }
        "panic_end" => panic::end(app, gid, &job.payload).await?,
        "panic_start" => {
            if !app.panic_runs.lock().unwrap().contains_key(&gid.get()) {
                let cfg = app.cfg(gid.get()).await?.panic.clone();
                panic::start(app, gid, &cfg).await?;
            }
        }
        "note_log" => {
            let n = serde_json::from_value(job.payload.clone()).context("bad note_log payload")?;
            crate::modview::send_note_log(app, gid.get(), &n).await?;
        }
        "lock_update" => {
            let text = job.payload["text"]
                .as_str()
                .context("job payload has no text")?;
            let by = job.payload["by"]
                .as_u64()
                .context("job payload has no by")?;
            crate::lockdown::dashboard_update(app, gid, by, text).await?;
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
        "confirm_expire" => crate::confirm::run_expire(app, &job.payload).await?,
        "raid_punish" => crate::joingate::punish_job(app, gid, &job.payload).await?,
        crate::reply::QUARANTINE_DM => crate::reply::quarantine_dm_job(app, gid, &job.payload).await?,
        crate::reply::QUARANTINE_DM_COOLDOWN => {}
        crate::joingate::MISSED_REJOINS => {
            crate::joingate::requarantine_missed(app, gid, &[], false).await?
        }
        "verify_deadline" => verify::deadline(app, gid, user(&job.payload)?).await?,
        "verify_blocked" => {
            let list = job.payload["list"].as_str().unwrap_or("VPN");
            verify::web_blocked(app, gid, user(&job.payload)?, list).await?
        }
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
            let code = job.payload["code"].as_str().unwrap_or_default();
            let url = crate::invites::raid_url(code).filter(|_| caught > 0);
            let log = crate::joingate::raid_report_log(caught, url.is_some().then_some(code));
            raid_report(app, gid, log, url).await?
        }
        "voice_raid_end" => crate::voice_raid::end(app, gid, &job.payload).await?,
        "nuke_revert" => {
            let p = &job.payload;
            let actor = Id::new(p["actor"].as_u64().context("job payload has no actor")?);
            let why = p["why"].as_str().unwrap_or_default();
            let since = p["since"].as_i64().context("job payload has no since")?;
            crate::antinuke::resume_nuke(app, gid, actor, why, since).await?
        }
        "restore_main_roles" => crate::timeout_roles::restore(app, gid, &job.payload).await?,
        crate::sticky::KIND => crate::sticky::run_job(app, gid, &job.payload).await?,
        "slowmode_off" => crate::slowmode::off(app, gid, &job.payload).await?,
        "ban_sync_share" => crate::bansync::share_job(app, gid, &job.payload).await?,
        "ban_sync_copy" => crate::bansync::copy_job(app, gid, &job.payload).await?,
        "ban_sync_lift" => crate::bansync::lift_job(app, gid, &job.payload).await?,
        "ban_sync_import" => crate::bansync::import_job(app, &job.payload).await?,
        "ban_sync_send_rest" => crate::bansync::send_rest_job(app, &job.payload).await?,
        "raider_check" => crate::raiders::check_job(app, gid, &job.payload).await?,
        "raider_scan" => crate::raiders::scan_job(app, gid).await?,
        "raider_lift" => crate::raiders::lift_job(app, gid, &job.payload).await?,
        other => tracing::warn!(kind = other, "unknown job kind"),
    }
    Ok(())
}

/// Posts a raid report, with the Full Raid Report button when there is a page to open.
pub(crate) async fn raid_report(
    app: &App,
    gid: Id<GuildMarker>,
    log: Log,
    url: Option<String>,
) -> Result<()> {
    match url {
        Some(url) => {
            send_log_link(app, gid.get(), LogKind::JoinRaid, log, (RAID_REPORT, url)).await
        }
        None => send_log(app, gid.get(), LogKind::JoinRaid, log).await,
    }
}

const ONE_TARGET: [&str; 9] = [
    "unban",
    "raid_punish",
    "timeout_renew",
    "delete_message",
    "slowmode_off",
    "ban_sync_copy",
    "ban_sync_lift",
    "raider_check",
    "raider_lift",
];

/// Runs one claim round for the light or heavy kinds; failures are logged, never fatal.
async fn round(app: &App, heavy: bool) {
    match jobs::claim_due(&app.pool, 50, heavy).await {
        Ok(due) => {
            for job in due {
                // A guard whose run is still alive here: that run deletes the job when it ends,
                // and the lease brings it back only if this process dies first.
                if job.kind == "nuke_revert"
                    && job.payload["actor"]
                        .as_u64()
                        .is_some_and(|a| crate::antinuke::nuke_running(app, job.guild_id as u64, a))
                {
                    continue;
                }
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
                            if job.kind == "ban_sync_copy" {
                                crate::bansync::copy_gave_up(app, &job, &e).await;
                            }
                            if job.kind == "raider_lift" {
                                crate::raiders::lift_gave_up(app, &job, &e).await;
                            }
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

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
