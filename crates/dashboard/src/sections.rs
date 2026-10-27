use crate::{
    access::Level,
    api::{self, Res},
    auth::Sess,
    schema, store, ApiError, AppState,
};
use axum::{
    extract::{Path, Query, State},
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
        .route("/api/guilds/{guild}/members", get(members_route))
        .route("/api/guilds/{guild}/wizard/auto-setup", post(auto_setup))
        .route("/api/guilds/{guild}/logging/setup", post(log_setup))
        .route(
            "/api/guilds/{guild}/wizard/verify-channel",
            post(verify_channel),
        )
        .route("/api/guilds/{guild}/tickets/setup", post(tickets_setup))
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
struct MemberQ {
    /// Name to search for.
    #[serde(default)]
    q: String,
    /// Or: comma-separated user ids to show by name.
    #[serde(default)]
    ids: String,
}

/// The user picker: members matching `q`, or the members behind `ids` (at most 25, unknown ids left out).
async fn members_route(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(m): Query<MemberQ>,
) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    let found = if m.q.trim().is_empty() {
        let mut found = vec![];
        // Digits only: the id becomes part of the Discord path.
        let ids = m
            .ids
            .split(',')
            .filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()));
        for id in ids.take(25) {
            if let Ok(x) = st
                .discord
                .call(Method::GET, &format!("/guilds/{guild}/members/{id}"), None)
                .await
            {
                found.push(x);
            }
        }
        found
    } else {
        let q = crate::discord::urlencode(m.q.trim());
        st.discord
            .call(
                Method::GET,
                &format!("/guilds/{guild}/members/search?limit=10&query={q}"),
                None,
            )
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    let list = found
        .iter()
        .filter(|m| m["user"]["id"].is_string())
        .map(|m| {
            let u = &m["user"];
            let name = m["nick"]
                .as_str()
                .or(u["global_name"].as_str())
                .or(u["username"].as_str());
            json!({"id": u["id"], "name": name.unwrap_or_default()})
        })
        .collect::<Vec<_>>();
    Ok(Json(json!(list)))
}

#[derive(Deserialize)]
struct Setup {
    expected_version: i32,
    /// Log channels to create besides the essential two; absent means all of them.
    logs: Option<Vec<String>>,
}

/// Wick's default staff permits, made with a role each when a server has none:
/// (name, rank, perms, whitelist). Admins are created first so they sit above Moderators.
const STAFF: [(&str, i32, &[&str], &[&str]); 2] = [
    (
        "Admins",
        2,
        &[
            "cases",
            "ban",
            "kick",
            "timeout",
            "warn",
            "slowmode",
            "purge",
            "sanitize",
            "reports",
            "lock_channels",
            "tickets",
            "lock_joins",
        ],
        &["automod", "reports", "words"],
    ),
    (
        "Moderators",
        1,
        &[
            "cases",
            "kick",
            "timeout",
            "warn",
            "slowmode",
            "purge",
            "sanitize",
            "reports",
            "lock_channels",
            "tickets",
        ],
        &["automod", "reports", "words"],
    ),
];

/// Wick's quarantine overwrite: no View Channel, Send Messages, Add Reactions or Connect.
const QUARANTINE_DENY: &str = "1051712";

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
    config["antinuke"]["quarantine_role"] = json!(role);
    // Lockdown and verification need a main role; members get "Member" when they verify.
    let empty = |v: &Value| v.as_array().is_none_or(Vec::is_empty);
    let mut member = None;
    if empty(&config["lockdown"]["main_roles"]) {
        let id = create(&st, guild, None, "roles", json!({"name": "Member"})).await?;
        config["lockdown"]["main_roles"] = json!([id]);
        if empty(&config["verification"]["roles"]) {
            config["verification"]["roles"] = json!([id]);
        }
        member = Some(id);
    }
    let mut permits = store::read_permits(&mut *st.db.acquire().await?, guild).await?;
    let staff = empty(&permits["custom"]);
    if staff {
        let mut custom = vec![];
        for (name, rank, perms, whitelist) in STAFF {
            let id = create(&st, guild, None, "roles", json!({"name": name})).await?;
            custom.push(json!({"name": name, "rank": rank, "perms": perms, "whitelist": whitelist, "roles": [id]}));
        }
        permits["custom"] = json!(custom);
        config["permits"] = permits;
    }
    let logs = log_channels(&st, guild, &mut config, b.logs.as_deref()).await?;
    let mut tx = st.db.begin().await?;
    if staff {
        store::write_permits(&mut tx, guild, &config["permits"]).await?;
    }
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    let quarantine_failed = quarantine_channels(&st, guild, &role).await;
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
        json!({"version": version, "created": {"quarantine_role": role, "member_role": member, "staff": staff, "logs": logs}, "quarantine_failed": quarantine_failed}),
    ))
}

