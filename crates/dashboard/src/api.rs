use crate::{
    access::{self, Facts, Level},
    auth::Sess,
    diff, store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub(crate) type Res = Result<Json<Value>, ApiError>;

/// Permissions granted by the user's permit roles.
pub(crate) async fn permit_perms(
    st: &AppState,
    s: &Sess,
    guild: i64,
) -> Result<Vec<String>, ApiError> {
    let roles: Vec<i64> = st
        .discord
        .member_roles(guild, &s.user_id.to_string())
        .await?
        .iter()
        .filter_map(|r| r.parse().ok())
        .collect();
    Ok(sqlx::query_scalar::<_, Vec<String>>(
        "select p.perms from permits p join permit_roles r on r.permit_id = p.id where p.guild_id = $1 and r.role_id = any($2)")
        .bind(guild).bind(&roles).fetch_all(&st.db).await?.concat())
}

async fn is_discord_owner(st: &AppState, s: &Sess, guild: i64) -> Result<bool, ApiError> {
    Ok(st.discord.guild_owner(guild).await? == s.user_id.to_string())
}

/// Re-resolved on every request, including writes (spec 3.1).
pub(crate) async fn level_of(st: &AppState, s: &Sess, guild: i64) -> Result<Level, ApiError> {
    let flag = |table: &'static str| async move {
        sqlx::query_scalar::<_, bool>(&format!(
            "select exists(select 1 from {table} where guild_id = $1 and user_id = $2)"
        ))
        .bind(guild)
        .bind(s.user_id)
        .fetch_one(&st.db)
        .await
    };
    let discord_owner = is_discord_owner(st, s, guild).await?;
    let (extra_owner, trusted_admin) = (flag("extra_owners").await?, flag("trusted_admins").await?);
    let perms = if discord_owner || extra_owner || trusted_admin {
        vec![]
    } else {
        permit_perms(st, s, guild).await?
    };
    Ok(access::level(&Facts {
        discord_owner,
        extra_owner,
        trusted_admin,
        permit_perms: &perms,
    }))
}

pub(crate) async fn require(
    st: &AppState,
    s: &Sess,
    guild: i64,
    min: Level,
    step_up: bool,
) -> Result<Level, ApiError> {
    let l = level_of(st, s, guild).await?;
    if l < min {
        return Err(ApiError::Forbidden("insufficient dashboard access"));
    }
    if step_up && !s.stepped_up {
        return Err(ApiError::StepUp);
    }
    Ok(l)
}

/// Dashboard access plus one of the given permits (Admin and above skip the permit).
pub(crate) async fn staff(
    st: &AppState,
    s: &Sess,
    guild: i64,
    perms: &[&str],
) -> Result<(), ApiError> {
    let l = require(st, s, guild, Level::Viewer, false).await?;
    if l >= Level::Admin
        || permit_perms(st, s, guild)
            .await?
            .iter()
            .any(|p| perms.contains(&p.as_str()))
    {
        Ok(())
    } else {
        Err(ApiError::Forbidden("needs a permit for this page"))
    }
}

/// The toggles of the permit editor, so the page never carries its own copy of the permission list.
pub async fn permit_catalog(_: Sess) -> Json<Value> {
    Json(json!(tommybot_core::permits::CATALOG
        .iter()
        .map(
            |s| json!({"id": s.id, "title": s.title, "items": s.items.iter().map(|d| json!({
            "key": d.key, "label": d.label, "help": d.help, "flag": d.flag, "locked": d.locked,
        })).collect::<Vec<_>>()})
        )
        .collect::<Vec<_>>()))
}

pub async fn guilds(State(st): State<AppState>, s: Sess) -> Res {
    let mut out = vec![];
    let bot_in = st.discord.bot_guild_ids().await?;
    for g in st.discord.user_guilds(&s.access_token).await? {
        let Ok(id) = g.id.parse::<i64>() else {
            continue;
        };
        if !bot_in.contains(&g.id) {
            continue;
        }
        let level = level_of(&st, &s, id).await?;
        if level > Level::None {
            out.push(json!({"id": g.id, "name": g.name, "icon": g.icon, "level": level}));
        }
    }
    Ok(Json(json!(out)))
}

pub async fn get_config(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let level = require(&st, &s, guild, Level::Viewer, false).await?;
    let (version, mut config) = store::current(&st.db, guild).await?;
    config["permits"] = store::read_permits(&mut *st.db.acquire().await?, guild).await?;
    Ok(Json(
        json!({"version": version, "config": config, "level": level, "sensitive": access::SENSITIVE}),
    ))
}

