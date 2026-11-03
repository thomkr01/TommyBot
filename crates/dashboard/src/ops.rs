use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::{self, Sess},
    ApiError, AppState,
};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use tommybot_antinuke::{
    security::{checks, setting_fix, strip_targets, stripped, GuildSafety, Setup},
    Inverse, RoleImg,
};

type ActionRow = (
    i64,
    i64,
    String,
    Option<i64>,
    Option<Value>,
    bool,
    chrono::DateTime<chrono::Utc>,
);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/guilds/{guild}/score", get(score))
        .route("/api/guilds/{guild}/score/fix", post(fix))
        .route("/api/guilds/{guild}/actions", get(actions))
        .route("/api/guilds/{guild}/revert", post(revert))
        .route("/api/guilds/{guild}/rescue", get(rescue_status))
        .route(
            "/api/guilds/{guild}/rescue/regenerate",
            post(rescue_regenerate),
        )
        .route("/api/rescue/claim", post(rescue_claim))
}

#[derive(Deserialize, Clone)]
pub(crate) struct Role {
    pub id: String,
    name: String,
    pub position: i64,
    #[serde(default)]
    pub managed: bool,
    permissions: String,
}

impl Role {
    pub fn perms(&self) -> u64 {
        self.permissions.parse().unwrap_or(0)
    }
}

async fn roles_of(st: &AppState, guild: i64) -> Result<Vec<Role>, ApiError> {
    Ok(serde_json::from_value(
        st.discord
            .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
            .await?,
    )
    .map_err(anyhow::Error::from)?)
}

/// Guild roles and the position of the bot's top role.
pub(crate) async fn roles_and_bot_pos(
    st: &AppState,
    guild: i64,
) -> Result<(Vec<Role>, i64), ApiError> {
    let roles = roles_of(st, guild).await?;
    let me = st.discord.call(Method::GET, "/users/@me", None).await?;
    let mine = st
        .discord
        .member_roles(guild, me["id"].as_str().unwrap_or_default())
        .await?;
    let bot_pos = roles
        .iter()
        .filter(|r| mine.contains(&r.id))
        .map(|r| r.position)
        .max()
        .unwrap_or(0);
    Ok((roles, bot_pos))
}

fn img(r: &Role) -> RoleImg {
    RoleImg {
        id: r.id.parse().unwrap_or(0),
        name: r.name.clone(),
        color: 0,
        permissions: r.perms(),
        position: r.position,
        hoist: false,
        mentionable: false,
        managed: r.managed,
    }
}

async fn score(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Viewer, false).await?;
    let (_, config) = crate::store::current(&st.db, guild).await?;
    let (roles, bot_pos) = roles_and_bot_pos(&st, guild).await?;
    let g = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}"), None)
        .await?;
    let n = |k: &str| g[k].as_u64().unwrap_or(0) as u8;
    let safety = GuildSafety {
        id: guild as u64,
        mfa: n("mfa_level"),
        verification: n("verification_level"),
        content_filter: n("explicit_content_filter"),
        notifications: n("default_message_notifications"),
    };
    let setup = Setup {
        quarantine_role: config["antinuke"]["quarantine_role"]
            .as_str()
            .and_then(|q| q.parse().ok()),
        log_fallback: config["logging"]["fallback"]
            .as_str()
            .is_some_and(|f| !f.is_empty()),
    };
    let roles: Vec<RoleImg> = roles.iter().map(img).collect();
    Ok(Json(json!(checks(&safety, &roles, bot_pos, &setup))))
}

/// One of a row's `fix`: a role id (`*` for every strippable role) or a server setting.
#[derive(Deserialize)]
struct FixBody {
    role_id: Option<String>,
    setting: Option<String>,
}