/// Wick's "Setup channel": a #verify-here channel only unverified members see, holding the
/// verify button. An already set verification channel is reused.
async fn verify_channel(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Setup>,
) -> Res {
    api::require(&st, &s, guild, Level::Admin, true).await?;
    let (cur, mut config) = store::current(&st.db, guild).await?;
    if cur != b.expected_version {
        return Err(ApiError::Conflict(cur));
    }
    // @everyone reads but cannot write; verified members (the verification roles) stop seeing it.
    let mut overwrites = vec![
        json!({"id": guild.to_string(), "type": 0, "allow": "66560", "deny": "2048"}),
        json!({"id": st.discord.client_id, "type": 1, "allow": "19456"}),
    ];
    for r in config["verification"]["roles"]
        .as_array()
        .into_iter()
        .flatten()
    {
        overwrites.push(json!({"id": r, "type": 0, "deny": "1024"}));
    }
    let channel = create(
        &st,
        guild,
        own(&config["verification"]["channel"]),
        "channels",
        json!({"name": "verify-here", "type": 0, "permission_overwrites": overwrites}),
    )
    .await?;
    config["verification"]["channel"] = json!(channel);
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    st.discord
        .call(
            Method::POST,
            &format!("/channels/{channel}/messages"),
            Some(json!({
                "embeds": [{"title": "Verification Required!", "color": 0x0D9DF8,
                    "description": "To access the server, you need to pass the verification first."}],
                "components": [{"type": 1, "components": [
                    {"type": 2, "style": 3, "label": "Verify", "custom_id": "verify:start"}
                ]}]
            })),
        )
        .await?;
    Ok(Json(json!({"version": version, "channel": channel})))
}

