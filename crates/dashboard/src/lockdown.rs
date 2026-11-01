use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    routing::{get, post, put},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use tommybot_automod::{
    discord_api::{rule_body, BotAction, DiscordAutomodCfg, MentionScope, RuleSpec},
    lockdown::{kind_from, tshoot, Fix, LockKind, Overwrite, Problem, RoleInfo, Snapshot},
};
use tommybot_core::jobs;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/lockdown", get(status))
        .route("/api/guilds/{guild}/lockdown/lock", post(lock))
        .route("/api/guilds/{guild}/lockdown/unlock", post(unlock))
        .route("/api/guilds/{guild}/lockdown/update", post(send_update))
        .route("/api/guilds/{guild}/lockdown/channels", get(channels))
        .route("/api/guilds/{guild}/lockdown/check", get(check))
        .route("/api/guilds/{guild}/lockdown/fix", post(fix))
        .route(
            "/api/guilds/{guild}/discord-rules",
            get(rules).post(create_rule),
        )
        .route(
            "/api/guilds/{guild}/discord-rules/{rule}",
            put(update_rule).delete(delete_rule),
        )
        .route(
            "/api/guilds/{guild}/discord-rules/{rule}/enabled",
            post(set_enabled),
        )
        .route(
            "/api/guilds/{guild}/discord-rules-migrate-words",
            post(migrate_words),
        )
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

async fn status(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let rows: Vec<(sqlx::types::Json<Snapshot>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "select snapshot, created_at from lock_snapshots where guild_id = $1 order by created_at",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    let unlock_at: Option<(chrono::DateTime<chrono::Utc>,)> = sqlx::query_as(
        "select run_at from scheduled_jobs where guild_id = $1 and kind = 'unlock' order by run_at limit 1",
    )
    .bind(guild)
    .fetch_optional(&st.db)
    .await?;
    let locks: Vec<Value> = rows
        .iter()
        .map(|(snap, at)| {
            let kind = snap.0.kind.as_ref();
            let (channels, roles, join) = kind.map(LockKind::covers).unwrap_or_default();
            json!({"channels": channels, "roles": roles, "join": join.map(|j| format!("{j:?}")),
                   "channel": matches!(kind, Some(LockKind::Channel(_))), "since": at,
                   "hidden": matches!(kind, Some(LockKind::Blind)),
                   "invites_paused": snap.0.invites_paused})
        })
        .collect();
    Ok(Json(
        json!({"locks": locks, "unlock_at": unlock_at.map(|(t,)| t)}),
    ))
}

#[derive(Deserialize)]
struct LockBody {
    kind: String,
    channel: Option<String>,
    duration_s: Option<i64>,
}

async fn lock(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<LockBody>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let channel = b.channel.as_deref().and_then(|c| c.parse().ok());
    kind_from(&b.kind, channel).ok_or_else(|| ApiError::BadRequest("unknown lock kind".into()))?;
    let duration = b.duration_s.filter(|d| *d > 0);
    jobs::enqueue(
        &st.db,
        guild,
        "lock",
        json!({"kind": b.kind, "channel": b.channel, "duration_s": duration}),
        now(),
    )
    .await?;
    audit(&st, &s, guild, format!("locked the server ({})", b.kind)).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize, Default)]
struct UnlockBody {
    /// Lifts only the locks overlapping this kind; absent lifts everything.
    kind: Option<String>,
}

async fn unlock(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    body: Option<Json<UnlockBody>>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let which = match body.and_then(|b| b.0.kind) {
        Some(k) => Some(
            kind_from(&k, None).ok_or_else(|| ApiError::BadRequest("unknown lock kind".into()))?,
        ),
        None => None,
    };
    let payload = which.as_ref().map(|k| json!({"lock": k}));
    sqlx::query("delete from scheduled_jobs where guild_id = $1 and kind = 'unlock' and ($2::jsonb is null or payload = $2)")
        .bind(guild)
        .bind(&payload)
        .execute(&st.db)
        .await?;
    jobs::enqueue(
        &st.db,
        guild,
        "unlock",
        payload.unwrap_or(Value::Null),
        now(),
    )
    .await?;
    audit(&st, &s, guild, "unlocked the server".into()).await?;
    Ok(Json(json!({"ok": true})))
}

