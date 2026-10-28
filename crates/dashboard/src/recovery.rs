//! Backups and panic mode from the dashboard. Reads go to Postgres or Discord directly; anything
//! that changes the server is queued as a job for the bot, which owns the restore code.
use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashSet;
use tommybot_antinuke::{describe, plan, ChannelImg, Image, OverwriteImg, RoleImg};
use tommybot_core::jobs;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/backups", get(list).post(create))
        .route("/api/guilds/{guild}/backups/{id}/preview", get(preview))
        .route("/api/guilds/{guild}/backups/{id}/load", post(load))
        .route("/api/guilds/{guild}/backups/{id}/delete", post(delete))
        .route("/api/guilds/{guild}/panic", get(panic_status))
        .route("/api/guilds/{guild}/panic/start", post(panic_start))
        .route("/api/guilds/{guild}/panic/end", post(panic_end))
}

async fn list(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let rows: Vec<(i64, i64, i32, i32)> = sqlx::query_as(
        "SELECT id, EXTRACT(EPOCH FROM created_at)::BIGINT, jsonb_array_length(image->'roles')::INT, \
         jsonb_array_length(image->'channels')::INT FROM backups \
         WHERE guild_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, at, roles, channels)| json!({"id": id, "at": at, "roles": roles, "channels": channels}))
            .collect(),
    )))
}

async fn queue(st: &AppState, guild: i64, kind: &str, payload: Value) -> Result<(), ApiError> {
    jobs::enqueue(&st.db, guild, kind, payload, chrono::Utc::now().timestamp()).await?;
    Ok(())
}

async fn announce(st: &AppState, s: &Sess, guild: i64, what: &str) -> Result<(), ApiError> {
    let (_, config) = store::current(&st.db, guild).await?;
    log_change(
        st,
        &config,
        format!("{} (<@{}>) {what}", s.username, s.user_id),
    )
    .await;
    Ok(())
}

async fn create(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    queue(&st, guild, "backup_create", json!({})).await?;
    announce(&st, &s, guild, "took a backup").await?;
    Ok(Json(json!({"queued": true})))
}

async fn owned_image(st: &AppState, guild: i64, id: i64) -> Result<Image, ApiError> {
    let row: Option<(sqlx::types::Json<Image>,)> =
        sqlx::query_as("SELECT image FROM backups WHERE guild_id = $1 AND id = $2")
            .bind(guild)
            .bind(id)
            .fetch_optional(&st.db)
            .await?;
    row.map(|(i,)| i.0).ok_or(ApiError::NotFound)
}

fn num<T: std::str::FromStr + Default>(v: &Value) -> T {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or_default()
}