#[derive(Deserialize)]
pub struct Put {
    expected_version: i32,
    value: Value,
}

pub(crate) async fn log_change(st: &AppState, config: &Value, text: String) {
    if let Some(ch) = config["logging"]["config_changes"]
        .as_str()
        .map(String::from)
    {
        let d = st.discord.clone();
        tokio::spawn(async move {
            let log =
                tommybot_logging::Log::new("Configuration Changed", tommybot_logging::embed::BLUE)
                    .text(&text)
                    .time();
            if let Err(e) = d.post_log(&ch, log).await {
                tracing::warn!("config-changes log failed: {e:#}");
            }
        });
    }
}

pub async fn put_section(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, section)): Path<(i64, String)>,
    Json(b): Json<Put>,
) -> Res {
    if !access::SECTIONS.contains(&section.as_str()) {
        return Err(ApiError::NotFound);
    }
    if !b.value.is_object() {
        return Err(ApiError::BadRequest("section must be an object".into()));
    }
    validate(&section, &b.value)?;
    let (min, step_up) = access::write_bar(&section);
    require(&st, &s, guild, min, step_up).await?;
    let (_, mut config) = store::current(&st.db, guild).await?;
    config[&section] = b.value;
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        &config,
        format!(
            "{} (<@{}>) changed `{section}` -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version})))
}

/// Replaces EO/TA lists and custom permits; recorded as a config version so it can be rolled back.
pub async fn put_permits(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Put>,
) -> Res {
    let level = require(&st, &s, guild, Level::Admin, true).await?;
    let mut tx = st.db.begin().await?;
    let old = store::read_permits(&mut tx, guild).await?;
    // Only owners may change who administers the bot, and only the server owner who owns it.
    if level < Level::Owner && old["ta"] != b.value["ta"] {
        return Err(ApiError::Forbidden("only owners can change the TA list"));
    }
    if old["eo"] != b.value["eo"] && !is_discord_owner(&st, &s, guild).await? {
        return Err(ApiError::Forbidden(
            "only the server owner can change the Extra Owners list",
        ));
    }
    store::write_permits(&mut tx, guild, &b.value).await?;
    let (_, mut config) = store::current(&st.db, guild).await?;
    config["permits"] = store::read_permits(&mut tx, guild).await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        &config,
        format!(
            "{} (<@{}>) changed `permits` -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version})))
}

pub async fn history(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let rows: Vec<(i32, i64, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("select version, coalesce(changed_by, 0), created_at from guild_config where guild_id = $1 order by version desc limit 200")
            .bind(guild).fetch_all(&st.db).await?;
    Ok(Json(json!(rows
        .iter()
        .map(|(v, by, at)| json!({"version": v, "changed_by": by.to_string(), "at": at}))
        .collect::<Vec<_>>())))
}

async fn version(st: &AppState, guild: i64, v: i32) -> Result<Value, ApiError> {
    sqlx::query_scalar("select config from guild_config where guild_id = $1 and version = $2")
        .bind(guild)
        .bind(v)
        .fetch_optional(&st.db)
        .await?
        .ok_or(ApiError::NotFound)
}

pub async fn diff_versions(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, from, to)): Path<(i64, i32, i32)>,
) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    Ok(Json(json!(diff::diff(
        &version(&st, guild, from).await?,
        &version(&st, guild, to).await?
    ))))
}

#[derive(Deserialize)]
pub struct Rollback {
    expected_version: i32,
}

