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
    let discord_owner = st.discord.guild_owner(guild).await? == s.user_id.to_string();
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
            if let Err(e) = d.post_message(&ch, &text).await {
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
    let sensitive = access::SENSITIVE.contains(&section.as_str());
    require(
        &st,
        &s,
        guild,
        if sensitive {
            Level::Admin
        } else {
            Level::Editor
        },
        sensitive,
    )
    .await?;
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
    // Only owners may change who owns or administers the bot.
    if level < Level::Owner && (old["eo"] != b.value["eo"] || old["ta"] != b.value["ta"]) {
        return Err(ApiError::Forbidden(
            "only owners can change the EO and TA lists",
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
        if level < Level::Owner && (p["eo"] != cur["eo"] || p["ta"] != cur["ta"]) {
            return Err(ApiError::Forbidden(
                "only owners can roll back the EO and TA lists",
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
        AntinukeCfg, AppealsCfg, AutomodCfg, LockdownCfg, MiscCfg, PanicCfg, WhitelistCfg,
    };
    let v = v.clone();
    let r = match section {
        "logging" => serde_json::from_value::<tommybot_logging::LogConfig>(v).map(drop),
        "antinuke" => serde_json::from_value::<AntinukeCfg>(v).map(drop),
        "automod" => serde_json::from_value::<AutomodCfg>(v).map(drop),
        "joingate" => serde_json::from_value::<tommybot_automod::join::GateCfg>(v).map(drop),
        "verification" => serde_json::from_value::<tommybot_automod::join::VerifyCfg>(v).map(drop),
        "joinraid" => serde_json::from_value::<tommybot_automod::join::RaidCfg>(v).map(drop),
        "panic" => serde_json::from_value::<PanicCfg>(v).map(drop),
        "lockdown" => serde_json::from_value::<LockdownCfg>(v).map(drop),
        "misc" => serde_json::from_value::<MiscCfg>(v).map(drop),
        "appeals" => serde_json::from_value::<AppealsCfg>(v).map(drop),
        "whitelist" => serde_json::from_value::<WhitelistCfg>(v).map(drop),
        _ => Ok(()),
    };
    r.map_err(|e| ApiError::BadRequest(format!("invalid {section} config: {e}")))
}
