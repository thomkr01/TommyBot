use crate::{
    access::Level,
    api::{self, Res},
    auth::Sess,
    schema, store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use tommybot_logging::LogEvent;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/schema", get(schema_route))
        .route("/api/guilds/{guild}/discord", get(discord_route))
        .route("/api/guilds/{guild}/wizard/auto-setup", post(auto_setup))
        .route("/api/guilds/{guild}/logging/setup", post(log_setup))
}

async fn schema_route(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    Ok(Json(json!(schema::schema())))
}

async fn discord_route(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    let list = |v: Value| v.as_array().cloned().unwrap_or_default();
    let channels = list(
        st.discord
            .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
            .await?,
    )
    .into_iter()
    .filter_map(|c| {
        let kind = match c["type"].as_i64()? {
            0 => "text",
            4 => "category",
            5 => "announcement",
            _ => return None,
        };
        Some(json!({"id": c["id"], "name": c["name"], "kind": kind}))
    })
    .collect::<Vec<_>>();
    let roles = list(st.discord.call(Method::GET, &format!("/guilds/{guild}/roles"), None).await?).into_iter()
        .map(|r| json!({"id": r["id"], "name": r["name"], "managed": r["managed"], "position": r["position"], "permissions": r["permissions"]}))
        .collect::<Vec<_>>();
    let info = st
        .discord
        .call(
            Method::GET,
            &format!("/guilds/{guild}?with_counts=true"),
            None,
        )
        .await?;
    Ok(Json(
        json!({"channels": channels, "roles": roles, "members": info["approximate_member_count"]}),
    ))
}

#[derive(Deserialize)]
struct Setup {
    expected_version: i32,
}

async fn auto_setup(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Setup>,
) -> Res {
    api::require(&st, &s, guild, Level::Admin, true).await?;
    let (cur, mut config) = store::current(&st.db, guild).await?;
    if cur != b.expected_version {
        return Err(ApiError::Conflict(cur)); // before any Discord side effect
    }
    let role = create(
        &st,
        guild,
        own(&config["antinuke"]["quarantine_role"]),
        "roles",
        json!({"name": "Quarantine", "permissions": "0"}),
    )
    .await?;
    let logs = create(
        &st,
        guild,
        own(&config["logging"]["fallback"]),
        "channels",
        json!({"name": "logs", "type": 0}),
    )
    .await?;
    let modlogs = create(
        &st,
        guild,
        own(&config["logging"]["routes"]["mod_command"]),
        "channels",
        json!({"name": "modlogs", "type": 0}),
    )
    .await?;
    config["antinuke"]["quarantine_role"] = json!(role);
    config["logging"]["fallback"] = json!(logs);
    config["logging"]["routes"]["mod_command"] = json!(modlogs);
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    api::log_change(
        &st,
        &config,
        format!(
            "{} (<@{}>) ran auto-setup -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(
        json!({"version": version, "created": {"quarantine_role": role, "logs": logs, "modlogs": modlogs}}),
    ))
}

fn at<'a>(config: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(config, |v, k| &v[*k])
}

fn own(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

/// Returns `existing`, or creates the role or channel on Discord and returns its id.
async fn create(
    st: &AppState,
    guild: i64,
    existing: Option<String>,
    path: &str,
    body: Value,
) -> Result<String, ApiError> {
    if let Some(id) = existing {
        return Ok(id);
    }
    let r = st
        .discord
        .call(Method::POST, &format!("/guilds/{guild}/{path}"), Some(body))
        .await?;
    Ok(r["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("discord returned no id"))?
        .to_string())
}

/// Wick's log types, each with its own channel: (config path, channel name).
const LOG_CHANNELS: [(&[&str], &str); 11] = [
    (&["logging", "fallback"], "tommybot-logs"),
    (&["logging", "routes", "mod_command"], "mod-logs"),
    (&["logging", "routes", "report"], "reports"),
    (&["appeals", "channel"], "appeals"),
    (&["logging", "routes", "auto_mod"], "automod-logs"),
    (&["logging", "routes", "anti_nuke"], "antinuke-logs"),
    (&["logging", "routes", "verification"], "verification-logs"),
    (&["logging", "routes", "join_gate"], "joingate-logs"),
    (&["logging", "routes", "join_raid"], "joinraid-logs"),
    (&["logging", "routes", "panic"], "panic-logs"),
    (&["logging", "routes", "server"], "server-logs"),
];

/// One click: a private "Logs" category with a channel per log type that has none yet, plus
/// "discord-logs" for every Discord event without a channel. Existing channels are kept.
async fn log_setup(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Setup>,
) -> Res {
    api::require(&st, &s, guild, Level::Admin, true).await?;
    let (cur, mut config) = store::current(&st.db, guild).await?;
    if cur != b.expected_version {
        return Err(ApiError::Conflict(cur)); // before any Discord side effect
    }
    let unset: Vec<String> = LogEvent::ALL
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok()?.as_str().map(String::from))
        .filter(|e| config["logging"]["events"][e].is_null())
        .collect();
    if unset.is_empty() && LOG_CHANNELS.iter().all(|(p, _)| !at(&config, p).is_null()) {
        return Err(ApiError::BadRequest(
            "every log type already has a channel".into(),
        ));
    }
    // Hidden from @everyone (its role id is the guild id); the bot (its user id is the app id)
    // keeps View Channel, Send Messages and Embed Links. Staff see it through their roles.
    let category = create(
        &st,
        guild,
        None,
        "channels",
        json!({"name": "Logs", "type": 4, "permission_overwrites": [
            {"id": guild.to_string(), "type": 0, "deny": "1024"},
            {"id": st.discord.client_id, "type": 1, "allow": "19456"}
        ]}),
    )
    .await?;
    let channel = |name: &str| json!({"name": name, "type": 0, "parent_id": category});
    let mut created = vec![];
    for (path, name) in LOG_CHANNELS {
        if at(&config, path).is_null() {
            let id = create(&st, guild, None, "channels", channel(name)).await?;
            let (last, parents) = path.split_last().expect("paths are not empty");
            let slot = parents.iter().fold(&mut config, |v, k| &mut v[*k]);
            slot[*last] = json!(id);
            created.push(name);
        }
    }
    if !unset.is_empty() {
        let id = create(&st, guild, None, "channels", channel("discord-logs")).await?;
        for e in &unset {
            config["logging"]["events"][e] = json!(id);
        }
        created.push("discord-logs");
    }
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    api::log_change(
        &st,
        &config,
        format!(
            "{} (<@{}>) created the log channels -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version, "created": created})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_channel_paths_are_real_log_routes() {
        let mut config = json!({});
        for (i, (path, _)) in LOG_CHANNELS.iter().enumerate() {
            let (last, parents) = path.split_last().unwrap();
            parents.iter().fold(&mut config, |v, k| &mut v[*k])[*last] = json!((i + 1).to_string());
        }
        let log: tommybot_logging::LogConfig =
            serde_json::from_value(config["logging"].clone()).unwrap();
        assert!(log.fallback.is_some());
        assert_eq!(log.routes.len(), 9);
        assert!(at(&config, &["appeals", "channel"]).is_string());
    }
}
