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

/// Permissions of the user's permit: the highest-ranked one, as the bot reads it (D72).
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
    let top = tommybot_core::permits::top_permit(&st.db, guild, &roles).await?;
    Ok(top.map(|p| p.1).unwrap_or_default())
}

/// The Discord owner, checked fresh against Discord and against what the bot recorded: owner-only
/// writes (Extra Owners, Keep, purge) need the bot to have processed an ownership
/// change, so it has paused the previous owner's staff first (D47).
pub(crate) async fn is_discord_owner(
    st: &AppState,
    s: &Sess,
    guild: i64,
) -> Result<bool, ApiError> {
    Ok(owner_state(st, s, guild, true).await? == Some(true))
}

/// None: not the owner. Some(true): the owner, and the bot's `guild_owners` agrees. Some(false):
/// Discord says this user is the owner, but the bot has not processed the change yet (a failed
/// `on_owner`); a retry is queued. Only the memoized owner is checked fresh, so other users cost
/// no Discord call.
async fn owner_state(
    st: &AppState,
    s: &Sess,
    guild: i64,
    always_fresh: bool,
) -> Result<Option<bool>, ApiError> {
    if st.discord.guild_owner(guild).await? != s.user_id.to_string() {
        return Ok(None);
    }
    // The memo is 30 s old at most, but the bot moves guild_owners and records the handover within
    // a second, so a stale memo never keeps the previous owner at Owner level.
    let (other_owner, was_owner): (bool, bool) = sqlx::query_as(
        "select exists(select 1 from guild_owners where guild_id = $1 and owner_id <> $2),
                exists(select 1 from owner_handovers where guild_id = $1 and old_owner = $2)",
    )
    .bind(guild)
    .bind(s.user_id)
    .fetch_one(&st.db)
    .await?;
    if was_owner {
        return Ok(None);
    }
    if !other_owner && !always_fresh {
        return Ok(Some(true));
    }
    // Fail closed: Discord must confirm it now, and a lookup error is "not the owner".
    let fresh = st.discord.guild_owner_fresh(guild).await.ok();
    if fresh.as_deref() != Some(s.user_id.to_string().as_str()) {
        return Ok(None);
    }
    if !other_owner {
        return Ok(Some(true));
    }
    // At most once an hour per server: a job dropped after its retries failed would otherwise come
    // back on every request and repeat its failure log (R-h3 round 3, 3). The bot's own guild
    // events still queue it at once.
    // shortcut: per process, so a dashboard restart may queue once more; fine for one instance.
    static QUEUED: std::sync::Mutex<std::collections::BTreeMap<i64, std::time::Instant>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    let due = {
        let mut q = QUEUED.lock().unwrap();
        let due = q
            .get(&guild)
            .is_none_or(|at| at.elapsed() > std::time::Duration::from_secs(3600));
        if due {
            q.insert(guild, std::time::Instant::now());
        }
        due
    };
    if due {
        sqlx::query(
            "insert into scheduled_jobs (guild_id, kind, payload, run_at)
             select $1, 'owner_check', '{}', now()
             where not exists (select 1 from scheduled_jobs where guild_id = $1 and kind = 'owner_check')",
        )
        .bind(guild)
        .execute(&st.db)
        .await?;
    }
    Ok(Some(false))
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
    // The real owner keeps Owner level while the bot catches up on a change (R-h3 round 2, 3);
    // the owner-only writes still wait for it.
    let discord_owner = owner_state(st, s, guild, false).await?.is_some();
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
    // Trusted admins too: View Mod Stats is the one permit their level does not imply.
    let perms = if level == Level::Owner {
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
        json!({"version": version, "config": config, "level": level, "sensitive": access::SENSITIVE, "unread": unread_sections(&config)}),
    ))
}

#[derive(Deserialize)]
pub struct Put {
    expected_version: i32,
    value: Value,
}

/// Where `log_change` posts: the config's channel, and with `version` also that of the version before.
async fn change_log_channels(
    db: &sqlx::PgPool,
    guild: i64,
    config: &Value,
    version: Option<i32>,
) -> Vec<String> {
    let mut before = None;
    if let Some(v) = version {
        before = sqlx::query_scalar(
            "select config->'logging'->>'config_changes' from guild_config where guild_id = $1 and version = $2",
        )
        .bind(guild)
        .bind(v - 1)
        .fetch_optional(db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("config-changes log channel of v{} not read: {e:#}", v - 1);
            None
        })
        .flatten();
    }
    let mut channels: Vec<String> = config["logging"]["config_changes"]
        .as_str()
        .map(String::from)
        .into_iter()
        .chain(before)
        .filter(|c| !c.is_empty())
        .collect();
    channels.dedup();
    channels
}

