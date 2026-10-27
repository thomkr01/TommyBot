//! Embed builder: stored embeds the bot posts as its own message and later edits in place.
use crate::{
    access::Level,
    api::{self, Res},
    auth::Sess,
    lockdown::audit,
    ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};

type Row = (
    i64,
    String,
    Value,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub fn routes() -> Router<AppState> {
    let g = "/api/guilds/{guild}/embeds";
    Router::new()
        .route(g, get(list).post(save))
        .route(&format!("{g}/{{id}}/post"), post(publish))
        .route(&format!("{g}/{{id}}/delete"), post(delete))
}

fn chars(v: &Value) -> usize {
    v.as_str().map_or(0, |s| s.chars().count())
}

/// Discord's embed limits: past any of them Discord rejects the whole message.
fn check(e: &Value) -> Result<(), String> {
    if !e.is_object() {
        return Err("embed must be an object".into());
    }
    let fields = e["fields"].as_array().map_or(&[][..], Vec::as_slice);
    if fields.len() > 25 {
        return Err("at most 25 fields".into());
    }
    let mut total = 0;
    for (key, max) in [
        ("/title", 256),
        ("/description", 4096),
        ("/author/name", 256),
        ("/footer/text", 2048),
    ] {
        let n = e.pointer(key).map_or(0, chars);
        if n > max {
            return Err(format!("{} is longer than {max} characters", &key[1..]));
        }
        total += n;
    }
    for f in fields {
        let (n, v) = (chars(&f["name"]), chars(&f["value"]));
        if n == 0 || v == 0 {
            return Err("every field needs a name and a value".into());
        }
        if n > 256 || v > 1024 {
            return Err("field names are at most 256 characters, values 1024".into());
        }
        total += n + v;
    }
    if total > 6000 {
        return Err("the embed has more than 6000 characters in total".into());
    }
    if total == 0 && e.pointer("/image/url").is_none() && e.pointer("/thumbnail/url").is_none() {
        return Err("the embed is empty".into());
    }
    Ok(())
}

async fn list(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "select id, name, embed, channel_id::text, message_channel::text, message_id::text
         from embeds where guild_id = $1 order by name, id",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(
            |(id, name, embed, channel, message_channel, message)| json!({
                "id": id, "name": name, "embed": embed, "channel": channel,
                "message_channel": message_channel, "message": message,
            })
        )
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct Save {
    id: Option<i64>,
    name: String,
    embed: Value,
    channel: Option<String>,
}

/// Creates or updates a draft; the Discord message only changes on the next post.
async fn save(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Save>,
) -> Res {
    api::require(&st, &s, guild, Level::Editor, false).await?;
    let name = b.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::BadRequest(
            "name must be 1 to 100 characters".into(),
        ));
    }
    check(&b.embed).map_err(ApiError::BadRequest)?;
    let channel = match b
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        Some(c) => Some(
            c.parse::<i64>()
                .map_err(|_| ApiError::BadRequest("bad channel".into()))?,
        ),
        None => None,
    };
    let q = match b.id {
        Some(id) => sqlx::query_scalar(
            "update embeds set name = $3, embed = $4, channel_id = $5, updated_by = $6, updated_at = now()
             where id = $1 and guild_id = $2 returning id",
        )
        .bind(id),
        None => sqlx::query_scalar(
            "insert into embeds (guild_id, name, embed, channel_id, updated_by)
             values ($2, $3, $4, $5, $6) returning id",
        )
        .bind(None::<i64>),
    };
    let id: Option<i64> = q
        .bind(guild)
        .bind(name)
        .bind(&b.embed)
        .bind(channel)
        .bind(s.user_id)
        .fetch_optional(&st.db)
        .await?;
    Ok(Json(json!({"id": id.ok_or(ApiError::NotFound)?})))
}

