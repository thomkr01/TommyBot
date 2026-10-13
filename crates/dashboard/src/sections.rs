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

/// Puts a field on a dashboard page of its section (no page: the General page).
fn pg(mut v: Value, page: &str) -> Value {
    v["page"] = json!(page);
    v
}

/// Overrides the label the field shows (default: the last path segment).
fn ti(mut v: Value, title: &str) -> Value {
    v["title"] = json!(title);
    v
}

fn pretty(key: &str) -> String {
    let mut out = String::new();
    for (i, c) in key.chars().enumerate() {
        match c {
            '_' => out.push(' '),
            c if c.is_uppercase() && i > 0 => out.extend([' ', c.to_ascii_lowercase()]),
            c => out.push(c),
        }
    }
    let mut cs = out.chars();
    cs.next()
        .map(|c| c.to_uppercase().chain(cs).collect())
        .unwrap_or_default()
}

/// (filter key, dashboard page, card title).
const FILTERS: &[(&str, &str, &str)] = &[
    ("regular", "spam_text", "Regular message"),
    ("similar", "spam_text", "Similar message"),
    ("emojis", "spam_text", "Emojis"),
    ("newlines", "spam_text", "New lines"),
    ("chars_upper", "spam_text", "Characters (uppercase)"),
    ("chars_lower", "spam_text", "Characters (lowercase)"),
    ("zalgo", "spam_text", "Zalgo"),
    ("suspicion", "spam_text", "Suspicion"),
    ("inactive_channel", "spam_text", "Inactive channel"),
    ("invites", "invites", "Invite links"),
    ("bad_sites", "malicious", "Bad websites"),
    ("scam_links", "malicious", "Nitro and Steam scam links"),
    ("nsfw_links", "nsfw", "NSFW links"),
    ("words.filter", "words", "Word blacklist"),
    ("links", "links", "Link blacklist"),
];
/// Heat weights shown on the card of the filter they feed.
const HEAT_FOR: &[(&str, &str)] = &[
    ("regular", "base"),
    ("similar", "similarity"),
    ("similar", "similarity_floor"),
    ("emojis", "per_emoji"),
    ("newlines", "per_newline"),
    ("chars_lower", "per_char"),
    ("chars_lower", "free_chars"),
    ("suspicion", "suspicious_account"),
    ("inactive_channel", "inactive_channel"),
    ("words.filter", "per_blacklisted_word"),
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
    let mut nuke = vec![
        dv(fg("Status", "enabled", "bool"), true),
        fg("Auto quarantine", "quarantine_role", "role"),
    ];
    for a in NUKE {
        let g = pretty(a);
        nuke.extend([
            pg(
                ti(
                    fg(&g, &format!("limits.{a}.per_minute"), "int"),
                    "Minute limit",
                ),
                "limits",
            ),
            pg(
                ti(fg(&g, &format!("limits.{a}.per_hour"), "int"), "Hour limit"),
                "limits",
            ),
        ]);
    }
    let mut auto = vec![
        fg("Status", "enabled", "bool"),
        ti(
            fg("Auto timeouts", "strike.strike_timeout_ms", "int"),
            "Regular strike duration",
        ),
        ti(
            fg("Auto timeouts", "strike.cap_timeout_ms", "int"),
            "CAP strike duration",
        ),
        ti(
            fg("Auto timeouts", "strike.cap", "int"),
            "Strikes before the CAP strike",
        ),
        ti(
            fg("Auto timeouts", "strike.reoffend_window_ms", "int"),
            "Reoffend window",
        ),
        ti(
            fg("Auto timeouts", "strike.strike_expiry_ms", "int"),
            "Strike expiry",
        ),
        fg("Heat settings", "heat.max", "float"),
        ti(
            fg("Heat settings", "heat.decay_per_sec", "float"),
            "Heat degradation (every second)",
        ),
        fg("Heat settings", "heat.webhook_multiplier", "float"),
        ti(
            fg("Heat settings", "strike.reset_heat_on_timeout", "bool"),
            "Set current heat to 0 after punishment",
        ),
    ];
    for (k, page, title) in FILTERS {
        let mut action = pg(
            ti(
                fg(title, &format!("filters.{k}.action"), "choice"),
                "Trigger action",
            ),
            page,
        );
        action["options"] = json!(FILTER_ACTIONS.iter().chain(&[""]).collect::<Vec<_>>());
        action["none_label"] = json!("strike system");
        auto.extend([
            pg(
                dv(
                    ti(
                        fg(title, &format!("filters.{k}.enabled"), "bool"),
                        "Use this filter",
                    ),
                    true,
                ),
                page,
            ),
            action,
            pg(
                ti(
                    fg(title, &format!("filters.{k}.duration_secs"), "int"),
                    "Action duration",
                ),
                page,
            ),
        ]);
        auto.extend(HEAT_FOR.iter().filter(|(f, _)| f == k).map(|(_, h)| {
            pg(
                fg(
                    title,
                    &format!("heat.{h}"),
                    if h.starts_with("free") {
                        "int"
                    } else {
                        "float"
                    },
                ),
                page,
            )
        }));
    }
    auto.extend([
        pg(fg("Mentions", "heat.per_mention", "float"), "spam_mentions"),
        pg(
            fg("Mentions", "heat.per_mass_mention", "float"),
            "spam_mentions",
        ),
        pg(
            fg("Attachments", "heat.per_attachment", "float"),
            "spam_attach",
        ),
        pg(fg("Attachments", "heat.per_embed", "float"), "spam_attach"),
        pg(
            fg("Attachments", "heat.per_sticker", "float"),
            "spam_attach",
        ),
        pg(
            fg("NSFW links", "filters.nsfw_crawl_shorteners", "bool"),
            "nsfw",
        ),
        pg(
            fg("Word blacklist", "filters.words.strict", "list"),
            "words",
        ),
        pg(
            fg("Word blacklist", "filters.words.wildcard", "list"),
            "words",
        ),
        pg(
            fg("Word blacklist", "filters.words.premade_profane", "bool"),
            "words",
        ),
        pg(
            fg("Word blacklist", "filters.words.premade_vulgar", "bool"),
            "words",
        ),
        pg(
            fg("Word blacklist", "filters.words.premade_racist", "bool"),
            "words",
        ),
        pg(
            fg(
                "Word blacklist",
                "filters.words.use_discord_automod",
                "bool",
            ),
            "words",
        ),
        pg(
            fg("Link blacklist", "filters.blacklisted_domains", "list"),
            "links",
        ),
    ]);
    let mut gate: Vec<Value> = GATE
        .iter()
        .map(|k| {
            let mut v = fg(&pretty(k), k, "choice");
            v["options"] = json!(["Log", "Kick", "Ban"]); // Timeout(ms) is a tuple variant: raw editor
            ti(v, "Trigger action")
        })
        .collect();
    gate.extend([
        dv(fg("General", "enabled", "bool"), true),
        fg("General", "require_all", "bool"),
        fg("Username", "username_terms", "list"),
    ]);
    let choice = |g: &str, path: &str, opts: &[&str]| {
        let mut v = fg(g, path, "choice");
        v["options"] = json!(opts);
        v
    };
    let raid = vec![
        dv(fg("General", "enabled", "bool"), true),
        f("count", "int"),
        f("span_ms", "int"),
        f("suspicious_only", "bool"),
        f("active_ms", "int"),
        f("string_patterns", "list"),
        f("flag_younger_than_ms", "int"),
        f("flag_no_avatar", "bool"),
        f("id_flag_min", "int"),
        dv(
            choice("Response", "action", &["Log", "Kick", "Ban"]),
            "Kick",
        ),
        choice("Response", "lock", &["", "Server", "Blind", "Role", "All"]),
        fg("Response", "warned_roles", "roles"),
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
    let verification = vec![
        fg("General", "enabled", "bool"),
        dv(
            choice("General", "target", &["Everyone", "SuspiciousOnly"]),
            "Everyone",
        ),
        dv(
            choice(
                "General",
                "on_fail",
                &["Nothing", "Kick", "Ban", "Quarantine"],
            ),
            "Nothing",
        ),
        dv(
            choice(
                "General",
                "mode",
                &["Button", "Captcha", "SimpleCaptcha", "Dm", "Web"],
            ),
            "Captcha",
        ),
        dv(
            ti(
                fg("General", "time_limit_ms", "int"),
                "Verification duration",
            ),
            120_000,
        ),
        fg("General", "channel", "channel"),
        fg("General", "roles", "roles"),
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
        (
            "lockdown",
            "Lockdown",
            vec![
                f("main_roles", "roles"),
                ti(
                    dv(fg("Lock scope", "channels", "bool"), true),
                    "Lock server channels",
                ),
                ti(
                    dv(fg("Lock scope", "roles", "bool"), false),
                    "Lock staff roles",
                ),
                ti(
                    dv(fg("Lock scope", "joins", "bool"), false),
                    "Lock server joins",
                ),
                ti(fg("Lock scope", "updates", "bool"), "Make lockdown updates"),
            ],
        ),
        ("panic", "Panic mode", panic),
        ("appeals", "Appeals", appeals),
        ("verification", "Verification", verification),
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
