//! Lockdown page (manage + settings check) and native Discord AutoMod rules.
use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post, put},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use tommybot_automod::{
    discord_api::{rule_body, RuleSpec},
    lockdown::{kind_from, tshoot, Op, RoleInfo},
};
use tommybot_core::jobs;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/lockdown", get(status))
        .route("/api/guilds/{guild}/lockdown/lock", post(lock))
        .route("/api/guilds/{guild}/lockdown/unlock", post(unlock))
        .route("/api/guilds/{guild}/lockdown/check", get(check))
        .route("/api/guilds/{guild}/lockdown/fix", post(fix))
        .route(
            "/api/guilds/{guild}/discord-rules",
            get(rules).post(create_rule),
        )
        .route(
            "/api/guilds/{guild}/discord-rules/{rule}",
            put(update_rule).delete(delete_rule),
        )
        .route(
            "/api/guilds/{guild}/discord-rules/{rule}/enabled",
            post(set_enabled),
        )
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

async fn status(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let row: Option<(String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("select kind, created_at from lock_snapshots where guild_id = $1")
            .bind(guild)
            .fetch_optional(&st.db)
            .await?;
    let unlock_at: Option<(chrono::DateTime<chrono::Utc>,)> = sqlx::query_as(
        "select run_at from scheduled_jobs where guild_id = $1 and kind = 'unlock' order by run_at limit 1",
    )
    .bind(guild)
    .fetch_optional(&st.db)
    .await?;
    Ok(Json(json!({
        "locked": row.as_ref().map(|(k, at)| json!({"kind": k, "since": at})),
        "unlock_at": unlock_at.map(|(t,)| t),
    })))
}

#[derive(Deserialize)]
struct LockBody {
    kind: String,
    channel: Option<String>,
    duration_s: Option<i64>,
}

async fn lock(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<LockBody>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let channel = b.channel.as_deref().and_then(|c| c.parse().ok());
    kind_from(&b.kind, channel).ok_or_else(|| ApiError::BadRequest("unknown lock kind".into()))?;
    jobs::enqueue(
        &st.db,
        guild,
        "lock",
        json!({"kind": b.kind, "channel": b.channel, "duration_s": b.duration_s}),
        now(),
    )
    .await?;
    audit(&st, &s, guild, format!("locked the server ({})", b.kind)).await?;
    Ok(Json(json!({"ok": true})))
}

async fn unlock(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    sqlx::query("delete from scheduled_jobs where guild_id = $1 and kind = 'unlock'")
        .bind(guild)
        .execute(&st.db)
        .await?;
    jobs::enqueue(&st.db, guild, "unlock", Value::Null, now()).await?;
    audit(&st, &s, guild, "unlocked the server".into()).await?;
    Ok(Json(json!({"ok": true})))
}

async fn audit(st: &AppState, s: &Sess, guild: i64, what: String) -> Result<(), ApiError> {
    let (_, c) = crate::store::current(&st.db, guild).await?;
    log_change(
        st,
        &c,
        format!(
            "{} (<@{}>) {what} from the dashboard",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(())
}

/// Roles as the lock planner sees them, plus the bot's top role position.
async fn roles(st: &AppState, guild: i64) -> Result<(Vec<RoleInfo>, i32), ApiError> {
    let raw = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
        .await?;
    let id = |v: &Value| v["id"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let info: Vec<RoleInfo> = raw
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| RoleInfo {
            id: id(r),
            perms: r["permissions"]
                .as_str()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0),
            position: r["position"].as_i64().unwrap_or(0) as i32,
            managed: r["managed"].as_bool().unwrap_or(false),
        })
        .collect();
    let me = st.discord.call(Method::GET, "/users/@me", None).await?;
    let mine = st
        .discord
        .member_roles(guild, me["id"].as_str().unwrap_or_default())
        .await?;
    let bot_pos = info
        .iter()
        .filter(|r| mine.contains(&r.id.to_string()))
        .map(|r| r.position)
        .max()
        .unwrap_or(0);
    Ok((info, bot_pos))
}

async fn problems(
    st: &AppState,
    guild: i64,
) -> Result<Vec<tommybot_automod::lockdown::Problem>, ApiError> {
    let (_, c) = crate::store::current(&st.db, guild).await?;
    let main: Vec<u64> = c["lockdown"]["main_roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str()?.parse().ok())
        .collect();
    let (info, bot_pos) = roles(st, guild).await?;
    Ok(tshoot(guild as u64, &main, &info, bot_pos))
}

async fn check(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    Ok(Json(json!(problems(&st, guild).await?)))
}

#[derive(Deserialize)]
struct FixBody {
    id: String,
}

async fn fix(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<FixBody>,
) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let p = problems(&st, guild)
        .await?
        .into_iter()
        .find(|p| p.id == b.id)
        .ok_or(ApiError::NotFound)?;
    let Some(Op::SetRolePerms { role, perms }) = p.fix else {
        return Err(ApiError::BadRequest(
            "this problem has no automatic fix".into(),
        ));
    };
    st.discord
        .call(
            Method::PATCH,
            &format!("/guilds/{guild}/roles/{role}"),
            Some(json!({"permissions": perms.to_string()})),
        )
        .await?;
    audit(
        &st,
        &s,
        guild,
        format!("applied the lockdown fix `{}`", b.id),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn rules(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let raw = st
        .discord
        .call(
            Method::GET,
            &format!("/guilds/{guild}/auto-moderation/rules"),
            None,
        )
        .await?;
    let list: Vec<Value> = raw
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let mut v = json!(RuleSpec::from_discord(r));
            v["id"] = r["id"].clone();
            v
        })
        .collect();
    Ok(Json(json!(list)))
}

fn body(spec: &RuleSpec) -> Result<Value, ApiError> {
    rule_body(spec).map_err(|e| ApiError::BadRequest(e.to_string()))
}

async fn create_rule(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(spec): Json<RuleSpec>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let b = body(&spec)?;
    st.discord
        .call(
            Method::POST,
            &format!("/guilds/{guild}/auto-moderation/rules"),
            Some(b),
        )
        .await?;
    audit(
        &st,
        &s,
        guild,
        format!("created the Discord AutoMod rule `{}`", spec.name),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

/// Rule ids are snowflakes; anything else would let a caller reach other Discord endpoints.
fn rule_path(guild: i64, rule: &str) -> Result<String, ApiError> {
    if rule.is_empty() || !rule.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::BadRequest("bad rule id".into()));
    }
    Ok(format!("/guilds/{guild}/auto-moderation/rules/{rule}"))
}

async fn update_rule(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, rule)): Path<(i64, String)>,
    Json(spec): Json<RuleSpec>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let b = body(&spec)?;
    st.discord
        .call(Method::PATCH, &rule_path(guild, &rule)?, Some(b))
        .await?;
    audit(
        &st,
        &s,
        guild,
        format!("edited the Discord AutoMod rule `{}`", spec.name),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn set_enabled(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, rule)): Path<(i64, String)>,
    Json(b): Json<EnabledBody>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    st.discord
        .call(
            Method::PATCH,
            &rule_path(guild, &rule)?,
            Some(json!({"enabled": b.enabled})),
        )
        .await?;
    audit(
        &st,
        &s,
        guild,
        format!(
            "{} a Discord AutoMod rule",
            if b.enabled {
                "activated"
            } else {
                "deactivated"
            }
        ),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn delete_rule(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, rule)): Path<(i64, String)>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    st.discord
        .call(Method::DELETE, &rule_path(guild, &rule)?, None)
        .await?;
    audit(&st, &s, guild, "deleted a Discord AutoMod rule".into()).await?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ids_must_be_snowflakes() {
        assert!(rule_path(1, "123").is_ok());
        assert!(rule_path(1, "../roles/1").is_err());
        assert!(rule_path(1, "").is_err());
    }
}
