//! Polls `scheduled_jobs` and runs what is due; failures are logged, never fatal.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tommybot_automod::lockdown::kind_from;
use tommybot_core::jobs::{self, Job};
use tommybot_logging::LogKind;
use twilight_model::id::{marker::UserMarker, Id};

use crate::{
    app::{now_secs, App},
    commands,
    events::send_log,
    lockdown, panic, verify,
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
                lockdown::lock(app, gid, kind).await?,
                p["duration_s"].as_i64(),
            ) {
                jobs::replace(
                    &app.pool,
                    job.guild_id,
                    "unlock",
                    Value::Null,
                    now_secs() + d,
                )
                .await?;
            }
        }
        "unlock" => lockdown::unlock(app, gid).await?,
        "timeout_renew" => {
            let until = job.payload["until_secs"]
                .as_i64()
                .context("job payload has no until_secs")?;
            commands::timeout_until(app, gid, user(&job.payload)?, until).await?;
        }
        "panic_end" => {
            panic::end(app, gid, job.payload["unlock"].as_bool().unwrap_or(false)).await?
        }
        "delete_message" => crate::reply::run_delete(app, &job.payload).await?,
        "verify_deadline" => verify::deadline(app, gid, user(&job.payload)?).await?,
        "raid_end" => send_log(app, gid.get(), LogKind::JoinRaid, "Raid ended").await?,
        other => tracing::warn!(kind = other, "unknown job kind"),
    }
    Ok(())
}

pub async fn run(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        match jobs::claim_due(&app.pool, 50).await {
            Ok(due) => {
                for job in due {
                    let done = match dispatch(&app, &job).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(?e, id = job.id, kind = %job.kind, "job failed");
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
        if let Err(e) = jobs::expire_cases(&app.pool).await {
            tracing::warn!(?e, "case expiry failed");
        }
    }
}