/// One click: a private "Tickets" category with #ticket-logs, and #open-a-ticket holding the
/// panel. Channels already set are reused; the panel is posted again on every run.
async fn tickets_setup(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Setup>,
) -> Res {
    api::require(&st, &s, guild, Level::Admin, true).await?;
    let (cur, mut config) = store::current(&st.db, guild).await?;
    if cur != b.expected_version {
        return Err(ApiError::Conflict(cur));
    }
    // Staff are the roles of permits with Manage Tickets; they get View Channel and Read History.
    let staff: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT r.role_id FROM permit_roles r JOIN permits p ON p.id = r.permit_id
         WHERE p.guild_id = $1 AND 'tickets' = ANY(p.perms)",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    // The bot also needs Attach Files in #ticket-logs for the transcripts.
    let bot = json!({"id": st.discord.client_id, "type": 1, "allow": "19456"});
    let mut hidden = vec![
        json!({"id": guild.to_string(), "type": 0, "deny": "1024"}),
        json!({"id": st.discord.client_id, "type": 1, "allow": "52224"}),
    ];
    hidden.extend(
        staff
            .iter()
            .map(|r| json!({"id": r.to_string(), "type": 0, "allow": "66560"})),
    );
    let t = config["tickets"].clone();
    let category = create(
        &st,
        guild,
        own(&t["category"]),
        "channels",
        json!({"name": "Tickets", "type": 4, "permission_overwrites": hidden}),
    )
    .await?;
    let log = create(
        &st,
        guild,
        own(&t["log_channel"]),
        "channels",
        json!({"name": "ticket-logs", "type": 0, "parent_id": category, "permission_overwrites": hidden}),
    )
    .await?;
    // @everyone reads the panel but cannot write there.
    let panel = create(
        &st,
        guild,
        own(&t["panel_channel"]),
        "channels",
        json!({"name": "open-a-ticket", "type": 0, "permission_overwrites": [
            {"id": guild.to_string(), "type": 0, "allow": "66560", "deny": "2048"}, bot
        ]}),
    )
    .await?;
    let tickets = &mut config["tickets"];
    tickets["enabled"] = json!(true);
    tickets["category"] = json!(category);
    tickets["log_channel"] = json!(log);
    tickets["panel_channel"] = json!(panel);
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    st.discord
        .call(
            Method::POST,
            &format!("/channels/{panel}/messages"),
            Some(json!({
                "embeds": [{"title": "Need help?", "color": 0x0D9DF8,
                    "description": "Press the button to open a private ticket with the staff team."}],
                "components": [{"type": 1, "components": [
                    {"type": 2, "style": 1, "label": "Open a ticket", "custom_id": "ticket:open"}
                ]}]
            })),
        )
        .await?;
    api::log_change(
        &st,
        &config,
        format!(
            "{} (<@{}>) set up tickets -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version, "channel": panel})))
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
    let created = log_channels(&st, guild, &mut config, None).await?;
    if created.is_empty() {
        return Err(ApiError::BadRequest(
            "every log type already has a channel".into(),
        ));
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

/// Sets the quarantine overwrite on every channel and returns how many it could not set (the
/// bot lacks Manage Roles there); `None` when the channels could not be listed. Runs after the
/// commit, so a failure here loses no setup.
async fn quarantine_channels(st: &AppState, guild: i64, role: &str) -> Option<usize> {
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await
        .ok()?;
    let body = json!({"type": 0, "deny": QUARANTINE_DENY});
    // shortcut: no 429 retry, so a rate-limited channel counts as failed; running it again fixes it.
    let mut failed = 0;
    for id in channels.as_array()?.iter().filter_map(|c| c["id"].as_str()) {
        let path = format!("/channels/{id}/permissions/{role}");
        if st
            .discord
            .call(Method::PUT, &path, Some(body.clone()))
            .await
            .is_err()
        {
            failed += 1;
        }
    }
    Some(failed)
}

/// Wick's essential logs, always created: the main log channel and the mod log.
const ESSENTIAL: [&str; 2] = ["tommybot-logs", "mod-logs"];

/// Creates the "Logs" category and the missing log channels, and sets them in `config`.
/// `only` limits it to the essentials plus the named channels ("discord-logs" for the Discord
/// events). Returns the names of the channels it created; nothing when all have one.
async fn log_channels(
    st: &AppState,
    guild: i64,
    config: &mut Value,
    only: Option<&[String]>,
) -> Result<Vec<&'static str>, ApiError> {
    let wanted =
        |name: &str| ESSENTIAL.contains(&name) || only.is_none_or(|l| l.iter().any(|n| n == name));
    let missing: Vec<_> = LOG_CHANNELS
        .into_iter()
        .filter(|(p, name)| at(config, p).is_null() && wanted(name))
        .collect();
    let unset: Vec<String> = if wanted("discord-logs") {
        LogEvent::ALL
            .iter()
            .filter_map(|e| serde_json::to_value(e).ok()?.as_str().map(String::from))
            .filter(|e| config["logging"]["events"][e].is_null())
            .collect()
    } else {
        vec![]
    };
    if unset.is_empty() && missing.is_empty() {
        return Ok(vec![]);
    }
    // A later run adds to the existing category instead of making a second one.
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await?;
    let existing = channels
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|c| (c["type"] == 4 && c["name"] == "Logs").then(|| own(&c["id"]))?);
    // Hidden from @everyone (its role id is the guild id); the bot (its user id is the app id)
    // keeps View Channel, Send Messages and Embed Links. Staff see it through their roles.
    let category = create(
        st,
        guild,
        existing,
        "channels",
        json!({"name": "Logs", "type": 4, "permission_overwrites": [
            {"id": guild.to_string(), "type": 0, "deny": "1024"},
            {"id": st.discord.client_id, "type": 1, "allow": "19456"}
        ]}),
    )
    .await?;
    let channel = |name: &str| json!({"name": name, "type": 0, "parent_id": category});
    let mut created = vec![];
    for (path, name) in missing {
        let id = create(st, guild, None, "channels", channel(name)).await?;
        let (last, parents) = path.split_last().expect("paths are not empty");
        let slot = parents.iter().fold(&mut *config, |v, k| &mut v[*k]);
        slot[*last] = json!(id);
        created.push(name);
    }
    if !unset.is_empty() {
        let id = create(st, guild, None, "channels", channel("discord-logs")).await?;
        for e in &unset {
            config["logging"]["events"][e] = json!(id);
        }
        created.push("discord-logs");
    }
    Ok(created)
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