/// Role strips take a backup first, so Backups can put the old permissions back.
async fn fix(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<FixBody>,
) -> Res {
    require(&st, &s, guild, Level::Admin, true).await?;
    let (_, c) = crate::store::current(&st.db, guild).await?;
    if let Some(setting) = b.setting {
        let (key, value) =
            setting_fix(&setting).ok_or_else(|| ApiError::BadRequest("unknown setting".into()))?;
        st.discord
            .call(
                Method::PATCH,
                &format!("/guilds/{guild}"),
                Some(json!({ key: value })),
            )
            .await?;
        log_change(
            &st,
            guild,
            &c,
            format!(
                "{} (<@{}>) set `{key}` to `{value}` from the security score",
                s.username, s.user_id
            ),
        )
        .await;
        return Ok(Json(json!({"ok": true})));
    }
    let role_id = b
        .role_id
        .ok_or_else(|| ApiError::BadRequest("nothing to fix".into()))?;
    let (roles, bot_pos) = roles_and_bot_pos(&st, guild).await?;
    let roles: Vec<RoleImg> = roles.iter().map(img).collect();
    let targets = if role_id == "*" {
        strip_targets(&roles, guild as u64, bot_pos)
    } else {
        let r = roles
            .iter()
            .find(|r| r.id.to_string() == role_id)
            .ok_or(ApiError::NotFound)?;
        if r.managed {
            return Err(ApiError::BadRequest(
                "managed roles cannot be changed here".into(),
            ));
        }
        vec![r]
    };
    let channels = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/channels"), None)
        .await?;
    let roles_json = st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/roles"), None)
        .await?;
    let backup: i64 =
        sqlx::query_scalar("INSERT INTO backups (guild_id, image) VALUES ($1, $2) RETURNING id")
            .bind(guild)
            .bind(sqlx::types::Json(crate::recovery::live_image(
                guild,
                &roles_json,
                &channels,
            )))
            .fetch_one(&st.db)
            .await?;
    for r in &targets {
        st.discord
            .call(
                Method::PATCH,
                &format!("/guilds/{guild}/roles/{}", r.id),
                Some(
                    json!({"permissions": stripped(r.id, guild as u64, r.permissions).to_string()}),
                ),
            )
            .await?;
    }
    let names: Vec<&str> = targets.iter().map(|r| r.name.as_str()).collect();
    log_change(
        &st,
        guild,
        &c,
        format!(
            "{} (<@{}>) stripped dangerous permissions from role(s) `{}`, backup #{backup}",
            s.username,
            s.user_id,
            names.join("`, `")
        ),
    )
    .await;
    Ok(Json(json!({"ok": true, "backup": backup})))
}

#[derive(Deserialize)]
struct ActionsQ {
    actor: Option<i64>,
    hours: Option<i32>,
}

async fn actions(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(q): Query<ActionsQ>,
) -> Res {
    require(&st, &s, guild, Level::Editor, false).await?;
    let rows: Vec<ActionRow> = sqlx::query_as(
        "select id, actor_id, action, target_id, inverse, reverted, created_at from action_log
         where guild_id = $1 and ($2::bigint is null or actor_id = $2)
           and ($3::int is null or created_at > now() - make_interval(hours => $3))
         order by created_at desc, id desc limit 200",
    )
    .bind(guild)
    .bind(q.actor)
    .bind(q.hours)
    .fetch_all(&st.db)
    .await?;
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
        Inverse::Ban { user } => (Method::PUT, format!("/guilds/{guild}/bans/{user}"), None),
        Inverse::Timeout { user, until } => (
            Method::PATCH,
            format!("/guilds/{guild}/members/{user}"),
            Some(
                json!({"communication_disabled_until": chrono::DateTime::from_timestamp(*until, 0).map(|t| t.to_rfc3339())}),
            ),
        ),
        Inverse::DeleteChannel { id } => (Method::DELETE, format!("/channels/{id}"), None),
        Inverse::DeleteRole { id } => (Method::DELETE, format!("/guilds/{guild}/roles/{id}"), None),
        Inverse::DeleteWebhook { id } => (Method::DELETE, format!("/webhooks/{id}"), None),
        Inverse::RestoreRoles { user, roles } => (
            Method::PATCH,
            format!("/guilds/{guild}/members/{user}"),
            Some(json!({"roles": roles.iter().map(u64::to_string).collect::<Vec<_>>()})),
        ),
        Inverse::Unquarantine { .. } | Inverse::Reinvite { .. } => {
            unreachable!("handled by the caller")
        }
    };
    st.discord.call(m, &path, body).await.map(drop)
}

async fn revert(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<RevertBody>,
) -> Res {
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
            Err(e) => {
                failed.push(json!({"id": id, "error": e.to_string()}));
                continue;
            }
        };
        match &inv {
            Inverse::Unquarantine { .. } => {
                skipped.push(id);
                continue;
            }
            Inverse::Reinvite { user } => reinvite.push(user.to_string()),
            _ => {
                if let Err(e) = run_inverse(&st, guild, &inv).await {
                    failed.push(json!({"id": id, "error": e.to_string()}));
                    continue;
                }
            }
        }
        sqlx::query("update action_log set reverted = true where id = $1")
            .bind(id)
            .execute(&st.db)
            .await?;
        done += 1;
    }
    {
        let (_, c) = crate::store::current(&st.db, guild).await?;
        log_change(
            &st,
            guild,
            &c,
            format!(
                "{} (<@{}>) reverted {done} action(s)",
                s.username, s.user_id
            ),
        )
        .await;
    }
    Ok(Json(
        json!({"done": done, "failed": failed, "skipped": skipped, "reinvite": reinvite}),
    ))
}

fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn rescue_status(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, false).await?;
    let has_key: bool =
        sqlx::query_scalar("select exists(select 1 from rescue_key where guild_id = $1)")
            .bind(guild)
            .fetch_one(&st.db)
            .await?;
    Ok(Json(json!({"has_key": has_key})))
}

async fn rescue_regenerate(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    require(&st, &s, guild, Level::Owner, true).await?;
    let key = auth::token();
    let secret = crate::otp::new_secret();
    sqlx::query(
        "insert into rescue_key (guild_id, key_hash, totp_secret) values ($1, $2, $3)
                 on conflict (guild_id) do update set key_hash = $2, totp_secret = $3, created_at = now()",
    )
    .bind(guild)
    .bind(hash(&key))
    .bind(&secret)
    .execute(&st.db)
    .await?;
    let uri = crate::otp::uri(&secret, &format!("rescue {guild}"))?;
    Ok(Json(json!({"key": key, "uri": uri})))
}