/// Rolling back re-applies an old snapshot as a new version, so history stays append-only.
pub async fn rollback(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, v)): Path<(i64, i32)>,
    Json(b): Json<Rollback>,
) -> Res {
    // Config may hold sensitive sections and permits, so a rollback needs the same bar as editing them.
    let level = require(&st, &s, guild, Level::Admin, true).await?;
    let old = version(&st, guild, v).await?;
    let mut tx = st.db.begin().await?;
    let cur = store::read_permits(&mut tx, guild).await?;
    if let Some(p) = old.get("permits") {
        if level < Level::Owner && p["ta"] != cur["ta"] {
            return Err(ApiError::Forbidden("only owners can roll back the TA list"));
        }
        if p["eo"] != cur["eo"] && !is_discord_owner(&st, &s, guild).await? {
            return Err(ApiError::Forbidden(
                "only the server owner can roll back the Extra Owners list",
            ));
        }
        store::write_permits(&mut tx, guild, p).await?;
    }
    let version = store::commit(&mut tx, guild, b.expected_version, &old, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        &old,
        format!(
            "{} (<@{}>) rolled config back to v{v} -> v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version})))
}

/// Sections the bot parses must deserialize into its types; the rest are stored as-is.
pub(crate) fn validate(section: &str, v: &Value) -> Result<(), ApiError> {
    use tommybot_core::sections::{
        AntinukeCfg, AppealsCfg, AutomodCfg, BackupsCfg, CatchCfg, LockdownCfg, MiscCfg, PanicCfg,
        TicketsCfg, WhitelistCfg,
    };
    if section == "logging" {
        for (i, c) in v["custom"].as_array().into_iter().flatten().enumerate() {
            let n = i + 1;
            if c["channel"].is_null() {
                return Err(ApiError::BadRequest(format!(
                    "logging type {n} needs a channel"
                )));
            }
            if c["events"].as_array().is_none_or(Vec::is_empty) {
                return Err(ApiError::BadRequest(format!(
                    "logging type {n} needs at least one event"
                )));
            }
        }
    }
    if section == "automod" {
        // Discord's own domains can never be blacklisted (it would block every invite).
        const OWN: [&str; 7] = [
            "discord.com",
            "discord.gg",
            "discordapp.com",
            "discord.media",
            "discordapp.net",
            "discord.new",
            "discord.gift",
        ];
        for d in v["filters"]["blacklisted_domains"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let d = d.as_str().unwrap_or("").trim().to_lowercase();
            if OWN.iter().any(|o| d == *o || d.ends_with(&format!(".{o}"))) {
                return Err(ApiError::BadRequest(format!("{d} cannot be blacklisted")));
            }
        }
    }
    let v = v.clone();
    let r = match section {
        "logging" => serde_json::from_value::<tommybot_logging::LogConfig>(v).map(drop),
        "antinuke" => serde_json::from_value::<AntinukeCfg>(v).map(drop),
        "automod" => serde_json::from_value::<AutomodCfg>(v).map(drop),
        "joingate" => serde_json::from_value::<tommybot_automod::join::GateCfg>(v).map(drop),
        "verification" => serde_json::from_value::<tommybot_automod::join::VerifyCfg>(v).map(drop),
        "joinraid" => serde_json::from_value::<tommybot_automod::join::RaidCfg>(v).map(drop),
        "backups" => serde_json::from_value::<BackupsCfg>(v).map(drop),
        "panic" => serde_json::from_value::<PanicCfg>(v).map(drop),
        "lockdown" => serde_json::from_value::<LockdownCfg>(v.clone())
            .and_then(|_| serde_json::from_value::<tommybot_automod::lockdown::LockdownPrefs>(v))
            .map(drop),
        "discord_automod" => {
            serde_json::from_value::<tommybot_automod::discord_api::DiscordAutomodCfg>(v).map(drop)
        }
        "misc" => serde_json::from_value::<MiscCfg>(v).map(drop),
        "appeals" => serde_json::from_value::<AppealsCfg>(v).map(drop),
        "tickets" => serde_json::from_value::<TicketsCfg>(v).map(drop),
        "whitelist" => serde_json::from_value::<WhitelistCfg>(v).map(drop),
        "channel_profiles" => serde_json::from_value::<crate::channels::ProfilesCfg>(v).map(drop),
        "catch" => serde_json::from_value::<CatchCfg>(v).map(drop),
        _ => Ok(()),
    };
    r.map_err(|e| ApiError::BadRequest(format!("invalid {section} config: {e}")))
}

#[cfg(test)]
mod logging_validate_tests {
    use super::validate;
    use serde_json::json;

    #[test]
    fn discord_domains_cannot_be_blacklisted() {
        let bad = |d| validate("automod", &json!({"filters": {"blacklisted_domains": [d]}}));
        assert!(bad("Discord.gg").is_err());
        assert!(bad("cdn.discordapp.com").is_err());
        assert!(bad("evil.example").is_ok());
    }

    #[test]
    fn custom_type_needs_channel_and_events() {
        let bad = |c| validate("logging", &json!({"custom": [c]})).is_err();
        assert!(bad(json!({"events": ["member_kick"], "channel": null})));
        assert!(bad(json!({"events": [], "channel": "5"})));
        assert!(!bad(json!({"events": ["member_kick"], "channel": "5"})));
    }
}