/// Calls Discord as the bot; `None` means 404, the statuses a user can fix become readable errors.
async fn discord(
    st: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Option<Value>, ApiError> {
    let r = st.discord.send(method, path, body).await?;
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or_default();
    match status {
        200..=299 => Ok(Some(v)),
        404 => Ok(None),
        403 => Err(ApiError::BadRequest(
            "TommyBot needs View Channel, Send Messages and Embed Links in that channel".into(),
        )),
        400 => Err(ApiError::BadRequest(format!(
            "Discord refused the embed: {v}"
        ))),
        _ => Err(anyhow::anyhow!("discord {status}: {v}").into()),
    }
}

#[derive(Deserialize)]
struct Publish {
    #[serde(default)]
    repost: bool,
}

/// Posts the embed, or edits the bot's earlier message. A new channel or `repost` replaces the old message.
async fn publish(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, i64)>,
    Json(b): Json<Publish>,
) -> Res {
    api::require(&st, &s, guild, Level::Editor, false).await?;
    let (name, embed, channel, old_channel, old_msg): (String, Value, Option<i64>, Option<i64>, Option<i64>) =
        sqlx::query_as("select name, embed, channel_id, message_channel, message_id from embeds where id = $1 and guild_id = $2")
            .bind(id).bind(guild).fetch_optional(&st.db).await?.ok_or(ApiError::NotFound)?;
    let channel = channel.ok_or_else(|| ApiError::BadRequest("choose a channel first".into()))?;
    // The bot is in many servers: never let an editor of this one post into another's channel.
    let ch = discord(&st, Method::GET, &format!("/channels/{channel}"), None).await?;
    if ch.and_then(|c| c["guild_id"].as_str()?.parse().ok()) != Some(guild) {
        return Err(ApiError::BadRequest(
            "that channel is not in this server".into(),
        ));
    }
    let body = json!({"embeds": [embed], "allowed_mentions": {"parse": []}});
    let (message, verb) = match (old_channel, old_msg) {
        (Some(c), Some(m)) if c == channel && !b.repost => {
            discord(
                &st,
                Method::PATCH,
                &format!("/channels/{c}/messages/{m}"),
                Some(body),
            )
            .await?
            .ok_or_else(|| {
                ApiError::BadRequest("the message was deleted on Discord: repost it".into())
            })?;
            (m, "edited")
        }
        _ => {
            if let (Some(c), Some(m)) = (old_channel, old_msg) {
                discord(
                    &st,
                    Method::DELETE,
                    &format!("/channels/{c}/messages/{m}"),
                    None,
                )
                .await?;
            }
            let v = discord(
                &st,
                Method::POST,
                &format!("/channels/{channel}/messages"),
                Some(body),
            )
            .await?
            .ok_or_else(|| ApiError::BadRequest("that channel no longer exists".into()))?;
            let m = v["id"]
                .as_str()
                .and_then(|m| m.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("no message id"))?;
            sqlx::query("update embeds set message_channel = $1, message_id = $2 where id = $3")
                .bind(channel)
                .bind(m)
                .bind(id)
                .execute(&st.db)
                .await?;
            (m, "posted")
        }
    };
    audit(
        &st,
        &s,
        guild,
        format!("{verb} embed **{name}** in <#{channel}>"),
    )
    .await?;
    Ok(Json(
        json!({"message_channel": channel.to_string(), "message": message.to_string()}),
    ))
}

/// Deletes the embed and the bot's message (already gone on Discord is fine).
async fn delete(State(st): State<AppState>, s: Sess, Path((guild, id)): Path<(i64, i64)>) -> Res {
    api::require(&st, &s, guild, Level::Editor, false).await?;
    let (name, c, m): (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "delete from embeds where id = $1 and guild_id = $2 returning name, message_channel, message_id",
    )
    .bind(id)
    .bind(guild)
    .fetch_optional(&st.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    if let (Some(c), Some(m)) = (c, m) {
        discord(
            &st,
            Method::DELETE,
            &format!("/channels/{c}/messages/{m}"),
            None,
        )
        .await?;
    }
    audit(&st, &s, guild, format!("deleted embed **{name}**")).await?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::check;
    use serde_json::json;

    #[test]
    fn limits() {
        assert!(check(&json!({"title": "hi"})).is_ok());
        assert!(check(&json!({"image": {"url": "https://x/y.png"}})).is_ok());
        assert!(check(&json!({})).is_err());
        assert!(check(&json!("x")).is_err());
        assert!(check(&json!({"title": "a".repeat(257)})).is_err());
        assert!(check(&json!({"author": {"name": "a".repeat(257)}})).is_err());
        assert!(check(&json!({"fields": [{"name": "a", "value": ""}]})).is_err());
        assert!(check(&json!({"fields": vec![json!({"name": "a", "value": "b"}); 26]})).is_err());
        // Each part within its own limit, but over 6000 together.
        let big = json!({"description": "a".repeat(4096), "footer": {"text": "a".repeat(2000)}});
        assert!(check(&big).is_err());
    }
}