/// Text-like channels with whether a server lock already denies sending there ("locked") and
/// whether @everyone cannot see them ("private"); Wick flags both in its channel list.
async fn channels(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let (_, c) = crate::store::current(&st.db, guild).await?;
    let main: Vec<String> = c["lockdown"]["main_roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(String::from))
        .chain([guild.to_string()])
        .collect();
    let all = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await?;
    let denies = |ch: &Value, bit: u64, roles: &[String]| {
        ch["permission_overwrites"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|o| {
                o["type"] == 0
                    && o["id"]
                        .as_str()
                        .is_some_and(|id| roles.iter().any(|r| r == id))
                    && o["deny"]
                        .as_str()
                        .and_then(|d| d.parse::<u64>().ok())
                        .is_some_and(|d| d & bit != 0)
            })
    };
    let everyone = [guild.to_string()];
    let list: Vec<Value> = all
        .as_array()
        .into_iter()
        .flatten()
        .filter(|ch| matches!(ch["type"].as_u64(), Some(0 | 5 | 15)))
        .map(|ch| {
            json!({"id": ch["id"], "name": ch["name"],
            "locked": denies(ch, tommybot_automod::lockdown::SEND_MESSAGES, &main),
            "private": denies(ch, tommybot_automod::lockdown::VIEW_CHANNEL, &everyone)})
        })
        .collect();
    Ok(Json(json!(list)))
}

#[derive(Deserialize)]
struct UpdateBody {
    text: String,
}

/// Wick's "Send an update": a message to the people locked out, posted in the `#server-locked`
/// channel of a hidden-channels lock, else in the Mod Logs channel.
async fn send_update(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<UpdateBody>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let text = b.text.trim();
    if text.is_empty() || text.chars().count() > 1900 {
        return Err(ApiError::BadRequest("write 1 to 1900 characters".into()));
    }
    let locked: Option<(sqlx::types::Json<Snapshot>,)> = sqlx::query_as(
        "select snapshot from lock_snapshots where guild_id = $1 and snapshot->>'locked_channel' is not null limit 1",
    )
    .bind(guild)
    .fetch_optional(&st.db)
    .await?;
    let (_, c) = crate::store::current(&st.db, guild).await?;
    let mod_logs = c["logging"]["routes"]["mod_command"]
        .as_str()
        .map(String::from);
    let channel = locked
        .and_then(|(snap,)| snap.0.locked_channel.map(|c| c.to_string()))
        .or(mod_logs)
        .ok_or_else(|| {
            ApiError::BadRequest("no lock channel or Mod Logs channel to post in".into())
        })?;
    let log =
        tommybot_logging::Log::new("Lockdown Update", tommybot_logging::embed::ALERT).text(text);
    st.discord.post_log(&channel, log).await?;
    Ok(Json(json!({"ok": true})))
}

pub(crate) async fn audit(
    st: &AppState,
    s: &Sess,
    guild: i64,
    what: String,
) -> Result<(), ApiError> {
    let (_, c) = crate::store::current(&st.db, guild).await?;
    log_change(
        st,
        &c,
        format!(
            "{} (<@{}>) {what} from the dashboard",
            s.username, s.user_id
        ),
    )
    .await;
    Ok(())
}

async fn roles(st: &AppState, guild: i64) -> Result<(Vec<RoleInfo>, i32), ApiError> {
    let (roles, bot_pos) = crate::ops::roles_and_bot_pos(st, guild).await?;
    let info = roles
        .iter()
        .map(|r| RoleInfo {
            id: r.id.parse().unwrap_or(0),
            perms: r.perms(),
            position: r.position as i32,
            managed: r.managed,
        })
        .collect();
    Ok((info, bot_pos as i32))
}

async fn problems(st: &AppState, guild: i64) -> Result<Vec<Problem>, ApiError> {
    let (_, c) = crate::store::current(&st.db, guild).await?;
    let main: Vec<u64> = c["lockdown"]["main_roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str()?.parse().ok())
        .collect();
    let (info, bot_pos) = roles(st, guild).await?;
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await?;
    let num = |v: &Value| v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let overwrites: Vec<Overwrite> = channels
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| {
            c["permission_overwrites"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o["type"] == 0)
                .map(|o| Overwrite {
                    channel: num(&c["id"]),
                    role: num(&o["id"]),
                    allow: num(&o["allow"]),
                    deny: num(&o["deny"]),
                })
        })
        .collect();
    Ok(tshoot(guild as u64, &main, &info, &overwrites, bot_pos))
}

async fn check(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    Ok(Json(json!(problems(&st, guild).await?)))
}

/// Wick's single Apply Fix button: applies every automatic fix the check found.
async fn fix(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let fixes: Vec<Fix> = problems(&st, guild)
        .await?
        .iter()
        .filter_map(|p| p.fix)
        .collect();
    for f in &fixes {
        let (path, body) = match *f {
            Fix::SetRolePerms { role, perms } => (
                format!("/guilds/{guild}/roles/{role}"),
                json!({"permissions": perms.to_string()}),
            ),
            Fix::SetOverwrite(o) => (
                format!("/channels/{}/permissions/{}", o.channel, o.role),
                json!({"type": 0, "allow": o.allow.to_string(), "deny": o.deny.to_string()}),
            ),
        };
        let method = if matches!(f, Fix::SetOverwrite(_)) {
            Method::PUT
        } else {
            Method::PATCH
        };
        st.discord.call(method, &path, Some(body)).await?;
    }
    audit(
        &st,
        &s,
        guild,
        format!("applied {} lockdown compatibility fix(es)", fixes.len()),
    )
    .await?;
    Ok(Json(json!({"ok": true, "fixed": fixes.len()})))
}

async fn rules(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let raw = st
        .discord
        .call(
            Method::GET,
            &format!("/guilds/{guild}/auto-moderation/rules"),
            None,
        )
        .await?;
    let (_, config) = crate::store::current(&st.db, guild).await?;
    let mine: DiscordAutomodCfg =
        serde_json::from_value(config["discord_automod"].clone()).unwrap_or_default();
    let list: Vec<Value> = raw
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let mut v = json!(RuleSpec::from_discord(r));
            v["id"] = r["id"].clone();
            v["bot_actions"] = json!(r["id"]
                .as_str()
                .and_then(|id| mine.actions.get(id))
                .cloned()
                .unwrap_or_default());
            v["lock"] = json!(r["id"]
                .as_str()
                .and_then(|id| mine.locks.get(id))
                .cloned()
                .unwrap_or_default());
            if let Some(sc) = r["id"].as_str().and_then(|id| mine.scopes.get(id)) {
                v["scope"] = json!(sc.scope);
                v["scope_triggers"] = json!(sc.triggers);
                v["scope_duration_s"] = json!(sc.duration_s);
            }
            v
        })
        .collect();
    Ok(Json(json!(list)))
}