/// Posts to the config-changes log. `version` is the config version this change saved, if any: then the
/// channel of the version before gets it too, so a save that clears or moves the log is still seen there.
pub(crate) async fn log_change(
    st: &AppState,
    guild: i64,
    config: &Value,
    version: Option<i32>,
    text: String,
) {
    for ch in change_log_channels(&st.db, guild, config, version).await {
        let text = text.clone();
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

/// The quarantine role and main roles the section adds (`roles::unfit`, the bot's rule too).
/// Ids already stored are left alone, as in [`check_ids`].
async fn check_held_roles(
    st: &AppState,
    guild: i64,
    section: &str,
    new: &Value,
    old: &Value,
) -> Result<(), ApiError> {
    let added = held_roles(section, new)
        .into_iter()
        .filter(|id| !held_roles(section, old).contains(id))
        .collect::<Vec<_>>();
    if added.is_empty() {
        return Ok(());
    }
    let (roles, bot_pos) = crate::ops::roles_and_bot_pos(st, guild).await?;
    match roles
        .iter()
        .filter(|r| added.contains(&r.id))
        .find_map(|r| Some((&r.id, r.unfit(guild, bot_pos)?)))
    {
        Some((id, why)) => Err(ApiError::BadRequest(format!(
            "<@&{id}> cannot be used here: {why}"
        ))),
        None => Ok(()),
    }
}

/// `antinuke.quarantine_role` and `lockdown.main_roles`.
fn held_roles(section: &str, v: &Value) -> Vec<String> {
    let ids = match section {
        "antinuke" => std::slice::from_ref(&v["quarantine_role"]),
        "lockdown" => v["main_roles"].as_array().map_or(&[][..], Vec::as_slice),
        _ => &[],
    };
    ids.iter()
        .filter_map(|i| i.as_str())
        .map(String::from)
        .collect()
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
    if !b.value.is_object() {
        return Err(ApiError::BadRequest("section must be an object".into()));
    }
    let (cur, mut config) = store::current(&st.db, guild).await?;
    validate(&section, &b.value, &config[&section])?;
    if section == "lockdown" && main_roles_guarded(&config, &b.value, &config["lockdown"]) {
        require(&st, &s, guild, Level::Owner, true).await?;
    }
    check_ids(&st, guild, &section, &b.value, &config[&section]).await?;
    check_held_roles(&st, guild, &section, &b.value, &config[&section]).await?;
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
        Some(version),
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
        // While the bot has not processed an ownership change yet, the real owner may still
        // remove the previous owner's Extra Owners, never add one (S-h3 round 3).
        let had = old["eo"].as_array().cloned().unwrap_or_default();
        let only_removes = b.value["eo"]
            .as_array()
            .is_some_and(|n| n.iter().all(|u| had.contains(u)));
        if !(only_removes && owner_state(&st, &s, guild, true).await? == Some(false)) {
            return Err(ApiError::Forbidden(
                "only the server owner can change the Extra Owners list",
            ));
        }
    }
    store::write_permits(&mut tx, guild, &b.value).await?;
    keep_exemptions(&mut tx, guild, level, &old).await?;
    let (_, config) = store::current(&st.db, guild).await?;
    let version = store::commit(&mut tx, guild, b.expected_version, &config, s.user_id).await?;
    tx.commit().await?;
    log_change(
        &st,
        guild,
        &config,
        Some(version),
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
            // View Mod Stats shows what colleagues did, so trusted admins may not grant it either.
            let stats = sorted(&c["perms"], &|p| p == "mod_stats");
            (!flags.is_empty() || !stats.is_empty())
                .then(|| json!([c["name"], sorted(&c["roles"], &|_| true), flags, stats]))
        })
        .collect();
    out.sort_by_key(|v| v.to_string());
    out
}

