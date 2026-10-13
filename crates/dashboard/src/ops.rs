use crate::{access::Level, api::{log_change, require}, auth::{self, Sess}, ApiError, AppState};
use axum::{extract::{Path, Query, State}, routing::{get, post}, Json, Router};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tommybot_antinuke::Inverse;

type ActionRow = (i64, i64, String, Option<i64>, Option<Value>, bool, chrono::DateTime<chrono::Utc>);
type Res = Result<Json<Value>, ApiError>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/score", get(score))
        .route("/api/guilds/{guild}/score/fix", post(fix))
        .route("/api/guilds/{guild}/actions", get(actions))
        .route("/api/guilds/{guild}/revert", post(revert))
        .route("/api/guilds/{guild}/rescue", get(rescue_status))
        .route("/api/guilds/{guild}/rescue/regenerate", post(rescue_regenerate))
        .route("/api/rescue/claim", post(rescue_claim))
}

// ---- security score ----

const DANGEROUS: u64 = 0x8 | 0x20 | 0x1000_0000 | 0x10 | 0x4 | 0x2 | 0x2000_0000;

#[derive(Deserialize, Clone)]
struct Role {
    id: String,
    name: String,
    position: i64,
    #[serde(default)]
    managed: bool,
    permissions: String,
}

impl Role {
    fn perms(&self) -> u64 {
        self.permissions.parse().unwrap_or(0)
    }
}

fn row(id: &str, severity: &str, title: &str, detail: String, fix: Option<(&str, &str)>) -> Value {
    json!({"id": id, "severity": severity, "title": title, "detail": detail,
           "fix": fix.map(|(label, role_id)| json!({"label": label, "role_id": role_id}))})
}

fn checks(roles: &[Role], guild: i64, bot_pos: i64, config: &Value) -> Vec<Value> {
    let mut out = vec![];
    let quarantine = config["antinuke"]["quarantine_role"].as_str().and_then(|q| roles.iter().find(|r| r.id == q));
    match (config["antinuke"]["quarantine_role"].as_str(), quarantine) {
        (None, _) => out.push(row("quarantine_unset", "warn", "No quarantine role", "Anti-nuke has no role to quarantine offenders into.".into(), None)),
        (Some(_), None) => out.push(row("quarantine_missing", "bad", "Quarantine role missing", "The configured quarantine role no longer exists.".into(), None)),
        (Some(_), Some(q)) => {
            if q.position >= bot_pos {
                out.push(row("quarantine_above_bot", "bad", "Quarantine role above the bot", format!("{} is not below the bot's top role, so the bot cannot assign it.", q.name), None));
            }
            if q.perms() & DANGEROUS != 0 {
                out.push(row("quarantine_perms", "bad", "Quarantine role is too powerful", format!("{} grants dangerous permissions.", q.name), Some(("Strip permissions", &q.id))));
            }
        }
    }
    let above: Vec<&str> = roles.iter().filter(|r| r.position > bot_pos && !r.managed && r.id != guild.to_string()).map(|r| r.name.as_str()).collect();
    if above.is_empty() {
        out.push(row("bot_position", "ok", "Bot role is near the top", "No manageable roles sit above the bot.".into(), None));
    } else {
        out.push(row("bot_position", "warn", "Roles above the bot", format!("The bot cannot manage: {}.", above.join(", ")), None));
    }
    for r in roles.iter().filter(|r| !r.managed && r.id != guild.to_string() && r.perms() & DANGEROUS != 0 && quarantine.is_none_or(|q| q.id != r.id)) {
        out.push(row(&format!("role_{}", r.id), "bad", "Role holds dangerous permissions", format!("{} can administer, ban, kick or manage the server.", r.name), Some(("Strip permissions", &r.id))));
    }
    if config["logging"]["fallback"].as_str().is_none_or(str::is_empty) {
        out.push(row("log_fallback", "warn", "No fallback log channel", "Events without a dedicated channel are dropped.".into(), None));
    }
    out
}

async fn roles_of(st: &AppState, guild: i64) -> Result<Vec<Role>, ApiError> {
    Ok(serde_json::from_value(st.discord.call(Method::GET, &format!("/guilds/{guild}/roles"), None).await?).map_err(anyhow::Error::from)?)
}

async fn score(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let (_, config) = crate::store::current(&st.db, guild).await?.ok_or(ApiError::NotFound)?;
    let roles = roles_of(&st, guild).await?;
    let me = st.discord.call(Method::GET, "/users/@me", None).await?;
    let mine = st.discord.member_roles(guild, me["id"].as_str().unwrap_or_default()).await?;
    let bot_pos = roles.iter().filter(|r| mine.contains(&r.id)).map(|r| r.position).max().unwrap_or(0);
    Ok(Json(json!(checks(&roles, guild, bot_pos, &config))))
}