/// Roles and channels as Discord's REST API returns them, in the shape backups use.
pub(crate) fn live_image(guild: i64, roles: &Value, channels: &Value) -> Image {
    let items = |v: &Value| v.as_array().cloned().unwrap_or_default();
    Image {
        guild: guild as u64,
        name: String::new(),
        settings: None,
        roles: items(roles)
            .iter()
            .map(|r| RoleImg {
                id: num(&r["id"]),
                name: r["name"].as_str().unwrap_or("").into(),
                color: r["color"].as_u64().unwrap_or(0) as u32,
                permissions: num(&r["permissions"]),
                position: r["position"].as_i64().unwrap_or(0),
                hoist: r["hoist"].as_bool().unwrap_or(false),
                mentionable: r["mentionable"].as_bool().unwrap_or(false),
                managed: r["managed"].as_bool().unwrap_or(false),
            })
            .collect(),
        channels: items(channels)
            .iter()
            .map(|c| ChannelImg {
                id: num(&c["id"]),
                name: c["name"].as_str().unwrap_or("").into(),
                kind: c["type"].as_u64().unwrap_or(0) as u8,
                parent: c["parent_id"].as_str().and_then(|p| p.parse().ok()),
                position: c["position"].as_i64().unwrap_or(0),
                topic: c["topic"].as_str().map(String::from),
                nsfw: c["nsfw"].as_bool().unwrap_or(false),
                overwrites: items(&c["permission_overwrites"])
                    .iter()
                    .map(|o| OverwriteImg {
                        target: num(&o["id"]),
                        is_role: o["type"].as_u64() == Some(0),
                        allow: num(&o["allow"]),
                        deny: num(&o["deny"]),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Live ids a restore never deletes: the quarantine role and the log channels (as the bot does).
fn keep_set(config: &Value) -> HashSet<u64> {
    let mut keep: HashSet<u64> = HashSet::new();
    let logging = &config["logging"];
    let ids = std::iter::once(&config["antinuke"]["quarantine_role"])
        .chain(std::iter::once(&logging["fallback"]))
        .chain(
            logging["routes"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values()),
        );
    keep.extend(ids.filter_map(|v| v.as_str()?.parse::<u64>().ok()));
    keep
}

async fn preview(State(st): State<AppState>, s: Sess, Path((guild, id)): Path<(i64, i64)>) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let image = owned_image(&st, guild, id).await?;
    let (_, config) = store::current(&st.db, guild).await?;
    let (roles, channels) = (
        st.discord
            .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
            .await?,
        st.discord
            .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
            .await?,
    );
    let live = live_image(guild, &roles, &channels);
    let lines = describe(&plan(&image, &live, &keep_set(&config), false), &live);
    Ok(Json(json!({"lines": lines})))
}

async fn load(State(st): State<AppState>, s: Sess, Path((guild, id)): Path<(i64, i64)>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    owned_image(&st, guild, id).await?;
    queue(&st, guild, "backup_load", json!({"id": id})).await?;
    announce(&st, &s, guild, &format!("loaded backup #{id}")).await?;
    Ok(Json(json!({"queued": true})))
}

async fn delete(State(st): State<AppState>, s: Sess, Path((guild, id)): Path<(i64, i64)>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let done = sqlx::query("DELETE FROM backups WHERE guild_id = $1 AND id = $2")
        .bind(guild)
        .bind(id)
        .execute(&st.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    announce(&st, &s, guild, &format!("deleted backup #{id}")).await?;
    Ok(Json(json!({"deleted": true})))
}

async fn panic_status(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let ends: Option<i64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM run_at)::BIGINT FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(guild)
    .fetch_optional(&st.db)
    .await?;
    Ok(Json(json!({"active": ends.is_some(), "ends_at": ends})))
}

async fn panic_start(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    queue(&st, guild, "panic_start", json!({})).await?;
    announce(&st, &s, guild, "started panic mode").await?;
    Ok(Json(json!({"queued": true})))
}

/// Moves the pending `panic_end` job to now, so the bot ends panic mode with the unlock settings it stored.
async fn panic_end(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let moved = sqlx::query(
        "UPDATE scheduled_jobs SET run_at = now() WHERE guild_id = $1 AND kind = 'panic_end'",
    )
    .bind(guild)
    .execute(&st.db)
    .await?;
    if moved.rows_affected() == 0 {
        return Err(ApiError::BadRequest("panic mode is not running".into()));
    }
    announce(&st, &s, guild, "ended panic mode").await?;
    Ok(Json(json!({"queued": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_image_reads_discord_json() {
        let roles = json!([{"id": "5", "name": "Mod", "color": 255, "permissions": "8", "position": 2,
            "hoist": true, "mentionable": false, "managed": false}]);
        let channels = json!([{"id": "9", "name": "chat", "type": 0, "parent_id": "4", "position": 1,
            "topic": null, "nsfw": true,
            "permission_overwrites": [{"id": "5", "type": 0, "allow": "2048", "deny": "0"},
                                       {"id": "6", "type": 1, "allow": "0", "deny": "1024"}]}]);
        let img = live_image(1, &roles, &channels);
        assert_eq!(
            (
                img.roles[0].id,
                img.roles[0].permissions,
                img.roles[0].color
            ),
            (5, 8, 255)
        );
        let c = &img.channels[0];
        assert_eq!((c.id, c.parent, c.kind, c.nsfw), (9, Some(4), 0, true));
        assert_eq!(
            c.overwrites,
            vec![
                OverwriteImg {
                    target: 5,
                    is_role: true,
                    allow: 2048,
                    deny: 0
                },
                OverwriteImg {
                    target: 6,
                    is_role: false,
                    allow: 0,
                    deny: 1024
                },
            ]
        );
    }

    #[test]
    fn keep_set_holds_quarantine_role_and_log_channels() {
        let cfg = json!({"antinuke": {"quarantine_role": "7"},
            "logging": {"fallback": "8", "routes": {"panic": "9"}}});
        assert_eq!(keep_set(&cfg), HashSet::from([7, 8, 9]));
        assert!(keep_set(&json!({})).is_empty());
    }
}
