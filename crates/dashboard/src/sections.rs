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
use tommybot_antinuke::security::{private_overwrites, synced_children};
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
        .route("/api/guilds/{guild}/catch/setup", post(catch_channel))
        .route("/api/guilds/{guild}/tickets/setup", post(tickets_setup))
        .route(
            "/api/guilds/{guild}/tickets/log/{channel}/{message}",
            get(ticket_log),
        )
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
        .map(|r| json!({"id": r["id"], "name": r["name"], "managed": r["managed"], "position": r["position"], "permissions": r["permissions"], "color": r["color"]}))
        .collect::<Vec<_>>();
    let info = st
        .discord
        .call(
            Method::GET,
            &format!("/guilds/{guild}?with_counts=true"),
            None,
        )
        .await?;
    // The wizard's score checks the bot's own roles.
    let bot_roles = st
        .discord
        .member_roles(guild, &st.discord.client_id)
        .await
        .unwrap_or_default();
    Ok(Json(
        json!({"channels": channels, "roles": roles, "members": info["approximate_member_count"], "bot_roles": bot_roles}),
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
    /// Log channels to create besides the essential two; absent means only those two.
    logs: Option<Vec<String>>,
}

/// Default staff permits, made with a role each when a server has none:
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
        ],
        &["automod", "reports", "words"],
    ),
];

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
    let role = quarantine_role(&st, guild, &config).await?;
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
    let quarantine_failed = quarantine_channels(&st, guild, &role, &config).await;
    api::log_change(
        &st,
        guild,
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

/// "Setup channel": a #verify-here channel only unverified members see, holding the
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
    // The bot falls back to the main roles when no verification roles are set.
    let roles = |s: &str, k: &str| config[s][k].as_array().filter(|a| !a.is_empty()).cloned();
    let given = roles("verification", "roles")
        .or_else(|| roles("lockdown", "main_roles"))
        .unwrap_or_default();
    for r in &given {
        overwrites.push(json!({"id": r, "type": 0, "deny": "1024"}));
    }
    // Members in the join quarantine read it too; the bot's enforce_quarantine knows this overwrite.
    if let Some(q) = own(&config["antinuke"]["quarantine_role"]) {
        overwrites.push(json!({"id": q, "type": 0, "allow": "66560", "deny": "2048"}));
    }
    let existing = own(&config["verification"]["channel"]);
    if existing.is_none() {
        let channels = st
            .discord
            .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
            .await?;
        drop_named(&st, &channels, &config, "verify-here").await?;
    }
    let channel = create(
        &st,
        guild,
        existing,
        "channels",
        json!({"name": "verify-here", "type": 0, "permission_overwrites": overwrites}),
    )
    .await?;
    config["verification"]["channel"] = json!(channel);
    // The same panel as the bot's `verifysetup`, so both look alike in Discord.
    let server = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}"), None)
        .await?;
    let (embed, buttons) =
        tommybot_logging::embed::verify_panel(server["name"].as_str().unwrap_or("the server"));
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    st.discord
        .call(
            Method::POST,
            &format!("/channels/{channel}/messages"),
            Some(json!({"embeds": [embed], "components": [buttons]})),
        )
        .await?;
    // The channel stays, but nobody can pass until there is a role to hand out.
    let warning = given.is_empty().then_some(NO_VERIFY_ROLE);
    Ok(Json(
        json!({"version": version, "channel": channel, "warning": warning}),
    ))
}

const NO_VERIFY_ROLE: &str = "The verification channel is ready, but members cannot pass yet: there is no role to give them. Pick a main role under Misc or the verification roles under Verification, then save.";

const CATCH_WARNING: &str =
    "Do not write here. Writing = instant ban. Account hacked? Change your password.";