#[derive(Deserialize)]
struct FixBody {
    role_id: String,
}

async fn fix(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>, Json(b): Json<FixBody>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let roles = roles_of(&st, guild).await?;
    let r = roles.iter().find(|r| r.id == b.role_id).ok_or(ApiError::NotFound)?;
    if r.managed || r.id == guild.to_string() {
        return Err(ApiError::BadRequest("managed and @everyone roles cannot be changed here".into()));
    }
    let perms = (r.perms() & !DANGEROUS).to_string();
    st.discord.call(Method::PATCH, &format!("/guilds/{guild}/roles/{}", r.id), Some(json!({"permissions": perms}))).await?;
    if let Some((_, c)) = crate::store::current(&st.db, guild).await? {
        log_change(&st, &c, format!("{} (<@{}>) stripped dangerous permissions from role `{}`", s.username, s.user_id, r.name)).await;
    }
    Ok(Json(json!({"ok": true})))
}

// ---- revert center ----

#[derive(Deserialize)]
struct ActionsQ {
    actor: Option<i64>,
    hours: Option<i32>,
}

async fn actions(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>, Query(q): Query<ActionsQ>) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let rows: Vec<ActionRow> = sqlx::query_as(
        "select id, actor_id, action, target_id, inverse, reverted, created_at from action_log
         where guild_id = $1 and ($2::bigint is null or actor_id = $2)
           and ($3::int is null or created_at > now() - make_interval(hours => $3))
         order by created_at desc, id desc limit 200")
        .bind(guild).bind(q.actor).bind(q.hours).fetch_all(&st.db).await?;
    Ok(Json(json!(rows.iter().map(|(id, actor, action, target, inverse, reverted, at)| json!({
        "id": id, "actor_id": actor.to_string(), "action": action, "target_id": target.map(|t| t.to_string()),
        "inverse": inverse, "reverted": reverted, "at": at})).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct RevertBody {
    ids: Vec<i64>,
}

async fn run_inverse(st: &AppState, guild: i64, inv: &Inverse) -> anyhow::Result<()> {
    let (m, path, body) = match inv {
        Inverse::Unban { user } => (Method::DELETE, format!("/guilds/{guild}/bans/{user}"), None),
        Inverse::DeleteChannel { id } => (Method::DELETE, format!("/channels/{id}"), None),
        Inverse::DeleteRole { id } => (Method::DELETE, format!("/guilds/{guild}/roles/{id}"), None),
        Inverse::DeleteWebhook { id } => (Method::DELETE, format!("/webhooks/{id}"), None),
        Inverse::RestoreRoles { user, roles } => (Method::PATCH, format!("/guilds/{guild}/members/{user}"),
            Some(json!({"roles": roles.iter().map(u64::to_string).collect::<Vec<_>>()}))),
        Inverse::Unquarantine { .. } | Inverse::Reinvite { .. } => unreachable!("handled by the caller"),
    };
    st.discord.call(m, &path, body).await.map(drop)
}

async fn revert(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>, Json(b): Json<RevertBody>) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    // Newest first so later changes are undone before the ones they depend on.
    let rows: Vec<(i64, Value)> = sqlx::query_as(
        "select id, inverse from action_log where guild_id = $1 and id = any($2) and not reverted and inverse is not null
         order by created_at desc, id desc")
        .bind(guild).bind(&b.ids).fetch_all(&st.db).await?;
    let (mut done, mut failed, mut skipped, mut reinvite) = (0, vec![], vec![], vec![]);
    for (id, raw) in rows {
        let inv: Inverse = match serde_json::from_value(raw) {
            Ok(i) => i,
            Err(e) => { failed.push(json!({"id": id, "error": e.to_string()})); continue }
        };
        match &inv {
            Inverse::Unquarantine { .. } => { skipped.push(id); continue }
            Inverse::Reinvite { user } => reinvite.push(user.to_string()),
            _ => if let Err(e) = run_inverse(&st, guild, &inv).await {
                failed.push(json!({"id": id, "error": e.to_string()}));
                continue;
            },
        }
        sqlx::query("update action_log set reverted = true where id = $1").bind(id).execute(&st.db).await?;
        done += 1;
    }
    if let Some((_, c)) = crate::store::current(&st.db, guild).await? {
        log_change(&st, &c, format!("{} (<@{}>) reverted {done} action(s)", s.username, s.user_id)).await;
    }
    Ok(Json(json!({"done": done, "failed": failed, "skipped": skipped, "reinvite": reinvite})))
}

// ---- rescue key ----

fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn rescue_status(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, false).await?;
    let has_key: bool = sqlx::query_scalar("select exists(select 1 from rescue_key where guild_id = $1)").bind(guild).fetch_one(&st.db).await?;
    Ok(Json(json!({"has_key": has_key})))
}

async fn rescue_regenerate(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, true).await?;
    let key = auth::token();
    sqlx::query("insert into rescue_key (guild_id, key_hash) values ($1, $2)
                 on conflict (guild_id) do update set key_hash = $2, created_at = now()")
        .bind(guild).bind(hash(&key)).execute(&st.db).await?;
    Ok(Json(json!({"key": key})))
}

#[derive(Deserialize)]
struct Claim {
    guild_id: String,
    key: String,
}

async fn claim_key(db: &sqlx::PgPool, guild: i64, user: i64, key: &str) -> Result<(), ApiError> {
    let stored: Option<String> = sqlx::query_scalar("select key_hash from rescue_key where guild_id = $1").bind(guild).fetch_optional(db).await?;
    let h = hash(key);
    // Equal-length dummy compare when no key exists keeps timing uniform.
    if !ct_eq(stored.as_deref().unwrap_or(&"0".repeat(64)), &h) || stored.is_none() {
        return Err(ApiError::Forbidden("invalid rescue key"));
    }
    let mut tx = db.begin().await?;
    // The delete is the single-use gate: a concurrent claim sees zero rows.
    if sqlx::query("delete from rescue_key where guild_id = $1 and key_hash = $2").bind(guild).bind(&h).execute(&mut *tx).await?.rows_affected() == 0 {
        return Err(ApiError::Forbidden("invalid rescue key"));
    }
    sqlx::query("insert into extra_owners (guild_id, user_id) values ($1, $2) on conflict do nothing").bind(guild).bind(user).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn rescue_claim(State(st): State<AppState>, s: Sess, Json(b): Json<Claim>) -> Res {
    let guild: i64 = b.guild_id.parse().map_err(|_| ApiError::BadRequest("bad guild id".into()))?;
    claim_key(&st.db, guild, s.user_id, &b.key).await?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: &str, pos: i64, managed: bool, perms: u64) -> Role {
        Role { id: id.into(), name: format!("r{id}"), position: pos, managed, permissions: perms.to_string() }
    }

    #[test]
    fn score_flags_dangerous_roles_and_missing_fallback() {
        let roles = [role("1", 0, false, 0), role("2", 5, false, 0x8), role("3", 6, true, 0x8), role("4", 1, false, 0x400), role("9", 3, false, 0x1)];
        let cfg = json!({"antinuke": {"quarantine_role": "9"}, "logging": {}});
        let rows = checks(&roles, 1, 4, &cfg);
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"role_2") && !ids.contains(&"role_3") && !ids.contains(&"role_4"));
        assert!(ids.contains(&"log_fallback") && ids.contains(&"bot_position"));
        assert_eq!(rows.iter().find(|r| r["id"] == "role_2").unwrap()["fix"]["role_id"], "2");
        let rows = checks(&roles, 1, 2, &json!({"antinuke": {"quarantine_role": "9"}, "logging": {"fallback": "5"}}));
        assert!(rows.iter().any(|r| r["id"] == "quarantine_above_bot"));
    }

    #[test]
    fn strips_only_dangerous_bits() {
        assert_eq!((0x8 | 0x400u64) & !DANGEROUS, 0x400);
        assert!(ct_eq(&hash("a"), &hash("a")) && !ct_eq(&hash("a"), &hash("b")));
    }

    #[tokio::test]
    async fn claim_is_single_use() {
        let Ok(url) = std::env::var("DATABASE_URL") else { return };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_100_000_000 + std::process::id() as i64;
        sqlx::query("insert into rescue_key (guild_id, key_hash) values ($1, $2) on conflict (guild_id) do update set key_hash = $2").bind(g).bind(hash("k")).execute(&db).await.unwrap();
        assert!(matches!(claim_key(&db, g, 5, "bad").await, Err(ApiError::Forbidden(_))));
        claim_key(&db, g, 5, "k").await.unwrap();
        assert!(matches!(claim_key(&db, g, 5, "k").await, Err(ApiError::Forbidden(_))));
        let n: i64 = sqlx::query_scalar("select count(*) from extra_owners where guild_id = $1 and user_id = 5").bind(g).fetch_one(&db).await.unwrap();
        assert_eq!(n, 1);
        sqlx::query("delete from extra_owners where guild_id = $1").bind(g).execute(&db).await.unwrap();
    }
}
