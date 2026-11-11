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
use tommybot_core::limits;

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

pub(crate) async fn is_discord_owner(
    st: &AppState,
    s: &Sess,
    guild: i64,
) -> Result<bool, ApiError> {
    Ok(st.discord.guild_owner(guild).await? == s.user_id.to_string())
}

/// Re-resolved on every request, including writes (spec 3.1).
pub(crate) async fn level_of(st: &AppState, s: &Sess, guild: i64) -> Result<Level, ApiError> {
    let flag = |table: &'static str| async move {
        sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(format!(
            "select exists(select 1 from {table} where guild_id = $1 and user_id = $2)"
        )))
        .bind(guild)
        .bind(s.user_id)
        .fetch_one(&st.db)
        .await
    };
    let discord_owner = is_discord_owner(st, s, guild).await?;
    let (mut extra_owner, mut trusted_admin) =
        (flag("extra_owners").await?, flag("trusted_admins").await?);
    // A banned or departed EO/TA keeps the table row, so the list alone never grants access.
    if !discord_owner
        && (extra_owner || trusted_admin)
        && st
            .discord
            .member(guild, &s.user_id.to_string())
            .await?
            .is_none()
    {
        (extra_owner, trusted_admin) = (false, false);
    }
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

/// The sample messages the bot saved at startup, for the previews; empty before its first start.
pub async fn previews(State(st): State<AppState>, _: Sess) -> Res {
    let m: Option<Value> = sqlx::query_scalar("select messages from bot_previews")
        .fetch_optional(&st.db)
        .await?;
    Ok(Json(m.unwrap_or_else(|| json!({}))))
}

