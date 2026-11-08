//! Self-host updates: the bot owner starts one here and the updater container (`scripts/updater.sh`)
//! carries it out, writing its progress to `update_runs`. The dashboard never touches Docker itself.
use crate::{api::Res, auth::Sess, ApiError, AppState};
use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const LATEST_URL: &str = "https://api.github.com/repos/thomkr01/TommyBot/actions/workflows/images.yml/runs?branch=main&status=success&per_page=1";
/// Seconds per step until an update has finished once: check, download, restart bot, restart dashboard, health check.
const DEFAULT_STEPS: [i32; 5] = [2, 60, 10, 10, 20];

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/update", get(status).post(start))
}

pub async fn is_bot_owner(st: &AppState, s: &Sess) -> bool {
    st.discord
        .bot_owner()
        .await
        .is_ok_and(|o| o == s.user_id.to_string())
}

async fn owner(st: &AppState, s: &Sess) -> Result<(), ApiError> {
    if is_bot_owner(st, s).await {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "only the bot owner can update TommyBot",
        ))
    }
}

/// The newest commit whose images were published, cached for a minute (GitHub allows 60 calls an hour without a token).
async fn latest(st: &AppState) -> Option<Value> {
    static CACHE: Mutex<Option<(Instant, Option<Value>)>> = Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap().clone() {
        if at.elapsed() < Duration::from_secs(60) {
            return v;
        }
    }
    let r: Option<Value> = async {
        let r = st
            .discord
            .http
            .get(LATEST_URL)
            .header("User-Agent", "TommyBot-dashboard");
        r.send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()
    }
    .await;
    let v = r.and_then(|r| {
        let run = &r["workflow_runs"][0];
        run["head_sha"].is_string().then(
            || json!({"sha": run["head_sha"], "title": run["display_title"], "at": run["created_at"]}),
        )
    });
    *CACHE.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

/// Seconds each step is expected to take: the average of the last finished runs, else a default.
fn estimate(done: &[Vec<i32>]) -> Vec<i32> {
    DEFAULT_STEPS
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let xs: Vec<i32> = done.iter().filter_map(|t| t.get(i).copied()).collect();
            if xs.is_empty() {
                *d
            } else {
                (xs.iter().sum::<i32>() / xs.len() as i32).max(1)
            }
        })
        .collect()
}

async fn updater_online(st: &AppState) -> Result<bool, ApiError> {
    Ok(
        sqlx::query_scalar("select at > now() - interval '90 seconds' from updater_seen")
            .fetch_optional(&st.db)
            .await?
            .unwrap_or(false),
    )
}

async fn status(State(st): State<AppState>, s: Sess) -> Res {
    owner(&st, &s).await?;
    let run: Option<Value> = sqlx::query_scalar(
        "select jsonb_build_object('status', status, 'step', step, 'step_times', step_times,
            'step_elapsed', extract(epoch from now() - step_started_at)::int, 'to_sha', to_sha, 'error', error, 'took', extract(epoch from finished_at - created_at)::int)
         from update_runs order by id desc limit 1",
    )
    .fetch_optional(&st.db)
    .await?;
    let done: Vec<Vec<i32>> = sqlx::query_scalar(
        "select step_times from update_runs where status = 'done' order by id desc limit 5",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!({
        "current": std::env::var("TOMMYBOT_SHA").ok().filter(|v| v.len() == 40),
        "latest": latest(&st).await,
        "updater": updater_online(&st).await?,
        "run": run,
        "estimate": estimate(&done),
    })))
}

async fn start(State(st): State<AppState>, s: Sess) -> Res {
    owner(&st, &s).await?;
    if !s.stepped_up {
        return Err(ApiError::StepUp);
    }
    if !updater_online(&st).await? {
        return Err(ApiError::BadRequest(
            "The updater is not running. Start it on the server: docker compose up -d updater"
                .into(),
        ));
    }
    let id: Option<i64> = sqlx::query_scalar(
        "insert into update_runs (requested_by) values ($1) on conflict do nothing returning id",
    )
    .bind(s.user_id)
    .fetch_optional(&st.db)
    .await?;
    match id {
        Some(_) => Ok(Json(json!({"ok": true}))),
        None => Err(ApiError::BadRequest("An update is already running.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_averages_finished_runs_per_step() {
        assert_eq!(estimate(&[]), DEFAULT_STEPS.to_vec());
        let runs = [vec![1, 40, 8, 6, 17], vec![3, 80, 12, 6, 17]];
        assert_eq!(estimate(&runs), vec![2, 60, 10, 6, 17]);
        // A step that took under a second still counts as one, so progress never divides by zero.
        assert_eq!(estimate(&[vec![0, 0, 0, 0, 0]]), vec![1; 5]);
    }
}