/// The catch channel: on top of the list (uncategorized channels sit above every category),
/// @everyone may only read and write plain text. Turns the catch channel on; an already set
/// channel is reused.
async fn catch_channel(
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
    let existing = own(&config["catch"]["channel"]);
    let fresh = existing.is_none();
    // @everyone: View, Send, History; no reactions, embeds or files. The bot also deletes and pins.
    let overwrites = json!([
        {"id": guild.to_string(), "type": 0, "allow": "68608", "deny": "49216"},
        {"id": st.discord.client_id, "type": 1, "allow": "93184"}
    ]);
    let channel = create(
        &st,
        guild,
        existing,
        "channels",
        json!({"name": "do-not-write", "type": 0, "position": 0, "topic": CATCH_WARNING, "permission_overwrites": overwrites}),
    )
    .await?;
    config["catch"]["channel"] = json!(channel);
    config["catch"]["enabled"] = json!(true);
    if fresh {
        config["catch"]["keep_top"] = json!(true);
    }
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    api::log_change(
        &st,
        guild,
        &config,
        format!(
            "{} (<@{}>) set up the catch channel -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    if fresh {
        let m = st
            .discord
            .call(
                Method::POST,
                &format!("/channels/{channel}/messages"),
                Some(json!({"embeds": [{"title": "Do not write here", "color": 0xE71B1B, "description": CATCH_WARNING}]})),
            )
            .await?;
        if let Some(id) = m["id"].as_str() {
            let pin = format!("/channels/{channel}/messages/pins/{id}");
            st.discord.call(Method::PUT, &pin, None).await?;
        }
    }
    Ok(Json(json!({"version": version, "channel": channel})))
}

/// One click: a private "Tickets" category with #ticket-logs. Channels already set are reused.
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
    let log = ticket_channels(&st, guild, &mut config).await?;
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    api::log_change(
        &st,
        guild,
        &config,
        format!(
            "{} (<@{}>) set up tickets -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version, "channel": log})))
}

/// Makes the Tickets category and #ticket-logs (each only when unset) and turns Tickets on in
/// `config`. Returns the log channel.
pub(crate) async fn ticket_channels(
    st: &AppState,
    guild: i64,
    config: &mut Value,
) -> Result<String, ApiError> {
    // Staff are the roles of permits with Manage Tickets plus the access roles.
    let mut staff: Vec<String> = tommybot_core::permits::roles_with(&st.db, guild, "tickets")
        .await?
        .iter()
        .map(i64::to_string)
        .collect();
    staff.extend(
        config["tickets"]["access_roles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(String::from)),
    );
    // The bot reads the history for the log and posts files there.
    let mut hidden = vec![
        json!({"id": guild.to_string(), "type": 0, "deny": "1024"}),
        json!({"id": st.discord.client_id, "type": 1, "allow": "117760"}),
    ];
    hidden.extend(
        staff
            .iter()
            .map(|r| json!({"id": r, "type": 0, "allow": "117760"})),
    );
    let t = config["tickets"].clone();
    let category = create(
        st,
        guild,
        own(&t["category"]),
        "channels",
        json!({"name": "Tickets", "type": 4, "permission_overwrites": hidden}),
    )
    .await?;
    let log = create(
        st,
        guild,
        own(&t["log_channel"]),
        "channels",
        json!({"name": "ticket-logs", "type": 0, "parent_id": category, "permission_overwrites": hidden}),
    )
    .await?;
    if !config["tickets"].is_object() {
        config["tickets"] = json!({});
    }
    let tickets = &mut config["tickets"];
    tickets["enabled"] = json!(true);
    tickets["category"] = json!(category);
    tickets["log_channel"] = json!(log);
    Ok(log)
}

/// The conversation file of a closed ticket, read from the ticket log message that holds it.
async fn ticket_log(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, channel, message)): Path<(i64, u64, u64)>,
) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    // The bot token reads every server's channels, so only this server's files are served.
    let ch = st
        .discord
        .call(Method::GET, &format!("/channels/{channel}"), None)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if ch["guild_id"].as_str() != Some(guild.to_string().as_str()) {
        return Err(ApiError::NotFound);
    }
    let m = st
        .discord
        .call(
            Method::GET,
            &format!("/channels/{channel}/messages/{message}"),
            None,
        )
        .await
        .map_err(|_| ApiError::NotFound)?;
    if m["author"]["id"].as_str() != Some(st.discord.client_id.as_str()) {
        return Err(ApiError::NotFound);
    }
    let file = m["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| {
            a["filename"]
                .as_str()
                .is_some_and(|f| f.starts_with("ticket_log_"))
        })
        .ok_or(ApiError::NotFound)?;
    let url = file["url"].as_str().ok_or(ApiError::NotFound)?;
    let text = st
        .discord
        .http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(Json(json!({"file": file["filename"], "text": text})))
}

