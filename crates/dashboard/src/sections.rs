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

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/schema", get(schema_route))
        .route("/api/guilds/{guild}/discord", get(discord_route))
        .route("/api/guilds/{guild}/wizard/auto-setup", post(auto_setup))
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
    let create = |existing: Option<String>, path: &'static str, body: Value| {
        let (d, p) = (st.discord.clone(), format!("/guilds/{guild}/{path}"));
        async move {
            if let Some(id) = existing {
                return Ok::<_, ApiError>(id);
            }
            let r = d.call(Method::POST, &p, Some(body)).await?;
            Ok(r["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("discord returned no id"))?
                .to_string())
        }
    };
    let own = |v: &Value| v.as_str().map(String::from);
    let role = create(
        own(&config["antinuke"]["quarantine_role"]),
        "roles",
        json!({"name": "Quarantine", "permissions": "0"}),
    )
    .await?;
    let logs = create(
        own(&config["logging"]["fallback"]),
        "channels",
        json!({"name": "logs", "type": 0}),
    )
    .await?;
    let modlogs = create(
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