fn body(spec: &RuleSpec) -> Result<Value, ApiError> {
    rule_body(spec).map_err(|e| ApiError::BadRequest(e.to_string()))
}

async fn create_rule(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(spec): Json<RuleSpec>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let b = body(&spec)?;
    let made = st
        .discord
        .call(
            Method::POST,
            &format!("/guilds/{guild}/auto-moderation/rules"),
            Some(b),
        )
        .await?;
    let id = made["id"].as_str().unwrap_or_default();
    set_bot_actions(&st, &s, guild, id, &spec).await?;
    audit(
        &st,
        &s,
        guild,
        format!("created the Discord AutoMod rule `{}`", spec.name),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn set_bot_actions(
    st: &AppState,
    s: &Sess,
    guild: i64,
    rule: &str,
    spec: &RuleSpec,
) -> Result<(), ApiError> {
    let (version, mut config) = crate::store::current(&st.db, guild).await?;
    // Wick-style mention scopes live next to the bot actions; Discord's own scope is on the rule.
    let scope = (spec.trigger == "mention" && matches!(spec.scope.as_str(), "user" | "server"))
        .then(|| MentionScope {
            scope: spec.scope.clone(),
            triggers: spec.scope_triggers.clamp(1, 100),
            duration_s: spec.scope_duration_s.clamp(1, 3600),
        });
    let mut changed = false;
    for (key, value) in [
        (
            "actions",
            (!spec.bot_actions.is_empty()).then(|| json!(spec.bot_actions)),
        ),
        ("scopes", scope.map(|s| json!(s))),
        (
            "locks",
            (spec.bot_actions.contains(&BotAction::Lock)
                && matches!(
                    spec.lock.as_str(),
                    "channel" | "server" | "kick" | "ban" | "invites"
                ))
            .then(|| json!(spec.lock)),
        ),
    ] {
        let slot = &mut config["discord_automod"][key];
        match value {
            Some(v) => {
                slot[rule] = v;
                changed = true;
            }
            None => changed |= slot.as_object_mut().and_then(|m| m.remove(rule)).is_some(),
        }
    }
    if !changed {
        return Ok(());
    }
    let mut tx = st.db.begin().await?;
    crate::store::commit(&mut tx, guild, version, &config, s.user_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Rule ids are snowflakes; anything else would let a caller reach other Discord endpoints.
fn rule_path(guild: i64, rule: &str) -> Result<String, ApiError> {
    if rule.is_empty() || !rule.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::BadRequest("bad rule id".into()));
    }
    Ok(format!("/guilds/{guild}/auto-moderation/rules/{rule}"))
}

async fn update_rule(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, rule)): Path<(i64, String)>,
    Json(spec): Json<RuleSpec>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let b = body(&spec)?;
    st.discord
        .call(Method::PATCH, &rule_path(guild, &rule)?, Some(b))
        .await?;
    set_bot_actions(&st, &s, guild, &rule, &spec).await?;
    audit(
        &st,
        &s,
        guild,
        format!("edited the Discord AutoMod rule `{}`", spec.name),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

/// Wick's "Migrate words to Discord's AutoMod": one copy of the local word blacklist into a new
/// keyword rule, then the local lists are cleared. Regex patterns stay local (rules have no regex here).
async fn migrate_words(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let (version, mut config) = crate::store::current(&st.db, guild).await?;
    let w = &mut config["automod"]["filters"]["words"];
    let list =
        |k: &str| -> Vec<String> { serde_json::from_value(w[k].clone()).unwrap_or_default() };
    let flag = |k: &str| w[k] == true;
    let keywords: Vec<String> = list("strict")
        .into_iter()
        .chain(list("wildcard").into_iter().map(|x| format!("*{x}*")))
        .chain(tommybot_automod::filters::premade_words(
            flag("premade_profane"),
            flag("premade_vulgar"),
            flag("premade_racist"),
        ))
        .collect();
    if keywords.is_empty() {
        return Err(ApiError::BadRequest("There are no words to migrate".into()));
    }
    if keywords.len() > 1000 || keywords.iter().any(|k| k.chars().count() > 60) {
        return Err(ApiError::BadRequest(
            "Discord allows at most 1000 words of up to 60 characters per rule".into(),
        ));
    }
    let spec = RuleSpec {
        name: "TommyBot migrated words".into(),
        enabled: true,
        trigger: "keyword".into(),
        keywords,
        block_message: true,
        ..Default::default()
    };
    st.discord
        .call(
            Method::POST,
            &format!("/guilds/{guild}/auto-moderation/rules"),
            Some(body(&spec)?),
        )
        .await?;
    for k in ["strict", "wildcard"] {
        w[k] = json!([]);
    }
    for k in ["premade_profane", "premade_vulgar", "premade_racist"] {
        w[k] = json!(false);
    }
    // The rule is a one-time copy, so the keep-in-sync toggle must not overwrite it later.
    w["use_discord_automod"] = json!(false);
    let mut tx = st.db.begin().await?;
    crate::store::commit(&mut tx, guild, version, &config, s.user_id).await?;
    tx.commit().await?;
    audit(
        &st,
        &s,
        guild,
        format!(
            "migrated {} word(s) to Discord AutoMod",
            spec.keywords.len()
        ),
    )
    .await?;
    Ok(Json(json!({"ok": true, "migrated": spec.keywords.len()})))
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn set_enabled(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, rule)): Path<(i64, String)>,
    Json(b): Json<EnabledBody>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    st.discord
        .call(
            Method::PATCH,
            &rule_path(guild, &rule)?,
            Some(json!({"enabled": b.enabled})),
        )
        .await?;
    audit(
        &st,
        &s,
        guild,
        format!(
            "{} a Discord AutoMod rule",
            if b.enabled {
                "activated"
            } else {
                "deactivated"
            }
        ),
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

async fn delete_rule(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, rule)): Path<(i64, String)>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    st.discord
        .call(Method::DELETE, &rule_path(guild, &rule)?, None)
        .await?;
    set_bot_actions(&st, &s, guild, &rule, &RuleSpec::default()).await?;
    audit(&st, &s, guild, "deleted a Discord AutoMod rule".into()).await?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ids_must_be_snowflakes() {
        assert!(rule_path(1, "123").is_ok());
        assert!(rule_path(1, "../roles/1").is_err());
        assert!(rule_path(1, "").is_err());
    }
}
