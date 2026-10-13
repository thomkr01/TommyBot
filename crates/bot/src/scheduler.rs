//! Polls `scheduled_jobs` and runs what is due; failures are logged, never fatal.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tommybot_core::jobs::{self, Job};
use twilight_model::id::{marker::UserMarker, Id};

use crate::{app::App, commands, lockdown};

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
        "unlock" => lockdown::unlock(app, gid).await?,
        "timeout_renew" => {
            let until = job.payload["until_secs"]
                .as_i64()
                .context("job payload has no until_secs")?;
            commands::timeout_until(app, gid, user(&job.payload)?, until).await?;
        }
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
                    if let Err(e) = dispatch(&app, &job).await {
                        tracing::warn!(?e, id = job.id, kind = %job.kind, "job failed");
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
