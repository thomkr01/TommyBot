use crate::{
    access::Level,
    api::{self, Res},
    auth::Sess,
    store, ApiError, AppState,
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

const HEAT: &[&str] = &[
    "max",
    "decay_per_sec",
    "base",
    "per_char",
    "free_chars",
    "per_newline",
    "per_emoji",
    "per_mention",
    "per_mass_mention",
    "per_attachment",
    "per_embed",
    "per_sticker",
    "inactive_channel",
    "per_blacklisted_word",
    "suspicious_account",
    "similarity",
    "similarity_floor",
    "webhook_multiplier",
];
const LOG_KINDS: &[&str] = &[
    "message",
    "member",
    "role",
    "channel",
    "server",
    "invite",
    "webhook",
    "emoji",
    "sticker",
    "scheduled_event",
    "thread",
    "report",
    "auto_mod",
    "anti_nuke",
    "verification",
    "join_gate",
    "join_raid",
    "panic",
    "mod_command",
];
const NUKE: &[&str] = &[
    "ChannelCreate",
    "ChannelDelete",
    "RoleCreate",
    "RoleDelete",
    "Ban",
    "Kick",
    "WebhookCreate",
    "WebhookDelete",
    "Prune",
    "BotAdd",
];
const GATE: &[&str] = &[
    "no_avatar",
    "unauthorized_bot",
    "unverified_bot",
    "invite_in_name",
    "suspicious",
    "username_filter",
];

fn f(path: &str, kind: &str) -> Value {
    json!({"path": path, "label": path.replace(['.', '_'], " "), "kind": kind})
}

/// `f` placed in a named card instead of the one its path prefix would give.
fn fg(group: &str, path: &str, kind: &str) -> Value {
    let mut v = f(path, kind);
    v["group"] = json!(group);
    v
}

/// Shows `default` for a field the stored config leaves out (matches the Rust `Default`).
fn dv(mut v: Value, default: impl Into<Value>) -> Value {
    v["default"] = default.into();
    v
}

const FILTERS: &[&str] = &[
    "regular",
    "similar",
    "emojis",
    "newlines",
    "chars_upper",
    "chars_lower",
    "zalgo",
    "suspicion",
    "inactive_channel",
    "invites",
    "bad_sites",
    "scam_links",
    "nsfw_links",
    "links",
    "words.filter",
];
const FILTER_ACTIONS: [&str; 5] = ["delete", "warn", "timeout", "kick", "ban"];

/// Typed form fields per section; paths mirror the serde types validated in `api::validate`.
fn schema() -> Vec<Value> {
    let mut log = vec![f("fallback", "channel"), f("ignored_channels", "channels")];
    log.extend(
        LOG_KINDS
            .iter()
            .map(|k| f(&format!("routes.{k}"), "channel")),
    );
    let mut nuke = vec![f("quarantine_role", "role")];
    for a in NUKE {
        nuke.extend([
            f(&format!("limits.{a}.per_minute"), "int"),
            f(&format!("limits.{a}.per_hour"), "int"),
        ]);
    }
    let mut auto = vec![f("enabled", "bool")];
    auto.extend(HEAT.iter().map(|k| {
        f(
            &format!("heat.{k}"),
            if k.starts_with("free") {
                "int"
            } else {
                "float"
            },
        )
    }));
    for k in FILTERS {
        let g = k.trim_end_matches(".filter").replace('_', " ");
        let mut action = fg(&g, &format!("filters.{k}.action"), "choice");
        action["options"] = json!(FILTER_ACTIONS.iter().chain(&[""]).collect::<Vec<_>>());
        action["none_label"] = json!("strike system");
        auto.extend([
            dv(fg(&g, &format!("filters.{k}.enabled"), "bool"), true),
            action,
            fg(&g, &format!("filters.{k}.duration_secs"), "int"),
        ]);
    }
    auto.extend([
        fg("nsfw links", "filters.nsfw_crawl_shorteners", "bool"),
        fg("words", "filters.words.strict", "list"),
        fg("words", "filters.words.wildcard", "list"),
        fg("words", "filters.words.premade_profane", "bool"),
        fg("words", "filters.words.premade_vulgar", "bool"),
        fg("words", "filters.words.premade_racist", "bool"),
        fg("words", "filters.words.use_discord_automod", "bool"),
        fg("links", "filters.blacklisted_domains", "list"),
    ]);
    auto.extend([
        f("strike.cap", "int"),
        f("strike.strike_timeout_ms", "int"),
        f("strike.cap_timeout_ms", "int"),
        f("strike.reoffend_window_ms", "int"),
        f("strike.strike_expiry_ms", "int"),
        f("strike.reset_heat_on_timeout", "bool"),
    ]);
    let mut gate: Vec<Value> = GATE
        .iter()
        .map(|k| {
            let mut v = f(k, "choice");
            v["options"] = json!(["Log", "Kick", "Ban"]); // Timeout(ms) is a tuple variant: raw editor
            v
        })
        .collect();
    gate.extend([f("username_terms", "list"), f("require_all", "bool")]);
    let raid = vec![
        f("count", "int"),
        f("span_ms", "int"),
        f("suspicious_only", "bool"),
        f("active_ms", "int"),
        f("string_patterns", "list"),
        f("flag_younger_than_ms", "int"),
        f("flag_no_avatar", "bool"),
        f("id_flag_min", "int"),
    ];
    let panic = vec![
        f("executors", "int"),
        dv(f("window_secs", "int"), 600),
        dv(f("duration_secs", "int"), 600),
        f("lock_mod_commands", "bool"),
        f("unlock_when_ending", "bool"),
        f("warned_roles", "roles"),
        f("heat_algorithm", "bool"),
        f("whitelisted_categories", "channels"),
    ];
    let whitelist = vec![
        f("users", "list"),
        f("roles", "roles"),
        f("channels", "channels"),
        f("categories", "channels"),
        f("webhooks", "list"),
    ];
    let misc = vec![
        dv(fg("General", "prefix", "text"), "w!"),
        dv(fg("General", "use_slash", "bool"), true),
        fg("General", "compact_replies", "bool"),
        fg("General", "delete_replies_secs", "int"),
        fg("General", "enforce_quarantine_perms", "bool"),
        fg("General", "ignore_no_permit", "bool"),
        fg("Statics", "modlogs_channel", "channel"),
        fg("Moderation commands", "confirm", "bool"),
        fg("Moderation commands", "always_dm", "bool"),
        fg("Moderation commands", "delete_invocation", "bool"),
        fg("Moderation commands", "hide_staff_in_dms", "bool"),
        dv(
            fg("Moderation commands", "default_timeout_secs", "int"),
            600,
        ),
        dv(fg("Moderation commands", "ban_purge_days", "int"), 0),
        dv(fg("Auto moderation", "show_reason_on_auto", "bool"), true),
        fg("Auto moderation", "purge_last_100", "bool"),
        dv(fg("Auto moderation", "staff_only_permitted", "bool"), true),
        dv(fg("Auto warn actions", "auto_warn_actions", "bool"), true),
        dv(fg("Auto warn actions", "warn_base_points", "int"), 1),
    ];
    let appeals = vec![
        f("enabled", "bool"),
        f("channel", "channel"),
        f("fields", "form"),
    ];
    [
        ("logging", "Logging", log),
        ("antinuke", "Anti-nuke", nuke),
        ("automod", "Automod", auto),
        ("joingate", "Join gate", gate),
        ("joinraid", "Join raid", raid),
        ("lockdown", "Lockdown", vec![f("main_roles", "roles")]),
        ("panic", "Panic mode", panic),
        ("appeals", "Appeals", appeals),
        ("whitelist", "Whitelist", whitelist),
        ("misc", "Miscellaneous", misc),
    ]
    .into_iter()
    .map(|(n, t, fields)| json!({"name": n, "title": t, "fields": fields}))
    .collect()
}

async fn schema_route(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    Ok(Json(json!(schema())))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_path_deserializes() {
        for sec in schema() {
            let mut obj = json!({});
            for fd in sec["fields"].as_array().unwrap() {
                let sample = match fd["kind"].as_str().unwrap() {
                    "bool" => json!(true),
                    "int" => json!(5),
                    "float" => json!(1.5),
                    "text" => json!("x"),
                    "list" => json!(["1"]),
                    "choice" => fd["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|o| o.as_str() != Some(""))
                        .unwrap()
                        .clone(),
                    "channel" | "role" => json!("1"),
                    "form" => json!([{"label": "x"}]),
                    _ => json!(["1"]),
                };
                let mut cur = &mut obj;
                let parts: Vec<_> = fd["path"].as_str().unwrap().split('.').collect();
                for p in &parts[..parts.len() - 1] {
                    cur = &mut cur[*p];
                }
                cur[parts[parts.len() - 1]] = sample;
            }
            let name = sec["name"].as_str().unwrap();
            api::validate(name, &obj)
                .unwrap_or_else(|_| panic!("{name} schema does not deserialize"));
        }
    }
}