/// Anti-nuke is Owner-only (write_bar), so below Owner the permits that exempt roles from it, or that
/// hold View Mod Stats, must stay as they were: no new flag, and no role added to or removed from them.
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
            "only owners can change anti-nuke whitelist flags, View Mod Stats, or the roles of permits that have them",
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
    let mut new = version(&st, guild, to).await?;
    // "Diff vs current" shows what a rollback changes, and a rollback compares with the live permits.
    if to == store::current(&st.db, guild).await?.0 {
        new["permits"] = store::read_permits(&mut *st.db.acquire().await?, guild).await?;
    }
    Ok(Json(json!(diff::diff(
        &version(&st, guild, from).await?,
        &new
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
    let mut old = version(&st, guild, v).await?;
    // Ban Sync is removed: an old version does not bring its section back.
    if let Some(m) = old.as_object_mut() {
        m.remove("ban_sync");
    }
    let (_, now) = store::current(&st.db, guild).await?;
    // A version from before the last ownership change would bring back what the change paused (D47).
    let before_handover: bool = sqlx::query_scalar(
        "select exists(select 1 from owner_handovers h join guild_config c on c.guild_id = h.guild_id
           where h.guild_id = $1 and c.version = $2 and c.created_at < h.at)",
    )
    .bind(guild)
    .bind(v)
    .fetch_one(&st.db)
    .await?;
    let mut held = vec![];
    // The quarantine whitelist is paused like EO/TA and comes back only through Keep.
    let qw = |c: &Value| match &c["antinuke"]["quarantine_whitelist"] {
        Value::Null => json!([]),
        v => v.clone(),
    };
    if before_handover && old["antinuke"].is_object() && qw(&old) != qw(&now) {
        old["antinuke"]["quarantine_whitelist"] = qw(&now);
        held.push("the quarantine whitelist stays as it is".to_string());
    }
    if level < rollback_bar(&old, &now) {
        return Err(ApiError::Forbidden(
            "this rollback changes a section only owners can edit",
        ));
    }
    // The same checks as a save: an old version may hold a value the bot would now read as off.
    // A number out of range is pulled into it and reported, so a cleanup is not blocked (D69b).
    let mut clamped = vec![];
    for k in access::SECTIONS {
        if let Some(v) = old.get_mut(*k).filter(|v| Some(&**v) != now.get(*k)) {
            clamped.extend(limits::clamp(k, v, &now[*k]));
            validate(k, v, &now[*k])?;
            limits::check(k, v, &now[*k]).map_err(ApiError::BadRequest)?;
        }
    }
    web_mode_ok(&old, &now)?;
    let mut tx = st.db.begin().await?;
    let cur = store::read_permits(&mut tx, guild).await?;
    if let Some(p) = old.get_mut("permits").filter(|_| before_handover) {
        held.extend(hold_back_permits(p, &cur));
    }
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
        Some(version),
        format!(
            "{} (<@{}>) rolled config back to v{v} -> v{version}{}{}",
            s.username,
            s.user_id,
            match clamped.is_empty() {
                true => String::new(),
                false => format!(", moved into range: {}", clamped.join(", ")),
            },
            match held.is_empty() {
                true => String::new(),
                false => format!(", kept from the ownership change: {}", held.join(", ")),
            }
        ),
    )
    .await;
    Ok(Json(
        json!({"version": version, "clamped": clamped, "held": held}),
    ))
}

/// Whether a permit is guarded (`permits::guarded`): an ownership change pauses its roles.
fn strong_permit(p: &Value) -> bool {
    let list = |k: &str| -> Vec<&str> {
        p[k].as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    tommybot_core::permits::guarded(&list("perms"), &list("whitelist"))
}

/// For a rollback to before the last ownership change: EO and TA stay as they are now, and no
/// role becomes guarded again, judged by the old version and the current one (R-h3 round 2), so
/// only Keep on Overview brings back what the change paused. Returns what was held.
fn hold_back_permits(p: &mut Value, cur: &Value) -> Vec<String> {
    let mut held = vec![];
    for (k, what) in [("eo", "Extra Owners"), ("ta", "Trusted Admins")] {
        if p[k] != cur[k] {
            p[k] = cur[k].clone();
            held.push(format!("{what} stay as they are"));
        }
    }
    let now = |name: &Value| {
        cur["custom"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"] == *name)
            .cloned()
    };
    for c in p["custom"].as_array_mut().into_iter().flatten() {
        if !strong_permit(c) {
            continue;
        }
        let name = c["name"].as_str().unwrap_or("?").to_string();
        match now(&c["name"]) {
            // Guarded now too: its current roles stay, the paused ones come back only through Keep.
            Some(n) if strong_permit(&n) => {
                if c["roles"] != n["roles"] {
                    c["roles"] = n["roles"].clone();
                    held.push(format!("the roles of the permit {name} stay as they are"));
                }
            }
            // Weak now: its roles stay, but without the old guarded perms and flags.
            Some(n) => {
                c["perms"] = n["perms"].clone();
                c["whitelist"] = n["whitelist"].clone();
                held.push(format!("the permit {name} keeps its current perms"));
            }
            None if c["roles"].as_array().is_some_and(|r| !r.is_empty()) => {
                c["roles"] = json!([]);
                held.push(format!("the permit {name} comes back without roles"));
            }
            None => {}
        }
    }
    held
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

/// Anti-nuke watches @everyone and the main roles while a monitor is on, so changing the main roles
/// then needs anti-nuke's bar (D67). `config` is the whole config whose monitors count.
fn main_roles_guarded(config: &Value, lockdown: &Value, was: &Value) -> bool {
    let an = &config["antinuke"];
    lockdown["main_roles"] != was["main_roles"]
        && (an["monitor_public_roles"] == true || an["monitor_channel_perms"] == true)
}

/// Whether going from `now` to `new` changes the main roles while either has a monitor on.
fn main_roles_bar(new: &Value, now: &Value) -> bool {
    [new, now]
        .iter()
        .any(|c| main_roles_guarded(c, &new["lockdown"], &now["lockdown"]))
}

/// The highest level any section a rollback from `now` to `old` changes needs (permits are checked apart).
fn rollback_bar(old: &Value, now: &Value) -> Level {
    if main_roles_bar(old, now) {
        return access::write_bar("antinuke").0;
    }
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
        // Removed sections (the old Channel Setup, Ban Sync); exports made before still carry them.
        if k == "channel_profiles" || k == "ban_sync" {
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
        if k == "raider_list" {
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
            validate(k, &v, &now[k])?;
            limits::check(k, &v, &now[k]).map_err(ApiError::BadRequest)?;
        }
        new[k] = v;
    }
    Ok((new, cleared))
}

/// Level and step-up an import needs: the bar of every section it changes, Admin + step-up for permits.
fn import_bar(new: &Value, now: &Value) -> (Level, bool) {
    let permits = new.get("permits") != now.get("permits");
    let step_up = permits
        || main_roles_bar(new, now)
        || access::SENSITIVE.iter().any(|k| new.get(*k) != now.get(*k));
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
    web_mode_ok(&new, &now)?;
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
        Some(version),
        format!(
            "{} (<@{}>) imported settings from another server -> config v{version}",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(Json(json!({"version": version, "cleared": cleared})))
}

/// Sections the bot parses must deserialize into its types; the rest are stored as-is. `old` is
/// the stored section: an entry the bot skips is refused only when it is not stored as it is.
pub(crate) fn validate(section: &str, v: &Value, old: &Value) -> Result<(), ApiError> {
    // The bot skips an unknown entry (D48); a save must still refuse it.
    if let Some(e) = tommybot_core::sections::unreadable_entry(section, v, old) {
        return Err(ApiError::BadRequest(format!(
            "{section}: unknown entry {e}"
        )));
    }
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
    let r = reads(section, v.clone());
    r.map_err(|e| ApiError::BadRequest(format!("invalid {section} config: {e}")))
}

/// Whether the bot's type for `section` reads `v`; one that fails runs on its defaults (D48).
fn reads(section: &str, v: Value) -> Result<(), serde_json::Error> {
    use tommybot_core::sections::{
        AntinukeCfg, AppealsCfg, AutomodCfg, BackupsCfg, CatchCfg, LockdownCfg, MiscCfg, PanicCfg,
        RaiderListCfg, TicketsCfg, WhitelistCfg,
    };
    match section {
        "logging" => serde_json::from_value::<tommybot_logging::LogConfig>(v).map(drop),
        "antinuke" => serde_json::from_value::<AntinukeCfg>(v).map(drop),
        "automod" => serde_json::from_value::<AutomodCfg>(v).map(drop),
        "joingate" => serde_json::from_value::<tommybot_automod::join::GateCfg>(v).map(drop),
        "verification" => {
            serde_json::from_value::<tommybot_core::sections::VerificationCfg>(v).map(drop)
        }
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
        "raider_list" => serde_json::from_value::<RaiderListCfg>(v).map(drop),
        _ => Ok(()),
    }
}

/// Sections in `config` that the bot cannot read and so runs on defaults (S-h3 6).
pub(crate) fn unread_sections(config: &Value) -> Vec<&'static str> {
    access::SECTIONS
        .iter()
        .copied()
        .filter(|k| {
            config
                .get(*k)
                .is_some_and(|v| !v.is_null() && reads(k, v.clone()).is_err())
        })
        .collect()
}

/// Web verification's rules, checked only when a whole-config write changes verification or lockdown.
fn web_mode_ok(new: &Value, now: &Value) -> Result<(), ApiError> {
    let web = ["verification", "lockdown"]
        .iter()
        .any(|k| new.get(*k) != now.get(*k));
    match tommybot_automod::join::web_mode_error(new).filter(|_| web) {
        Some(e) => Err(ApiError::BadRequest(e.into())),
        None => Ok(()),
    }
}

/// No negative heat weights. Max and decay have their ranges in `limits`.
fn heat_ok(h: &tommybot_automod::heat::HeatCfg) -> Result<(), &'static str> {
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

    /// S-h3 6: a section the bot cannot read is named, so Overview and the page show it.
    #[test]
    fn unread_sections_are_named() {
        let c =
            json!({"antinuke": {"enabled": "yes"}, "automod": {}, "logging": null, "permits": {}});
        assert_eq!(unread_sections(&c), ["antinuke"]);
        assert!(unread_sections(&json!({"antinuke": {"enabled": true}})).is_empty());
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres. Only a save also reaches the channel
    /// of the version before; other events go to the current channel alone.
    #[tokio::test]
    async fn change_log_goes_to_the_old_channel_only_for_the_save_that_moved_it() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_884_000_000 + std::process::id() as i64;
        let cfg = |ch: Option<&str>| json!({"logging": {"config_changes": ch}});
        for (v, ch) in [(1, Some("11")), (2, Some("22")), (3, None)] {
            sqlx::query("insert into guild_config (guild_id, version, config) values ($1, $2, $3)")
                .bind(g)
                .bind(v)
                .bind(cfg(ch))
                .execute(&db)
                .await
                .unwrap();
        }
        let (moved, none) = (cfg(Some("22")), cfg(None));
        let at = |c, v| change_log_channels(&db, g, c, v);
        assert_eq!(at(&moved, Some(2)).await, ["22", "11"], "moved");
        assert_eq!(at(&none, Some(3)).await, ["22"], "cleared");
        assert!(at(&none, None).await.is_empty(), "no save: current only");
        assert_eq!(at(&moved, None).await, ["22"]);
        sqlx::query("delete from guild_config where guild_id = $1")
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
    }

    #[test]
    fn verification_is_checked_with_the_bots_type() {
        // `channel` and `roles` are only in the bot's VerificationCfg; a bad one made the bot read it all as off.
        assert!(validate(
            "verification",
            &json!({"channel": "not an id"}),
            &Value::Null
        )
        .is_err());
        assert!(validate("verification", &json!({"roles": "6"}), &Value::Null).is_err());
        assert!(validate(
            "verification",
            &json!({"enabled": true, "channel": "5", "roles": ["6"]}),
            &Value::Null
        )
        .is_ok());
    }

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
        let old = json!({"ban_sync": {"enabled": true, "receive": "ban"}, "automod": {}});
        let (new, _) = import_config(&old, &json!({})).unwrap();
        assert!(new.get("ban_sync").is_none());
    }

    #[test]
    fn import_never_turns_raider_list_on() {
        let file = json!({"raider_list": {"enabled": true, "action": "kick"}});
        let (new, _) = import_config(&file, &json!({})).unwrap();
        assert_eq!(
            new["raider_list"],
            json!({"enabled": false, "action": "kick"})
        );
        let bad = json!({"raider_list": {"categories": ["nuke", "bad"]}});
        assert!(import_config(&bad, &json!({})).is_err());
        assert_eq!(import_bar(&new, &json!({})), (Level::Owner, true));
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
    fn main_roles_need_the_antinuke_bar_while_a_monitor_is_on() {
        let now =
            json!({"lockdown": {"main_roles": ["1"]}, "antinuke": {"monitor_public_roles": false}});
        let mut new = now.clone();
        new["lockdown"]["main_roles"] = json!(["2"]);
        assert_eq!(rollback_bar(&new, &now), Level::Editor);
        assert_eq!(import_bar(&new, &now), (Level::Editor, false));
        let mut on = now.clone();
        on["antinuke"]["monitor_channel_perms"] = json!(true);
        let mut new_on = on.clone();
        new_on["lockdown"]["main_roles"] = json!(["2"]);
        assert_eq!(rollback_bar(&new_on, &on), Level::Owner);
        assert_eq!(import_bar(&new_on, &on), (Level::Owner, true));
        assert!(main_roles_guarded(
            &on,
            &new_on["lockdown"],
            &on["lockdown"]
        ));
        assert!(!main_roles_guarded(&on, &on["lockdown"], &on["lockdown"]));
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
        let stats =
            json!({"custom": [{"name": "S", "roles": ["4"], "perms": ["cases", "mod_stats"]}]});
        assert_ne!(
            antinuke_exempt(&json!({"custom": []})),
            antinuke_exempt(&stats)
        );
        let more = json!({"custom": [{"name": "S", "roles": ["4", "5"], "perms": ["mod_stats"]}]});
        assert_ne!(antinuke_exempt(&stats), antinuke_exempt(&more));
        let other =
            json!({"custom": [{"name": "S", "roles": ["4"], "perms": ["mod_stats", "ban"]}]});
        assert_eq!(antinuke_exempt(&stats), antinuke_exempt(&other));
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
    fn held_roles_are_the_quarantine_and_main_roles() {
        let v = json!({"quarantine_role": "5", "main_roles": ["6", "7"]});
        assert_eq!(held_roles("antinuke", &v), vec!["5"]);
        assert_eq!(held_roles("lockdown", &v), vec!["6", "7"]);
        assert!(held_roles("logging", &v).is_empty());
        assert!(held_roles("antinuke", &json!({})).is_empty());
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
        let heat = |h| validate("automod", &json!({"heat": h}), &Value::Null);
        assert!(heat(json!({})).is_ok());
        let ranged = |h| limits::check("automod", &json!({"heat": h}), &json!({}));
        assert!(ranged(json!({"max": 0})).is_err());
        assert!(ranged(json!({"decay_per_sec": 1})).is_err());
        assert!(ranged(json!({"decay_per_sec": 0})).is_err());
        assert!(ranged(json!({"decay_per_sec": -0.1})).is_err());
        assert!(heat(json!({"per_emoji": -1})).is_err());
    }

    #[test]
    fn discord_domains_cannot_be_blacklisted() {
        let bad = |d| {
            validate(
                "automod",
                &json!({"filters": {"blacklisted_domains": [d]}}),
                &Value::Null,
            )
        };
        assert!(bad("Discord.gg").is_err());
        assert!(bad("cdn.discordapp.com").is_err());
        assert!(bad("evil.example").is_ok());
    }

    #[test]
    fn custom_type_needs_channel_and_events() {
        let bad = |c| validate("logging", &json!({"custom": [c]}), &Value::Null).is_err();
        assert!(bad(json!({"events": ["member_kick"], "channel": null})));
        assert!(bad(json!({"events": [], "channel": "5"})));
        assert!(!bad(json!({"events": ["member_kick"], "channel": "5"})));
    }
}

/// Handler tests on a scratch Postgres (they return early without DATABASE_URL). Discord is never
/// called: the guild and member lookups are pre-filled in the memo, user 7 owns the server.
#[cfg(test)]
pub(crate) mod g3_handler_tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    pub(crate) async fn setup(base: i64) -> Option<(AppState, Sess, i64)> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = base + std::process::id() as i64;
        let discord = crate::discord::Discord {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            client_id: String::new(),
            client_secret: String::new(),
            bot_token: String::new(),
            redirect_uri: String::new(),
            api: "http://127.0.0.1:1".into(),
            memo: Default::default(),
        };
        {
            let mut m = discord.memo.lock().unwrap();
            // Dated ahead, so the 2 s fresh owner read still hits it on a slow test run.
            m.insert(
                format!("/guilds/{g}"),
                (
                    Instant::now() + std::time::Duration::from_secs(60),
                    Some(json!({"owner_id": "7", "name": "T", "roles": []})),
                ),
            );
            m.insert(
                format!("/guilds/{g}/members/7"),
                (Instant::now(), Some(json!({"roles": []}))),
            );
        }
        let st = AppState {
            db,
            discord,
            secure: false,
            totp: Default::default(),
            rate: Default::default(),
            ips: Default::default(),
            vpn_logged: Default::default(),
        };
        let s = Sess {
            id: String::new(),
            user_id: 7,
            username: "owner".into(),
            access_token: String::new(),
            stepped_up: true,
            csrf: String::new(),
        };
        Some((st, s, g))
    }

    async fn put_version(st: &AppState, g: i64, v: i32, c: Value) {
        sqlx::query("insert into guild_config (guild_id, version, config) values ($1, $2, $3)")
            .bind(g)
            .bind(v)
            .bind(c)
            .execute(&st.db)
            .await
            .unwrap();
    }

    async fn cleanup(st: &AppState, g: i64) {
        for t in ["guild_config", "trusted_admins", "extra_owners"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "delete from {t} where guild_id = $1"
            )))
            .bind(g)
            .execute(&st.db)
            .await
            .unwrap();
        }
    }

    async fn tas(st: &AppState, g: i64) -> Vec<i64> {
        sqlx::query_scalar("select user_id from trusted_admins where guild_id = $1")
            .bind(g)
            .fetch_all(&st.db)
            .await
            .unwrap()
    }

    fn body<T: serde::de::DeserializeOwned>(v: Value) -> Json<T> {
        Json(serde_json::from_value(v).unwrap())
    }

    /// D65. TA 55 is removed in Discord, which (since the fix, see the bot's
    /// `trusted_admin_removal_is_a_config_version`) appends v3 with the live permits. A Permits
    /// page or History page still holding v2 cannot write 55 back.
    #[tokio::test]
    async fn stale_permits_save_and_rollback_do_not_bring_back_a_removed_ta() {
        let Some((st, s, g)) = setup(9_880_000_000).await else {
            return;
        };
        let with = |p: &str, ta: Value| json!({"misc": {"prefix": p}, "permits": {"eo": [], "ta": ta, "custom": []}});
        put_version(&st, g, 1, with("a!", json!(["55"]))).await;
        put_version(&st, g, 2, with("b!", json!(["55"]))).await;
        // The Discord removal as the bot now does it: row gone, v3 with the live permits.
        put_version(&st, g, 3, with("b!", json!([]))).await;
        let stale = put_permits(
            State(st.clone()),
            s.clone(),
            Path(g),
            body(json!({"expected_version": 2, "value": {"eo": [], "ta": ["55"], "custom": []}})),
        )
        .await;
        assert!(matches!(stale, Err(ApiError::Conflict(3))), "{stale:?}");
        let back = rollback(
            State(st.clone()),
            s.clone(),
            Path((g, 1)),
            body(json!({"expected_version": 2})),
        )
        .await;
        assert!(matches!(back, Err(ApiError::Conflict(3))), "{back:?}");
        assert!(tas(&st, g).await.is_empty(), "55 was written back");
        cleanup(&st, g).await;
    }

    /// D65 History. "Diff vs current" compares with the live permit tables, so a table change
    /// the newest version's copy does not hold (TA removed) shows up in what a rollback changes.
    #[tokio::test]
    async fn diff_vs_current_shows_the_live_permits() {
        let Some((st, s, g)) = setup(9_881_000_000).await else {
            return;
        };
        let with = |p: &str| json!({"misc": {"prefix": p}, "permits": {"eo": [], "ta": ["55"], "custom": []}});
        put_version(&st, g, 1, with("a!")).await;
        put_version(&st, g, 2, with("b!")).await;
        // trusted_admins has no row: 55 was removed without a version (as before the fix).
        let Json(d) = diff_versions(State(st.clone()), s.clone(), Path((g, 1, 2)))
            .await
            .unwrap();
        let text = d.to_string();
        assert!(
            text.contains("permits"),
            "rollback to v1 re-adds TA 55 unseen: {text}"
        );
        cleanup(&st, g).await;
    }

    /// D69 + D70. A rollback runs the save checks: an old version whose verification the bot cannot
    /// parse, or whose panic window is 0, is refused; an in-range one still rolls back.
    #[tokio::test]
    async fn rollback_refuses_what_a_save_refuses() {
        let Some((st, s, g)) = setup(9_882_000_000).await else {
            return;
        };
        put_version(
            &st,
            g,
            1,
            json!({"verification": {"enabled": true, "channel": "not an id"}}),
        )
        .await;
        put_version(&st, g, 2, json!({"panic": {"window_secs": 0}})).await;
        put_version(
            &st,
            g,
            3,
            json!({"automod": {"strike": {"strike_timeout_ms": 0}}}),
        )
        .await;
        put_version(&st, g, 4, json!({"panic": {"window_secs": 600}})).await;
        put_version(&st, g, 5, json!({})).await;
        let r = rollback(
            State(st.clone()),
            s.clone(),
            Path((g, 1)),
            body(json!({"expected_version": 5})),
        )
        .await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))), "{r:?}");
        // D69b: an out-of-range number is clamped and named instead of refusing the rollback.
        let r = rollback(
            State(st.clone()),
            s.clone(),
            Path((g, 2)),
            body(json!({"expected_version": 5})),
        )
        .await
        .unwrap();
        assert_eq!(r.0["clamped"], json!(["panic.window_secs: 0 -> 1"]));
        let (v, c) = store::current(&st.db, g).await.unwrap();
        assert_eq!((v, &c["panic"]["window_secs"]), (6, &json!(1)));
        let ok = rollback(
            State(st.clone()),
            s.clone(),
            Path((g, 4)),
            body(json!({"expected_version": 6})),
        )
        .await;
        assert!(ok.is_ok(), "{ok:?}");
        cleanup(&st, g).await;
    }

    /// D70. A section save refuses 0 for a window or count that would switch its feature off,
    /// and `panic.executors` 0 (off on purpose) still saves.
    #[tokio::test]
    async fn section_save_refuses_zero_windows() {
        let Some((st, s, g)) = setup(9_883_000_000).await else {
            return;
        };
        let save = |sec: &'static str, value: Value| {
            let (st, s) = (st.clone(), s.clone());
            async move {
                put_section(
                    State(st),
                    s,
                    Path((g, sec.to_string())),
                    body(json!({"expected_version": 0, "value": value})),
                )
                .await
            }
        };
        for (sec, v) in [
            ("panic", json!({"window_secs": 0})),
            (
                "automod",
                json!({"auto_lockdown": {"mention_window_ms": 0}}),
            ),
            ("automod", json!({"strike": {"cap": 0}})),
            ("joinraid", json!({"span_ms": 0})),
        ] {
            let r = save(sec, v.clone()).await;
            assert!(
                matches!(r, Err(ApiError::BadRequest(_))),
                "{sec} {v}: {r:?}"
            );
        }
        let off = save("panic", json!({"executors": 0})).await;
        assert!(off.is_ok(), "{off:?}");
        cleanup(&st, g).await;
    }

    /// D47 (S1): the 30 s owner memo still names user 7, but the bot already moved the server on.
    #[tokio::test]
    async fn a_stale_owner_memo_gives_the_previous_owner_nothing() {
        let Some((st, s, g)) = setup(9_894_000_000).await else {
            return;
        };
        let wipe = || async {
            for t in ["guild_owners", "owner_handovers"] {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "delete from {t} where guild_id = $1"
                )))
                .bind(g)
                .execute(&st.db)
                .await
                .unwrap();
            }
        };
        wipe().await;
        assert!(is_discord_owner(&st, &s, g).await.unwrap());
        // R-h3 round 2, 3: the bot still names user 8 (its on_owner failed) but Discord says 7 owns
        // it: 7 keeps Owner level, the owner-only writes wait, and a retry is queued.
        sqlx::query("insert into guild_owners (guild_id, owner_id) values ($1, 8)")
            .bind(g)
            .execute(&st.db)
            .await
            .unwrap();
        sqlx::query("delete from scheduled_jobs where guild_id = $1")
            .bind(g)
            .execute(&st.db)
            .await
            .unwrap();
        assert_eq!(level_of(&st, &s, g).await.unwrap(), Level::Owner);
        assert!(!is_discord_owner(&st, &s, g).await.unwrap());
        let _ = level_of(&st, &s, g).await.unwrap();
        let queued: i64 = sqlx::query_scalar(
            "select count(*) from scheduled_jobs where guild_id = $1 and kind = 'owner_check'",
        )
        .bind(g)
        .fetch_one(&st.db)
        .await
        .unwrap();
        assert_eq!(queued, 1, "one retry, not one per request");
        sqlx::query("delete from scheduled_jobs where guild_id = $1")
            .bind(g)
            .execute(&st.db)
            .await
            .unwrap();
        // R-h3 round 3: a dropped retry is not queued again within the hour.
        let _ = level_of(&st, &s, g).await.unwrap();
        let queued: i64 =
            sqlx::query_scalar("select count(*) from scheduled_jobs where guild_id = $1")
                .bind(g)
                .fetch_one(&st.db)
                .await
                .unwrap();
        assert_eq!(queued, 0, "at most one owner_check per hour");
        // S-h3 round 3: while the reset is pending the real owner may remove Extra Owners, not add.
        sqlx::query("insert into extra_owners (guild_id, user_id) values ($1, 5)")
            .bind(g)
            .execute(&st.db)
            .await
            .unwrap();
        let put = |eo: Value| {
            let (st, s) = (st.clone(), s.clone());
            async move {
                let v = store::current(&st.db, g).await.unwrap().0;
                put_permits(
                    State(st),
                    s,
                    Path(g),
                    body(
                        json!({"expected_version": v, "value": {"eo": eo, "ta": [], "custom": []}}),
                    ),
                )
                .await
            }
        };
        let add = put(json!(["5", "6"])).await;
        assert!(matches!(add, Err(ApiError::Forbidden(_))), "{add:?}");
        let remove = put(json!([])).await;
        assert!(remove.is_ok(), "{remove:?}");
        // S1: the memo (10 s old) still names 7, but no fresh answer confirms it.
        st.discord.memo.lock().unwrap().insert(
            format!("/guilds/{g}"),
            (
                Instant::now() - std::time::Duration::from_secs(10),
                Some(json!({"owner_id": "7", "name": "T", "roles": []})),
            ),
        );
        wipe().await;
        for sql in [
            "insert into guild_owners (guild_id, owner_id) values ($1, 8)",
            "insert into owner_handovers (guild_id, old_owner, new_owner) values ($1, 7, 8)",
        ] {
            sqlx::query(sql).bind(g).execute(&st.db).await.unwrap();
            assert!(!is_discord_owner(&st, &s, g).await.unwrap(), "{sql}");
            assert!(level_of(&st, &s, g).await.unwrap() < Level::Owner);
            wipe().await;
        }
    }

    async fn c_qw(st: &AppState, g: i64) -> Value {
        store::current(&st.db, g).await.unwrap().1["antinuke"]["quarantine_whitelist"].clone()
    }

    /// D47 (R-h3 S4): a rollback to before the ownership change leaves the paused staff, the
    /// strong permit's roles as they are, says so, and the card stays.
    #[tokio::test]
    async fn a_rollback_to_before_the_handover_keeps_what_it_paused() {
        let Some((st, s, g)) = setup(9_895_000_000).await else {
            return;
        };
        cleanup(&st, g).await;
        for sql in [
            "delete from owner_handovers where guild_id = $1",
            "delete from permits where guild_id = $1",
            "insert into permits (guild_id, name, perms) values ($1, 'Admins', '{dashboard_edit}'), ($1, 'Mods', '{ban}'), ($1, 'Helpers', '{warn}')",
            "insert into permit_roles (permit_id, guild_id, role_id) select id, $1, 22 from permits where guild_id = $1 and name = 'Mods'",
            "insert into permit_roles (permit_id, guild_id, role_id) select id, $1, 30 from permits where guild_id = $1 and name = 'Helpers'",
            "insert into owner_handovers (guild_id, old_owner, new_owner, extra_owners, paused_roles, at) values ($1, 3, 7, '{5}', '[{\"permit\": \"Admins\", \"role\": \"20\"}]', now() - interval '1 minute')",
        ] {
            sqlx::query(sql).bind(g).execute(&st.db).await.unwrap();
        }
        // Helpers was guarded in v1 and made weak before the handover, so its role stayed on.
        let old = json!({"antinuke": {"quarantine_whitelist": ["9"]}, "permits": {"eo": ["5"], "ta": ["6"], "custom": [
            {"name": "Admins", "rank": 0, "perms": ["dashboard_edit"], "limits": {}, "whitelist": [], "roles": ["20"]},
            {"name": "Helpers", "rank": 0, "perms": ["dashboard_edit"], "limits": {}, "whitelist": ["wl_role_delete"], "roles": ["30"]},
            {"name": "Mods", "rank": 0, "perms": ["ban"], "limits": {}, "whitelist": [], "roles": ["21"]}]}});
        sqlx::query("insert into guild_config (guild_id, version, config, created_at) values ($1, 1, $2, now() - interval '1 hour')")
            .bind(g).bind(old).execute(&st.db).await.unwrap();
        put_version(&st, g, 2, json!({})).await;
        let r = rollback(
            State(st.clone()),
            s.clone(),
            Path((g, 1)),
            body(json!({"expected_version": 2})),
        )
        .await
        .unwrap();
        assert_eq!(
            r.0["held"],
            json!([
                "the quarantine whitelist stays as it is",
                "Extra Owners stay as they are",
                "Trusted Admins stay as they are",
                "the roles of the permit Admins stay as they are",
                "the permit Helpers keeps its current perms"
            ])
        );
        assert_eq!(c_qw(&st, g).await, json!([]));
        let (_, c) = store::current(&st.db, g).await.unwrap();
        let p = &c["permits"];
        assert_eq!((&p["eo"], &p["ta"]), (&json!([]), &json!([])));
        let roles = |n: &str| {
            p["custom"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == n)
                .unwrap()["roles"]
                .clone()
        };
        assert_eq!(
            roles("Admins"),
            json!([]),
            "paused, so only Keep brings it back"
        );
        assert_eq!(
            roles("Mods"),
            json!(["21"]),
            "a weaker permit rolls back as usual"
        );
        let helpers = p["custom"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "Helpers")
            .unwrap();
        assert_eq!(
            (&helpers["perms"], &helpers["whitelist"], &helpers["roles"]),
            (&json!(["warn"]), &json!([]), &json!(["30"])),
            "R2-2: never guarded again without Keep"
        );
        let card: i64 = sqlx::query_scalar("select count(*) from owner_handovers where guild_id = $1 and kept_at is null and extra_owners = '{5}'")
            .bind(g).fetch_one(&st.db).await.unwrap();
        assert_eq!(card, 1, "the card still offers the paused ones");
        for sql in [
            "delete from owner_handovers where guild_id = $1",
            "delete from permits where guild_id = $1",
        ] {
            sqlx::query(sql).bind(g).execute(&st.db).await.unwrap();
        }
        cleanup(&st, g).await;
    }

    /// R-h3 2: after a downgrade, a section holding an entry only a newer version reads can
    /// still be saved, while a new or changed such entry is refused.
    #[tokio::test]
    async fn a_downgraded_section_still_saves() {
        let Some((st, s, g)) = setup(9_897_000_000).await else {
            return;
        };
        cleanup(&st, g).await;
        let stored = json!({"enabled": false, "limits": {"Future": {"per": 3}}});
        put_version(&st, g, 1, json!({"antinuke": stored})).await;
        let save = |value: Value, v: i32| {
            let (st, s) = (st.clone(), s.clone());
            async move {
                put_section(
                    State(st),
                    s,
                    Path((g, "antinuke".to_string())),
                    body(json!({"expected_version": v, "value": value})),
                )
                .await
            }
        };
        let ok = save(
            json!({"enabled": true, "limits": {"Future": {"per": 3}}}),
            1,
        )
        .await;
        assert!(ok.is_ok(), "{ok:?}");
        let changed = save(
            json!({"enabled": true, "limits": {"Future": {"per": 4}}}),
            2,
        )
        .await;
        assert!(
            matches!(changed, Err(ApiError::BadRequest(_))),
            "{changed:?}"
        );
        let bad = save(
            json!({"enabled": true, "limits": {"Ban": {"enabled": "no"}}}),
            2,
        )
        .await;
        assert!(matches!(bad, Err(ApiError::BadRequest(_))), "{bad:?}");
        cleanup(&st, g).await;
    }
}