/// The command list the bot saved with the previews, for the public Commands page (no login).
pub async fn commands(State(st): State<AppState>) -> Res {
    let m = sqlx::query_scalar::<_, Option<Value>>("select messages->'commands' from bot_previews")
        .fetch_optional(&st.db)
        .await?
        .flatten();
    Ok(Json(m.unwrap_or_else(|| json!({}))))
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

/// The user's access per dashboard page, fetched again on every page change so permit edits show at once.
pub async fn page_access(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let level = level_of(&st, &s, guild).await?;
    let perms = if level >= Level::Admin {
        vec![]
    } else {
        permit_perms(&st, &s, guild).await?
    };
    let mut pages: serde_json::Map<String, Value> = access::PAGES
        .iter()
        .map(|p| (p.to_string(), json!(access::page(p, level, &perms))))
        .collect();
    if level == Level::Owner && !is_discord_owner(&st, &s, guild).await? {
        pages.insert("rescue".into(), json!(access::PageAccess::Hidden));
        pages.insert("ban_sync".into(), json!(access::PageAccess::Read));
    }
    let cmds = sqlx::query_scalar::<_, Option<Value>>(
        "select messages->'commands'->'commands' from bot_previews",
    )
    .fetch_optional(&st.db)
    .await?
    .flatten()
    .unwrap_or_default();
    let denied: Vec<&str> = cmds
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| !access::command(c["perm"].as_str().unwrap_or("owner"), level, &perms))
        .filter_map(|c| c["name"].as_str())
        .collect();
    Ok(Json(
        json!({"guild": guild.to_string(), "level": level, "pages": pages, "commands_denied": denied}),
    ))
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

pub(crate) async fn log_change(st: &AppState, guild: i64, config: &Value, text: String) {
    if let Some(ch) = config["logging"]["config_changes"]
        .as_str()
        .map(String::from)
    {
        let d = st.discord.clone();
        tokio::spawn(async move {
            match d.private_channel_of(guild, &ch).await {
                Ok(true) => {}
                Ok(false) => {
                    return tracing::warn!(
                        guild,
                        ch,
                        "config-changes log skipped: channel is public or in another server"
                    )
                }
                Err(e) => return tracing::warn!("config-changes log check failed: {e:#}"),
            }
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

/// The first id `new` adds that is not one of the guild's channels or roles.
fn foreign_id(
    section: &str,
    new: &Value,
    old: &Value,
    channels: &Value,
    roles: &Value,
) -> Option<String> {
    let (old_ch, old_ro) = crate::schema::ids(section, old);
    let (ch, ro) = crate::schema::ids(section, new);
    let known = |list: &Value, id: &String| {
        list.as_array()
            .into_iter()
            .flatten()
            .any(|x| x["id"].as_str() == Some(id))
    };
    // Ids already stored are left alone, so a deleted channel never blocks saving the rest.
    ch.into_iter()
        .filter(|id| !old_ch.contains(id))
        .find(|id| !known(channels, id))
        .or_else(|| {
            ro.into_iter()
                .filter(|id| !old_ro.contains(id))
                .find(|id| !known(roles, id))
        })
}

/// The bot is in many servers: a section may only point at this guild's channels and roles.
async fn check_ids(
    st: &AppState,
    guild: i64,
    section: &str,
    new: &Value,
    old: &Value,
) -> Result<(), ApiError> {
    if foreign_id(section, new, old, &Value::Null, &Value::Null).is_none() {
        return Ok(());
    }
    let get = |p| async move {
        st.discord
            .call(reqwest::Method::GET, &format!("/guilds/{guild}/{p}"), None)
            .await
    };
    let (channels, roles) = (get("channels").await?, get("roles").await?);
    match foreign_id(section, new, old, &channels, &roles) {
        Some(id) => Err(ApiError::BadRequest(format!(
            "{id} is not a channel or role of this server"
        ))),
        None => Ok(()),
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
    // Access first, so someone without it learns nothing about the section's shape.
    let (min, step_up) = access::write_bar(&section);
    require(&st, &s, guild, min, step_up).await?;
    let owner_only = access::DISCORD_OWNER_ONLY.contains(&section.as_str());
    discord_owner_for(&st, &s, guild, owner_only).await?;
    if !b.value.is_object() {
        return Err(ApiError::BadRequest("section must be an object".into()));
    }
    validate(&section, &b.value)?;
    let (cur, mut config) = store::current(&st.db, guild).await?;
    check_ids(&st, guild, &section, &b.value, &config[&section]).await?;
    limits::check(&section, &b.value, &config[&section]).map_err(ApiError::BadRequest)?;
    // Turning Tickets on is the whole setup: its category and log channel are made here.
    let tickets_on = section == "tickets"
        && b.value["enabled"] == true
        && (b.value["category"].as_str().is_none_or(str::is_empty)
            || b.value["log_channel"].as_str().is_none_or(str::is_empty));
    config[&section] = b.value;
    if tickets_on {
        if cur != b.expected_version {
            return Err(ApiError::Conflict(cur)); // before any Discord side effect
        }
        crate::sections::ticket_channels(&st, guild, &mut config).await?;
    }
    let web = matches!(section.as_str(), "verification" | "lockdown");
    if let Some(e) = tommybot_automod::join::web_mode_error(&config).filter(|_| web) {
        return Err(ApiError::BadRequest(e.into()));
    }
    let mut tx = st.db.begin().await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        guild,
        &config,
        format!(
            "{} (<@{}>) changed `{section}` -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version})))
}

/// Whether `new` and `now` differ in a section only the Discord owner may change.
fn owner_only_changed(new: &Value, now: &Value) -> bool {
    access::DISCORD_OWNER_ONLY
        .iter()
        .any(|k| new.get(*k) != now.get(*k))
}

/// With `needed`, refuses anyone but the Discord owner (extra owners included).
async fn discord_owner_for(
    st: &AppState,
    s: &Sess,
    guild: i64,
    needed: bool,
) -> Result<(), ApiError> {
    if needed && !is_discord_owner(st, s, guild).await? {
        return Err(ApiError::Forbidden(
            "only the server owner can change Ban Sync",
        ));
    }
    Ok(())
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
    keep_exemptions(&mut tx, guild, level, &old).await?;
    let (_, mut config) = store::current(&st.db, guild).await?;
    config["permits"] = store::read_permits(&mut tx, guild).await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        guild,
        &config,
        format!(
            "{} (<@{}>) changed `permits` -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version})))
}

/// Permits whose whitelist exempts their roles from anti-nuke, as (name, roles, flags).
fn antinuke_exempt(p: &Value) -> Vec<Value> {
    fn sorted<'a>(v: &'a Value, keep: &dyn Fn(&str) -> bool) -> Vec<&'a str> {
        let mut v: Vec<&str> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| keep(s))
            .collect();
        v.sort();
        v
    }
    let mut out: Vec<Value> = p["custom"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let flags = sorted(&c["whitelist"], &|f| {
                f == "all" || f == "antinuke" || f.starts_with("wl_")
            });
            (!flags.is_empty()).then(|| json!([c["name"], sorted(&c["roles"], &|_| true), flags]))
        })
        .collect();
    out.sort_by_key(|v| v.to_string());
    out
}

/// Anti-nuke is Owner-only (write_bar), so below Owner the permits that exempt roles from it must stay
/// as they were: no new flag, and no role added to or removed from such a permit.
async fn keep_exemptions(
    tx: &mut sqlx::PgConnection,
    guild: i64,
    level: Level,
    before: &Value,
) -> Result<(), ApiError> {
    if level >= Level::Owner {
        return Ok(());
    }
    let after = store::read_permits(tx, guild).await?;
    if antinuke_exempt(before) != antinuke_exempt(&after) {
        return Err(ApiError::Forbidden(
            "only owners can change anti-nuke whitelist flags or the roles of permits that have them",
        ));
    }
    Ok(())
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

pub(crate) async fn version(st: &AppState, guild: i64, v: i32) -> Result<Value, ApiError> {
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
    let (_, now) = store::current(&st.db, guild).await?;
    if level < rollback_bar(&old, &now) {
        return Err(ApiError::Forbidden(
            "this rollback changes a section only owners can edit",
        ));
    }
    discord_owner_for(&st, &s, guild, owner_only_changed(&old, &now)).await?;
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
        keep_exemptions(&mut tx, guild, level, &cur).await?;
    }
    let version = store::commit(&mut tx, guild, b.expected_version, &old, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        guild,
        &old,
        format!(
            "{} (<@{}>) rolled config back to v{v} -> v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version})))
}

/// Deletes everything stored for the server (PRIVACY.md); the bot keeps running on defaults.
pub async fn purge(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, true).await?;
    if !is_discord_owner(&st, &s, guild).await? {
        return Err(ApiError::Forbidden(
            "only the server owner can delete the server's data",
        ));
    }
    tommybot_core::db::purge_guild(&st.db, guild).await?;
    tracing::info!(
        guild,
        user = s.user_id,
        "server data purged from the dashboard"
    );
    Ok(Json(json!({})))
}

/// The highest level any section a rollback from `now` to `old` changes needs (permits are checked apart).
fn rollback_bar(old: &Value, now: &Value) -> Level {
    let keys = |v: &Value| {
        v.as_object()
            .into_iter()
            .flat_map(|o| o.keys().cloned())
            .collect::<Vec<_>>()
    };
    keys(old)
        .into_iter()
        .chain(keys(now))
        .filter(|k| k != "permits" && old.get(k) != now.get(k))
        .map(|k| access::write_bar(&k).0)
        .max()
        .unwrap_or(Level::None)
}

/// A Discord snowflake: channel, role, user, webhook and message ids are 17-20 digits.
fn is_id(v: &Value) -> bool {
    match v {
        Value::String(s) => (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()),
        Value::Number(n) => n.as_u64().is_some_and(|n| n >= 10u64.pow(16)),
        _ => false,
    }
}

/// Drops every id (as a value, list item or map key) and records where, as `section.field`.
fn strip_ids(v: &mut Value, path: &str, cleared: &mut Vec<String>) {
    match v {
        Value::Object(o) => o.retain(|k, x| {
            let p = format!("{path}.{k}");
            if is_id(&Value::String(k.clone())) || is_id(x) {
                cleared.push(p);
                return false;
            }
            strip_ids(x, &p, cleared);
            true
        }),
        Value::Array(a) => {
            let n = a.len();
            a.retain(|x| !is_id(x));
            if a.len() < n {
                cleared.push(path.into());
            }
            for (i, x) in a.iter_mut().enumerate() {
                strip_ids(x, &format!("{path}.{i}"), cleared);
            }
        }
        _ => {}
    }
}

/// `now` with the sections of an exported config copied over, minus the ids of the server it came
/// from (they do not exist here). EO/TA stay as they are; custom permits come over without roles.
fn import_config(file: &Value, now: &Value) -> Result<(Value, Vec<String>), ApiError> {
    let file = file
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("this is not a TommyBot settings export".into()))?;
    let (mut new, mut cleared) = (now.clone(), vec![]);
    for (k, v) in file {
        // Removed with the old Channel Setup; exports made before still carry it.
        if k == "channel_profiles" {
            continue;
        }
        if k != "permits" && !access::SECTIONS.contains(&k.as_str()) {
            return Err(ApiError::BadRequest(format!("unknown section `{k}`")));
        }
        if !v.is_object() {
            return Err(ApiError::BadRequest(format!("{k} must be an object")));
        }
        let mut v = v.clone();
        strip_ids(&mut v, k, &mut cleared);
        // An import never switches a cross-server feature on.
        if k == "ban_sync" {
            v["enabled"] = json!(false);
        }
        if k == "permits" {
            v["eo"] = now["permits"]["eo"].clone();
            v["ta"] = now["permits"]["ta"].clone();
            cleared.retain(|p| !p.starts_with("permits.eo") && !p.starts_with("permits.ta"));
        } else {
            // A custom log type cannot exist without its channel.
            let custom = v.get_mut("custom").and_then(Value::as_array_mut);
            if let Some(custom) = custom.filter(|_| k == "logging") {
                let n = custom.len();
                custom.retain(|c| !c["channel"].is_null());
                if custom.len() < n {
                    cleared.retain(|p| !p.starts_with("logging.custom."));
                    cleared.push("logging.custom (log types without a channel removed)".into());
                }
            }
            validate(k, &v)?;
            limits::check(k, &v, &now[k]).map_err(ApiError::BadRequest)?;
        }
        new[k] = v;
    }
    Ok((new, cleared))
}

/// Level and step-up an import needs: the bar of every section it changes, Admin + step-up for permits.
fn import_bar(new: &Value, now: &Value) -> (Level, bool) {
    let permits = new.get("permits") != now.get("permits");
    let step_up = permits || access::SENSITIVE.iter().any(|k| new.get(*k) != now.get(*k));
    let floor = if permits { Level::Admin } else { Level::Editor };
    (rollback_bar(new, now).max(floor), step_up)
}

#[derive(Deserialize)]
pub struct Import {
    expected_version: i32,
    config: Value,
    #[serde(default)]
    apply: bool,
}

/// Copies another server's exported settings here; without `apply` it only previews the diff.
pub async fn import(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Import>,
) -> Res {
    // The floor first, so a file is never checked against a server the user cannot edit.
    require(&st, &s, guild, Level::Editor, false).await?;
    let (_, mut now) = store::current(&st.db, guild).await?;
    let mut tx = st.db.begin().await?;
    now["permits"] = store::read_permits(&mut tx, guild).await?;
    let (new, cleared) = import_config(&b.config, &now)?;
    let (min, step_up) = import_bar(&new, &now);
    let level = require(&st, &s, guild, min, step_up).await?;
    discord_owner_for(&st, &s, guild, owner_only_changed(&new, &now)).await?;
    let web = ["verification", "lockdown"]
        .iter()
        .any(|k| new.get(*k) != now.get(*k));
    if let Some(e) = tommybot_automod::join::web_mode_error(&new).filter(|_| web) {
        return Err(ApiError::BadRequest(e.into()));
    }
    if !b.apply {
        return Ok(Json(
            json!({"diff": diff::diff(&now, &new), "cleared": cleared}),
        ));
    }
    if new["permits"] != now["permits"] {
        store::write_permits(&mut tx, guild, &new["permits"]).await?;
        keep_exemptions(&mut tx, guild, level, &now["permits"]).await?;
    }
    let version = store::commit(&mut tx, guild, b.expected_version, &new, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        guild,
        &new,
        format!(
            "{} (<@{}>) imported settings from another server -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version, "cleared": cleared})))
}

/// Sections the bot parses must deserialize into its types; the rest are stored as-is.
pub(crate) fn validate(section: &str, v: &Value) -> Result<(), ApiError> {
    use tommybot_core::sections::{
        AntinukeCfg, AppealsCfg, AutomodCfg, BackupsCfg, BanSyncCfg, CatchCfg, LockdownCfg,
        MiscCfg, PanicCfg, TicketsCfg, WhitelistCfg,
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
        // A wrongly typed heat block is reported by the AutomodCfg parse below.
        let heat = serde_json::from_value(v["heat"].clone()).unwrap_or_default();
        heat_ok(&heat).map_err(|e| ApiError::BadRequest(e.into()))?;
    }
    if section == "tickets" {
        let mut seen = std::collections::HashSet::new();
        for sn in v["snippets"].as_array().into_iter().flatten() {
            let (n, c) = (
                sn["name"].as_str().unwrap_or(""),
                sn["content"].as_str().unwrap_or(""),
            );
            let n_ok = (1..=100).contains(&n.chars().count()) && !n.contains(char::is_whitespace);
            if !n_ok || !(1..=1000).contains(&c.chars().count()) || !seen.insert(n) {
                return Err(ApiError::BadRequest(format!(
                    "snippet \"{n}\" needs a unique name of 1-100 characters without spaces and 1-1000 characters of content"
                )));
            }
        }
    }
    if section == "misc" && v["warn_actions"].as_array().is_some_and(|a| a.len() > 5) {
        return Err(ApiError::BadRequest("at most 5 warn actions".into()));
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
        "catch" => serde_json::from_value::<CatchCfg>(v).map(drop),
        "ban_sync" => serde_json::from_value::<BanSyncCfg>(v).map(drop),
        _ => Ok(()),
    };
    r.map_err(|e| ApiError::BadRequest(format!("invalid {section} config: {e}")))
}

/// Bounds that keep the heat model meaningful: a max to reach, decay below 100%/s, no negative weights.
fn heat_ok(h: &tommybot_automod::heat::HeatCfg) -> Result<(), &'static str> {
    if h.max <= 0.0 {
        return Err("heat max must be above 0");
    }
    if !(0.0..1.0).contains(&h.decay_per_sec) {
        return Err("heat decay must be at least 0 and below 1");
    }
    let weights = serde_json::to_value(h).unwrap_or_default();
    if weights
        .as_object()
        .into_iter()
        .flatten()
        .any(|(_, w)| w.as_f64().is_some_and(|w| w < 0.0))
    {
        return Err("heat weights cannot be negative");
    }
    Ok(())
}

#[cfg(test)]
mod logging_validate_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn imports_refuse_new_numbers_outside_their_range() {
        let nuke = |n: i64| json!({"antinuke": {"limits": {"Ban": {"heat": n}}}});
        assert!(import_config(&nuke(100), &json!({})).is_ok());
        assert!(import_config(&nuke(0), &json!({})).is_err());
        // An old 0 already saved here may come back unchanged.
        assert!(import_config(&nuke(0), &nuke(0)).is_ok());
        let age = json!({"joingate": {"min_age_override_ms": 4_999}});
        assert!(import_config(&age, &json!({})).is_err());
        // Exports from before the Channel Setup rebuild still import; the old section is dropped.
        let old = json!({"channel_profiles": {"profiles": []}, "automod": {}});
        let (new, _) = import_config(&old, &json!({})).unwrap();
        assert!(new.get("channel_profiles").is_none());
    }

    #[test]
    fn import_never_turns_ban_sync_on() {
        let file = json!({"ban_sync": {"enabled": true, "receive": "ban"}});
        let (new, _) = import_config(&file, &json!({})).unwrap();
        assert_eq!(new["ban_sync"], json!({"enabled": false, "receive": "ban"}));
        assert!(import_config(&json!({"ban_sync": {"receive": "nuke"}}), &json!({})).is_err());
        assert_eq!(import_bar(&new, &json!({})), (Level::Owner, true));
        assert!(owner_only_changed(&new, &json!({})));
    }

    #[test]
    fn rollback_needs_the_bar_of_every_changed_section() {
        let now = json!({"automod": {"enabled": true}, "antinuke": {"enabled": true}, "permits": {"ta": []}});
        let mut old = now.clone();
        assert_eq!(rollback_bar(&old, &now), Level::None);
        old["permits"] = json!({"ta": ["1"]});
        assert_eq!(
            rollback_bar(&old, &now),
            Level::None,
            "permits are checked apart"
        );
        old["automod"]["enabled"] = json!(false);
        assert_eq!(rollback_bar(&old, &now), Level::Editor);
        old.as_object_mut().unwrap().remove("antinuke");
        assert_eq!(
            rollback_bar(&old, &now),
            Level::Owner,
            "a dropped section counts"
        );
    }

    #[test]
    fn import_keeps_settings_and_clears_server_ids() {
        let (ch, ro, us) = (
            "123456789012345678",
            "223456789012345678",
            "323456789012345678",
        );
        let file = json!({
            "automod": {"enabled": true, "filters": {"blacklisted_domains": ["evil.example"]}},
            "logging": {"fallback": ch, "events": {"member_kick": ch}, "ignored_channels": [ch],
                "custom": [{"events": ["member_kick"], "channel": ch}]},
            "whitelist": {"users": [us], "roles": [ro], "webhooks": [us]},
            "tickets": {"enabled": true, "greeting": "Hi", "ping_roles": [ro], "log_channel": ch},
            "permits": {"eo": [us], "ta": [us], "custom": [{"name": "Mod", "rank": 2, "perms": ["ban"], "roles": [ro]}]},
        });
        let now = json!({"tickets": {"enabled": false}, "permits": {"eo": ["5"], "ta": [], "custom": []}});
        let (new, cleared) = import_config(&file, &now).unwrap();
        assert_eq!(new["automod"], file["automod"]);
        assert_eq!(
            new["logging"],
            json!({"events": {}, "ignored_channels": [], "custom": []})
        );
        assert_eq!(
            new["whitelist"],
            json!({"users": [], "roles": [], "webhooks": []})
        );
        assert_eq!(
            new["tickets"],
            json!({"enabled": true, "greeting": "Hi", "ping_roles": []})
        );
        assert_eq!(
            new["permits"],
            json!({"eo": ["5"], "ta": [], "custom": [{"name": "Mod", "rank": 2, "perms": ["ban"], "roles": []}]}),
            "EO/TA stay, permit roles are cleared"
        );
        for p in [
            "logging.fallback",
            "logging.events.member_kick",
            "logging.custom (log types without a channel removed)",
            "whitelist.users",
            "tickets.log_channel",
            "permits.custom.0.roles",
        ] {
            assert!(cleared.contains(&p.to_string()), "{p} in {cleared:?}");
        }
        assert!(!cleared.iter().any(|p| p.starts_with("permits.eo")));
        // Importing the result again changes nothing and clears nothing.
        let (again, cleared) = import_config(&new, &new).unwrap();
        assert_eq!((again, cleared.len()), (new, 0));
    }

    #[test]
    fn antinuke_exemptions_ignore_other_flags_and_order() {
        let a = json!({"custom": [{"name": "Mods", "roles": ["2", "1"], "whitelist": ["automod", "wl_ban"]}]});
        let b = json!({"custom": [{"name": "Mods", "roles": ["1", "2"], "whitelist": ["wl_ban"]}, {"name": "X", "roles": ["3"], "whitelist": ["words"]}]});
        assert_eq!(antinuke_exempt(&a), antinuke_exempt(&b));
        let c = json!({"custom": [{"name": "Mods", "roles": ["1", "2", "9"], "whitelist": ["wl_ban"]}]});
        assert_ne!(antinuke_exempt(&a), antinuke_exempt(&c));
        let d = json!({"custom": [{"name": "Mods", "roles": ["1", "2"], "whitelist": ["wl_ban", "all"]}]});
        assert_ne!(antinuke_exempt(&a), antinuke_exempt(&d));
    }

    #[test]
    fn import_needs_the_bar_of_every_changed_section() {
        let now = json!({"permits": {"eo": [], "ta": [], "custom": []}});
        let bar = |file| {
            let (new, _) = import_config(&file, &now).unwrap();
            import_bar(&new, &now)
        };
        assert_eq!(
            bar(json!({"automod": {"enabled": true}})),
            (Level::Editor, false)
        );
        assert_eq!(
            bar(json!({"whitelist": {"users": []}})),
            (Level::Admin, true)
        );
        assert_eq!(
            bar(json!({"antinuke": {"enabled": true}})),
            (Level::Owner, true)
        );
        assert_eq!(
            bar(json!({"permits": {"custom": [{"name": "Mod", "perms": []}]}})),
            (Level::Admin, true)
        );
        // An editor is refused anything above their level.
        assert!(Level::Editor < bar(json!({"misc": {}})).0);
    }

    #[test]
    fn malformed_imports_are_refused() {
        let now = json!({});
        for file in [
            json!(null),
            json!("text"),
            json!([1]),
            json!({"nope": {}}),
            json!({"automod": 5}),
            json!({"automod": {"heat": {"max": 0}}}),
            json!({"tickets": {"enabled": "yes"}}),
        ] {
            assert!(
                matches!(import_config(&file, &now), Err(ApiError::BadRequest(_))),
                "{file}"
            );
        }
    }

    #[test]
    fn foreign_ids_are_refused_unless_already_stored() {
        let (channels, roles) = (json!([{"id": "1"}]), json!([{"id": "5"}]));
        let check = |new, old| foreign_id("logging", &new, &old, &channels, &roles);
        assert_eq!(check(json!({"fallback": "1"}), json!({})), None);
        assert_eq!(check(json!({"fallback": "2"}), json!({})), Some("2".into()));
        assert_eq!(
            check(json!({"fallback": "2"}), json!({"fallback": "2"})),
            None
        );
        assert_eq!(
            check(json!({"fallback": "5"}), json!({})),
            Some("5".into()),
            "a role is not a channel"
        );
    }

    #[test]
    fn heat_bounds() {
        let heat = |h| validate("automod", &json!({"heat": h}));
        assert!(heat(json!({})).is_ok());
        assert!(heat(json!({"max": 0})).is_err());
        assert!(heat(json!({"decay_per_sec": 1})).is_err());
        assert!(heat(json!({"decay_per_sec": -0.1})).is_err());
        assert!(heat(json!({"per_emoji": -1})).is_err());
    }

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