/// Either the rescue key or a current 6-digit code from the key's authenticator.
#[derive(Deserialize)]
struct Claim {
    guild_id: String,
    key: Option<String>,
    code: Option<String>,
}

/// Claim attempts per user in the last 10 minutes.
static CLAIMS: LazyLock<Mutex<HashMap<i64, Vec<Instant>>>> = LazyLock::new(Default::default);
const MAX_CLAIMS: usize = 5;

fn claim_allowed(user: i64) -> bool {
    let mut m = CLAIMS.lock().unwrap();
    let v = m.entry(user).or_default();
    v.retain(|t| t.elapsed() < Duration::from_secs(600));
    v.push(Instant::now());
    v.len() <= MAX_CLAIMS
}

async fn claim_key(
    db: &sqlx::PgPool,
    guild: i64,
    user: i64,
    (key, code): (Option<&str>, Option<&str>),
) -> Result<(), ApiError> {
    let mut tx = db.begin().await?;
    // The delete is the single-use gate: a concurrent claim sees zero rows.
    let ok = match (key, code) {
        (Some(key), _) => {
            sqlx::query("delete from rescue_key where guild_id = $1 and key_hash = $2")
                .bind(guild)
                .bind(hash(key))
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0
        }
        (None, Some(code)) => {
            let secret: Option<Option<String>> = sqlx::query_scalar(
                "select totp_secret from rescue_key where guild_id = $1 for update",
            )
            .bind(guild)
            .fetch_optional(&mut *tx)
            .await?;
            match secret.flatten() {
                Some(s) if crate::otp::step(&s, code)?.is_some() => {
                    sqlx::query("delete from rescue_key where guild_id = $1")
                        .bind(guild)
                        .execute(&mut *tx)
                        .await?;
                    true
                }
                _ => false,
            }
        }
        _ => false,
    };
    if !ok {
        return Err(ApiError::Forbidden("invalid rescue key or code"));
    }
    sqlx::query(
        "insert into extra_owners (guild_id, user_id) values ($1, $2) on conflict do nothing",
    )
    .bind(guild)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn rescue_claim(State(st): State<AppState>, s: Sess, Json(b): Json<Claim>) -> Res {
    let guild: i64 = b
        .guild_id
        .trim()
        .parse()
        .map_err(|_| ApiError::BadRequest("bad guild id".into()))?;
    if !claim_allowed(s.user_id) {
        return Err(ApiError::Forbidden(
            "too many attempts, try again in 10 minutes",
        ));
    }
    let (_, config) = crate::store::current(&st.db, guild).await?;
    let r = claim_key(
        &st.db,
        guild,
        s.user_id,
        (
            b.key.as_deref().map(str::trim),
            b.code.as_deref().map(str::trim),
        ),
    )
    .await;
    let what = if r.is_ok() {
        "claimed Extra Owner with the rescue key"
    } else {
        "failed a rescue claim"
    };
    log_change(
        &st,
        guild,
        &config,
        format!("{} (<@{}>) {what}", s.username, s.user_id),
    )
    .await;
    r?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authenticator_code_claims_once() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_200_000_000 + std::process::id() as i64;
        let secret = crate::otp::new_secret();
        sqlx::query("insert into rescue_key (guild_id, key_hash, totp_secret) values ($1, 'x', $2) on conflict (guild_id) do update set totp_secret = $2")
            .bind(g).bind(&secret).execute(&db).await.unwrap();
        assert!(claim_key(&db, g, 6, (None, Some("000000"))).await.is_err());
        let code = crate::otp::current(&secret);
        claim_key(&db, g, 6, (None, Some(&code))).await.unwrap();
        assert!(claim_key(&db, g, 6, (None, Some(&code))).await.is_err());
        sqlx::query("delete from extra_owners where guild_id = $1")
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn claim_is_single_use() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let g = 9_100_000_000 + std::process::id() as i64;
        sqlx::query("insert into rescue_key (guild_id, key_hash) values ($1, $2) on conflict (guild_id) do update set key_hash = $2").bind(g).bind(hash("k")).execute(&db).await.unwrap();
        assert!(matches!(
            claim_key(&db, g, 5, (Some("bad"), None)).await,
            Err(ApiError::Forbidden(_))
        ));
        claim_key(&db, g, 5, (Some("k"), None)).await.unwrap();
        assert!(matches!(
            claim_key(&db, g, 5, (Some("k"), None)).await,
            Err(ApiError::Forbidden(_))
        ));
        let n: i64 = sqlx::query_scalar(
            "select count(*) from extra_owners where guild_id = $1 and user_id = 5",
        )
        .bind(g)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, 1);
        sqlx::query("delete from extra_owners where guild_id = $1")
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
    }
}
