//! Ban Sync: the servers in this server's group, and the owner's "Import existing bans" and
//! "Send the rest" buttons. Nothing here bans or unbans: both buttons queue a job for the bot, so its
//! hold, rate cap and exemptions apply to them too.
use crate::{
    access::Level,
    api::{is_discord_owner, log_change, require, Res},
    auth::Sess,
    store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tommybot_core::jobs;

pub fn routes() -> Router<AppState> {
    let g = "/api/guilds/{guild}/ban_sync";
    Router::new()
        .route(g, get(group))
        .route(&format!("{g}/import"), post(import))
        .route(&format!("{g}/send_rest"), post(send_rest))
}

/// The other servers with the same Discord owner, confirmed live: `guild_owners` is only a candidate list.
pub(crate) async fn siblings(st: &AppState, guild: i64) -> Result<Vec<i64>, ApiError> {
    let owner = st.discord.guild_owner(guild).await?;
    let Ok(owner_id) = owner.parse::<i64>() else {
        return Ok(vec![]);
    };
    let ids: Vec<i64> = sqlx::query_scalar(
        "select guild_id from guild_owners where owner_id = $1 and guild_id <> $2 order by guild_id",
    )
    .bind(owner_id)
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    let mut out = vec![];
    for id in ids {
        // A server the bot left, or that changed hands, is left out until the bot sees it again.
        if st.discord.guild_owner(id).await.is_ok_and(|o| o == owner) {
            out.push(id);
        }
    }
    Ok(out)
}

async fn group(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Admin, false).await?;
    let mut servers = vec![];
    for id in siblings(&st, guild).await? {
        let (_, config) = store::current(&st.db, id).await?;
        let name = st
            .discord
            .guild_name(id)
            .await?
            .unwrap_or_else(|| id.to_string());
        servers.push(json!({
            "id": id.to_string(),
            "name": name,
            "enabled": config["ban_sync"]["enabled"] == true,
            "receive": config["ban_sync"]["receive"].as_str().unwrap_or("alert"),
        }));
    }
    // Bans held back by the hourly cap; only the owner's button sends them.
    let waiting: i64 = sqlx::query_scalar(
        "select count(*) from ban_sync_links where source_guild = $1 and state = 'skipped' and skip_reason = 'rate cap'",
    )
    .bind(guild)
    .fetch_one(&st.db)
    .await?;
    Ok(Json(json!({"servers": servers, "waiting": waiting})))
}

/// The Discord owner after 2FA, with Ban Sync on here: what both buttons need.
async fn owner_with_sync_on(st: &AppState, s: &Sess, guild: i64) -> Result<(), ApiError> {
    require(st, s, guild, Level::Owner, true).await?;
    if !is_discord_owner(st, s, guild).await? {
        return Err(ApiError::Forbidden(
            "only the server owner can use Ban Sync",
        ));
    }
    let (_, config) = store::current(&st.db, guild).await?;
    if config["ban_sync"]["enabled"] != true {
        return Err(ApiError::BadRequest(
            "Turn Ban Sync on and save first.".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Import {
    source: String,
}

/// Copies the bans that already exist in `source` to this server, through the bot.
async fn import(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Import>,
) -> Res {
    owner_with_sync_on(&st, &s, guild).await?;
    let source: i64 = b
        .source
        .parse()
        .map_err(|_| ApiError::BadRequest("source must be a server id".into()))?;
    if !siblings(&st, guild).await?.contains(&source) {
        return Err(ApiError::BadRequest(
            "that server is not in this server's Ban Sync group".into(),
        ));
    }
    let payload = json!({"source": source.to_string(), "target": guild.to_string()});
    let now = chrono::Utc::now().timestamp();
    jobs::enqueue(&st.db, guild, "ban_sync_import", payload, now).await?;
    announce(
        &st,
        &s,
        guild,
        &format!("started a Ban Sync import of the bans in server {source}"),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

/// Sends this server's bans that the hourly cap held back.
async fn send_rest(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    owner_with_sync_on(&st, &s, guild).await?;
    let payload = json!({"source": guild.to_string()});
    let now = chrono::Utc::now().timestamp();
    jobs::enqueue(&st.db, guild, "ban_sync_send_rest", payload, now).await?;
    announce(
        &st,
        &s,
        guild,
        "sent the Ban Sync bans the hourly limit held back",
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn announce(st: &AppState, s: &Sess, guild: i64, what: &str) -> Result<(), ApiError> {
    let (_, config) = store::current(&st.db, guild).await?;
    let text = format!("{} (<@{}>) {what}", s.username, s.user_id);
    log_change(st, guild, &config, None, text).await;
    Ok(())
}