fn at<'a>(config: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(config, |v, k| &v[*k])
}

fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().and_then(|e| e.status())
        == Some(reqwest::StatusCode::NOT_FOUND)
}

fn own(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Deletes the text channels named `name` that `config` does not use, so a fresh setup
/// replaces a leftover channel instead of adding a second one with the same name.
async fn drop_named(
    st: &AppState,
    channels: &Value,
    config: &Value,
    name: &str,
) -> Result<(), ApiError> {
    for id in stale(channels, config, name) {
        st.discord
            .call(Method::DELETE, &format!("/channels/{id}"), None)
            .await?;
    }
    Ok(())
}

fn stale<'a>(channels: &'a Value, config: &Value, name: &str) -> Vec<&'a str> {
    let used = config.to_string();
    channels
        .as_array()
        .into_iter()
        .flatten()
        // Roles have no type; of channels only text channels count.
        .filter(|c| (c["type"] == 0 || c["type"].is_null()) && c["name"] == name)
        .filter_map(|c| c["id"].as_str())
        .filter(|id| !used.contains(&format!("\"{id}\"")))
        .collect()
}

/// The quarantine role. A picked role is kept and stripped of every permission. With none picked,
/// leftover roles named Quarantine are deleted first so the server never ends up with several.
/// A picked role that cannot be one (@everyone, managed, not below the bot) is replaced.
async fn quarantine_role(st: &AppState, guild: i64, config: &Value) -> Result<String, ApiError> {
    let picked = own(&config["antinuke"]["quarantine_role"]);
    let fit = match &picked {
        Some(id) => {
            let (roles, bot_pos) = crate::ops::roles_and_bot_pos(st, guild).await?;
            // A role missing from the list is handled below as deleted in Discord.
            roles
                .iter()
                .find(|r| r.id == *id)
                .is_none_or(|r| r.unfit(guild, bot_pos).is_none())
        }
        None => false,
    };
    if let Some(id) = picked.filter(|_| fit) {
        let r = st
            .discord
            .call(
                Method::PATCH,
                &format!("/guilds/{guild}/roles/{id}"),
                Some(json!({"permissions": "0"})),
            )
            .await;
        // A saved role someone deleted in Discord is made again below instead of failing the setup.
        match r {
            Ok(_) => return Ok(id),
            Err(e) if !not_found(&e) => return Err(e.into()),
            Err(_) => {}
        }
    }
    let roles = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
        .await?;
    for id in stale(&roles, config, "Quarantine") {
        st.discord
            .call(Method::DELETE, &format!("/guilds/{guild}/roles/{id}"), None)
            .await?;
    }
    create(
        st,
        guild,
        None,
        "roles",
        json!({"name": "Quarantine", "permissions": "0"}),
    )
    .await
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
        let at = match path {
            "roles" => format!("/guilds/{guild}/roles/{id}"),
            _ => format!("/channels/{id}"),
        };
        // Reuse it only while it still exists; one deleted in Discord is made again.
        match st.discord.call(Method::GET, &at, None).await {
            Ok(_) => return Ok(id),
            Err(e) if !not_found(&e) => return Err(e.into()),
            Err(_) => {}
        }
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

/// Log types, each with its own channel: (config path, channel name).
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

/// One click: the essential log channels plus the named ones (a channel per log type, "discord-logs"
/// for every Discord event without one), in private "Logs" and "Staff" categories. Set channels are kept.
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
    let created = log_channels(&st, guild, &mut config, b.logs.as_deref()).await?;
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
        guild,
        &config,
        format!(
            "{} (<@{}>) created the log channels -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version, "created": created})))
}

/// Sets the quarantine overwrite on every channel but the verification channel (members in the
/// join quarantine must see it) and returns how many it could not set (the bot lacks Manage Roles
/// there); `None` when the channels could not be listed. Runs after the commit, so a failure here
/// loses no setup.
pub(crate) async fn quarantine_channels(
    st: &AppState,
    guild: i64,
    role: &str,
    config: &Value,
) -> Option<usize> {
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await
        .ok()?;
    let body = json!({"type": 0, "deny": tommybot_core::sections::QUARANTINE_DENY.to_string()});
    let mut failed = 0;
    let verification = own(&config["verification"]["channel"]);
    for id in channels
        .as_array()?
        .iter()
        .filter_map(|c| c["id"].as_str())
        .filter(|id| verification.as_deref() != Some(id))
    {
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

/// Essential logs, always created: the main log channel and the mod log.
const ESSENTIAL: [&str; 2] = ["tommybot-logs", "mod-logs"];

/// Staff-facing channels, kept in their own "Staff" category instead of "Logs".
const STAFF_CHANNELS: [&str; 2] = ["reports", "appeals"];

/// The overwrite PUTs, as (channel, target, body), that make the reused category `id` and its
/// synced channels private; none when it already is. The children get the same overwrites so they stay synced.
fn privatize(
    guild: i64,
    channels: &Value,
    id: &str,
    bot: &str,
    main: &[u64],
) -> Vec<(u64, u64, Value)> {
    let img = crate::recovery::live_image(guild, &json!([]), channels);
    let Some(cat) = img.channels.iter().find(|c| c.id.to_string() == id) else {
        return vec![];
    };
    let ows = private_overwrites(cat, guild as u64, bot.parse().unwrap_or_default(), main);
    std::iter::once(cat)
        .chain(synced_children(&img.channels, cat))
        .flat_map(|c| {
            ows.iter()
                .map(|o| (c.id, o.target, crate::ops::overwrite_body(o)))
        })
        .collect()
}

/// The private category `name`: the existing one, so a later run adds to it, or a new one.
/// Hidden from @everyone (its role id is the guild id) and the main roles; the bot (its user id
/// is the app id) keeps View Channel, Send Messages and Embed Links. Staff see it through their
/// roles. A reused category that anyone can see is made private first.
async fn category(
    st: &AppState,
    guild: i64,
    channels: &Value,
    name: &str,
    main: &[u64],
) -> Result<String, ApiError> {
    let existing = channels
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|c| (c["type"] == 4 && c["name"] == name).then(|| own(&c["id"]))?);
    if let Some(id) = &existing {
        for (ch, target, body) in privatize(guild, channels, id, &st.discord.client_id, main) {
            st.discord
                .call(
                    Method::PUT,
                    &format!("/channels/{ch}/permissions/{target}"),
                    Some(body),
                )
                .await?;
        }
    }
    create(
        st,
        guild,
        existing,
        "channels",
        json!({"name": name, "type": 4, "permission_overwrites": [
            {"id": guild.to_string(), "type": 0, "deny": "1024"},
            {"id": st.discord.client_id, "type": 1, "allow": "19456"}
        ]}),
    )
    .await
}

/// Creates the missing log channels under "Logs" (reports and appeals under "Staff"), and
/// sets them in `config`.
/// `only` adds the named channels to the essentials ("discord-logs" for the Discord events);
/// `None` makes just the essentials, as Auto Setup does. Returns the names of the channels it created; nothing when all have one.
async fn log_channels(
    st: &AppState,
    guild: i64,
    config: &mut Value,
    only: Option<&[String]>,
) -> Result<Vec<&'static str>, ApiError> {
    let wanted =
        |name: &str| ESSENTIAL.contains(&name) || only.is_some_and(|l| l.iter().any(|n| n == name));
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
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await?;
    // `None` is the shared discord-logs channel for every unset Discord event.
    let todo: Vec<(Option<&[&str]>, &str)> = missing
        .into_iter()
        .map(|(path, name)| (Some(path), name))
        .chain((!unset.is_empty()).then_some((None, "discord-logs")))
        .collect();
    let category_of = |name: &str| {
        if STAFF_CHANNELS.contains(&name) {
            "Staff"
        } else {
            "Logs"
        }
    };
    let main: Vec<u64> = config["lockdown"]["main_roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str()?.parse().ok())
        .collect();
    let mut parents = vec![];
    for cat in ["Logs", "Staff"] {
        if todo.iter().any(|(_, name)| category_of(name) == cat) {
            parents.push((cat, category(st, guild, &channels, cat, &main).await?));
        }
    }
    let mut created = vec![];
    for (path, name) in todo {
        let parent = parents
            .iter()
            .find(|(c, _)| *c == category_of(name))
            .map(|(_, id)| id);
        drop_named(st, &channels, config, name).await?;
        let body = json!({"name": name, "type": 0, "parent_id": parent});
        let id = create(st, guild, None, "channels", body).await?;
        match path.and_then(|p| p.split_last()) {
            Some((last, parents)) => {
                parents.iter().fold(&mut *config, |v, k| &mut v[*k])[*last] = json!(id)
            }
            None => {
                for e in &unset {
                    config["logging"]["events"][e] = json!(id);
                }
            }
        }
        created.push(name);
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

    #[test]
    fn a_reused_public_category_and_its_synced_channels_turn_private() {
        let public = json!([
            {"id": "10", "name": "Logs", "type": 4, "permission_overwrites": [{"id": "99", "type": 0, "allow": "1024", "deny": "0"}]},
            {"id": "11", "name": "mod-logs", "type": 0, "parent_id": "10", "permission_overwrites": [{"id": "99", "type": 0, "allow": "1024", "deny": "0"}]},
            {"id": "12", "name": "chat", "type": 0, "parent_id": "10", "permission_overwrites": []}
        ]);
        let puts = privatize(1, &public, "10", "7", &[99]);
        let hit: Vec<_> = puts
            .iter()
            .map(|(c, t, b)| (*c, *t, b["deny"].as_str().unwrap()))
            .collect();
        // @everyone and the main role 99 lose View, the bot is let in; the synced child follows, chat (not synced) does not.
        assert_eq!(
            hit,
            [
                (10, 1, "1024"),
                (10, 99, "1024"),
                (10, 7, "0"),
                (11, 1, "1024"),
                (11, 99, "1024"),
                (11, 7, "0")
            ]
        );
        let private = json!([{"id": "10", "name": "Logs", "type": 4, "permission_overwrites": [{"id": "1", "type": 0, "allow": "0", "deny": "1024"}]}]);
        assert!(privatize(1, &private, "10", "7", &[99]).is_empty());
    }

    #[test]
    fn stale_skips_used_and_other_channels() {
        let channels = json!([
            {"id": "1", "name": "reports", "type": 0},
            {"id": "2", "name": "reports", "type": 0},
            {"id": "3", "name": "reports", "type": 4},
            {"id": "4", "name": "appeals", "type": 0}
        ]);
        let config = json!({"logging": {"events": {"ban": "2"}}});
        assert_eq!(stale(&channels, &config, "reports"), ["1"]);
        let roles = json!([{"id": "5", "name": "Quarantine"}, {"id": "6", "name": "Quarantine"}, {"id": "7", "name": "Member"}]);
        let config = json!({"misc": {"quarantine_role": "6"}});
        assert_eq!(stale(&roles, &config, "Quarantine"), ["5"]);
    }
}
