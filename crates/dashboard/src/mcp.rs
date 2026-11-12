//! Writes from the MCP server (specs/mcp-plan.md, PR 4). The MCP server proposes a change with a token
//! made on the MCP tokens page, the bot DMs the owner what it does and a one-time code, and the change runs
//! only when that code comes back. Claude never sees the code, so text from Discord cannot talk it into a
//! change. The change then runs through the normal dashboard route as the bot owner, so every check there
//! (level, TOTP step-up, versions, limits) applies.
use crate::{access, api::Res, auth::Sess, diff, store, ApiError, AppState};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, Request},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const TTL_MINUTES: i32 = 5;
/// Wrong codes before a change is closed.
const MAX_TRIES: i32 = 3;
/// Open changes at once, so a looping client cannot flood the owner's DMs.
const MAX_OPEN: i64 = 5;
/// Live tokens at once; one per client is the idea.
const MAX_TOKENS: i64 = 10;

/// Everything the MCP server can reach: name, method and path below `/api/guilds/{guild}/`. Update,
/// purge, backups and antinuke revert are left out on purpose (Thomas, 2026-11-10).
pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("config_section", "PUT", "config/{t}"),
    ("permits", "PUT", "permits"),
    ("rollback", "POST", "history/{t}/rollback"),
    ("lockdown_lock", "POST", "lockdown/lock"),
    ("lockdown_unlock", "POST", "lockdown/unlock"),
    ("note_add", "POST", "notes"),
    ("note_edit", "POST", "notes/{t}"),
    ("case_edit", "POST", "cases/{t}"),
    ("embed_save", "POST", "embeds"),
    ("embed_post", "POST", "embeds/{t}/post"),
];

pub fn routes() -> Router<AppState> {
    let service = Router::new()
        .route("/api/mcp/propose", post(propose))
        .route("/api/mcp/confirm", post(confirm))
        .route("/api/mcp/score/{guild}", get(score))
        // The MCP server calls straight from the compose network; anything a proxy (Cloudflare or another)
        // forwarded is refused. Defence in depth only: the hashed token is the real guard.
        .route_layer(axum::middleware::from_fn(
            |req: Request<Body>, next: axum::middleware::Next| async move {
                let h = req.headers();
                if [
                    "cf-connecting-ip",
                    "x-forwarded-for",
                    "forwarded",
                    "x-real-ip",
                ]
                .iter()
                .any(|k| h.contains_key(*k))
                {
                    return ApiError::NotFound.into_response();
                }
                next.run(req).await
            },
        ));
    Router::new()
        .merge(service)
        .route("/api/mcp-tokens", get(tokens).post(token_create))
        .route("/api/mcp-tokens/{id}/revoke", post(token_revoke))
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::BadRequest(m.into())
}

// ---- Token management (dashboard session, bot owner) ----

/// Only the bot owner manages tokens; making or revoking one also needs a fresh 2FA code.
async fn owner(st: &AppState, s: &Sess, step_up: bool) -> Result<(), ApiError> {
    if !crate::update::is_bot_owner(st, s).await {
        return Err(ApiError::Forbidden(
            "only the bot owner can manage MCP tokens",
        ));
    }
    if step_up && !s.stepped_up {
        return Err(ApiError::StepUp);
    }
    Ok(())
}

pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token).to_vec()
}

async fn tokens(State(st): State<AppState>, s: Sess) -> Res {
    owner(&st, &s, false).await?;
    let rows: Option<Value> = sqlx::query_scalar(
        "select jsonb_agg(jsonb_build_object('id', id, 'name', name, 'created_at', created_at,
            'last_used_at', last_used_at, 'revoked_at', revoked_at) order by revoked_at nulls first, id desc)
         from mcp_tokens",
    )
    .fetch_one(&st.db)
    .await?;
    Ok(Json(rows.unwrap_or_else(|| json!([]))))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

/// The token is in this answer only; the database keeps its hash.
async fn token_create(State(st): State<AppState>, s: Sess, Json(b): Json<NewToken>) -> Res {
    owner(&st, &s, true).await?;
    let name = b.name.trim();
    if name.is_empty()
        || name.chars().count() > 40
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
    {
        return Err(bad(
            "name: 1 to 40 letters, digits, spaces, dots, dashes or underscores",
        ));
    }
    // Count and insert under one lock, or parallel requests all see room and pass the cap.
    let mut tx = st.db.begin().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('mcp_tokens:', 0))")
        .execute(&mut *tx)
        .await?;
    let live: i64 = sqlx::query_scalar("select count(*) from mcp_tokens where revoked_at is null")
        .fetch_one(&mut *tx)
        .await?;
    if live >= MAX_TOKENS {
        return Err(bad(format!(
            "{live} tokens are live; revoke one you no longer use first"
        )));
    }
    let token = format!("tbmcp_{}", crate::auth::token());
    let id: i64 = sqlx::query_scalar(
        "insert into mcp_tokens (name, token_hash, created_by) values ($1, $2, $3) returning id",
    )
    .bind(name)
    .bind(token_hash(&token))
    .bind(s.user_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(id, name, "mcp token created");
    Ok(Json(json!({"id": id, "name": name, "token": token})))
}

/// Revoking also closes the changes this token proposed that still wait for a code.
async fn token_revoke(State(st): State<AppState>, s: Sess, Path(id): Path<i64>) -> Res {
    owner(&st, &s, true).await?;
    let mut tx = st.db.begin().await?;
    let done = sqlx::query(
        "update mcp_tokens set revoked_at = now() where id = $1 and revoked_at is null",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if done == 0 {
        return Err(ApiError::NotFound);
    }
    sqlx::query("update mcp_writes set status = 'revoked', body = null where token_id = $1 and status = 'pending'")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(id, "mcp token revoked");
    Ok(Json(json!({"ok": true})))
}

// ---- Calls from the MCP server ----

/// The token that called, and the name it gets in the audit trail and the DM.
struct Client {
    token_id: i64,
    name: String,
}

/// A live token from the MCP tokens page. Only its hash is stored and the lookup is by hash, so the time
/// it takes says nothing about the token. `x-mcp-via` (which login reached the MCP server) is only a label.
async fn service(st: &AppState, h: &HeaderMap) -> Result<Client, ApiError> {
    let got = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.len() >= 32)
        .ok_or(ApiError::Unauthorized)?;
    let (token_id, name): (i64, String) = sqlx::query_as(
        "update mcp_tokens set last_used_at = now() where token_hash = $1 and revoked_at is null returning id, name",
    )
    .bind(token_hash(got))
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| {
        tracing::warn!("mcp call refused: unknown or revoked token");
        ApiError::Unauthorized
    })?;
    // Expired changes lose their body on the next call, so no request text outlives its 5 minutes by much.
    sqlx::query("update mcp_writes set status = 'expired', body = null where status = 'pending' and expires_at < now()")
        .execute(&st.db)
        .await?;
    let via: String = h
        .get("x-mcp-via")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ' '))
        .take(20)
        .collect();
    let name = if via.is_empty() {
        name
    } else {
        format!("{name} via {via}")
    };
    Ok(Client { token_id, name })
}

/// Method and path for an action. `target` is a section name or an id, never free text in the path.
fn route(action: &str, guild: i64, target: Option<&str>) -> Result<(Method, String), ApiError> {
    let (_, m, p) = ACTIONS
        .iter()
        .find(|a| a.0 == action)
        .ok_or_else(|| bad(format!("unknown action: {action}")))?;
    let p = if p.contains("{t}") {
        let t = target
            .filter(|t| {
                !t.is_empty()
                    && t.len() <= 32
                    && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .ok_or_else(|| bad("target is required: a section name or an id"))?;
        if action == "config_section" && !access::SECTIONS.contains(&t) {
            return Err(bad(format!("unknown section: {t}")));
        }
        p.replace("{t}", t)
    } else {
        p.to_string()
    };
    Ok((m.parse().unwrap(), format!("/api/guilds/{guild}/{p}")))
}

fn code_hash(id: i64, code: &str) -> Vec<u8> {
    Sha256::digest(format!("{id}:{code}")).to_vec()
}

/// What the change does: a diff for config, permits and rollback, the request itself for the rest.
async fn preview(
    st: &AppState,
    guild: i64,
    action: &str,
    target: Option<&str>,
    body: &Value,
) -> Result<Value, ApiError> {
    let (_, config) = store::current(&st.db, guild).await?;
    Ok(match action {
        "config_section" => json!(diff::diff(
            &config[target.unwrap_or_default()],
            &body["value"]
        )),
        "permits" => {
            let mut c = st.db.acquire().await?;
            json!(diff::diff(
                &store::read_permits(&mut c, guild).await?,
                &body["value"]
            ))
        }
        "rollback" => {
            let v = target.and_then(|t| t.parse().ok()).unwrap_or(-1);
            // Live permits, not the copy in the last version: a rescue claim adds an Extra Owner without one.
            let mut now = config;
            let mut c = st.db.acquire().await?;
            now["permits"] = store::read_permits(&mut c, guild).await?;
            json!(diff::diff(&now, &crate::api::version(st, guild, v).await?))
        }
        // What gets posted is the stored embed, not the request, so the DM shows that.
        "embed_post" => {
            let id: i64 = target.and_then(|t| t.parse().ok()).unwrap_or(-1);
            let (name, embed, channel): (String, Value, Option<i64>) = sqlx::query_as(
                "select name, embed, channel_id from embeds where id = $1 and guild_id = $2",
            )
            .bind(id)
            .bind(guild)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| bad("no embed with this id in this server"))?;
            json!({"name": name, "channel": channel.map(|c| c.to_string()), "embed": embed, "repost": body["repost"]})
        }
        _ => body.clone(),
    })
}

/// The most a DM shows of a change. Discord allows 2000 characters per message, and the rest is the header.
const DM_MAX: usize = 1500;

/// The change as the DM shows it: one line per changed path, or the request itself.
fn lines(changes: &Value) -> String {
    match changes.as_array() {
        Some(a) if a.is_empty() => "(no change)".to_string(),
        Some(a) => a
            .iter()
            .map(|c| {
                format!(
                    "{}: {} -> {}",
                    c["path"].as_str().unwrap_or("?"),
                    c["old"],
                    c["new"]
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => changes.to_string(),
    }
}

fn preview_hash(changes: &Value) -> Vec<u8> {
    Sha256::digest(changes.to_string()).to_vec()
}

/// Strips what would let a name or value break out of its markup in the DM.
fn plain(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '*' | '`' | '_' | '~' | '|' | '<' | '@' | '\n'))
        .collect()
}

/// `lines` is never cut: the owner approves only what the DM shows, so `propose` refuses a longer change.
fn dm_text(
    client: &str,
    id: i64,
    guild: i64,
    action: &str,
    target: Option<&str>,
    lines: &str,
    code: &str,
) -> String {
    let what = match target {
        Some(t) => format!("{action} {t}"),
        None => action.to_string(),
    };
    let fence = "`".repeat(3);
    format!(
        "**Claude wants to make a change** (#{id}, token {}): `{what}` in server {guild}.\n{fence}\n{}\n{fence}\nCode: **{code}** (valid {TTL_MINUTES} min). Give it to Claude only if this is what you want. If you did not ask for this, revoke the token on the dashboard's MCP tokens page.",
        plain(client),
        lines.replace(&fence, "'''")
    )
}

#[derive(Deserialize)]
struct Propose {
    guild_id: String,
    action: String,
    target: Option<String>,
    #[serde(default)]
    body: Value,
}

async fn propose(State(st): State<AppState>, h: HeaderMap, Json(p): Json<Propose>) -> Res {
    let client = service(&st, &h).await?;
    let guild: i64 = p
        .guild_id
        .parse()
        .map_err(|_| bad("guild_id is not an id"))?;
    // Only actions with `{t}` in their path take a target, and only those check it, so drop any other:
    // it would reach the DM and the audit row unchecked.
    let takes_target = ACTIONS
        .iter()
        .any(|a| a.0 == p.action && a.2.contains("{t}"));
    let target = p
        .target
        .as_deref()
        .filter(|t| takes_target && !t.is_empty());
    route(&p.action, guild, target)?;
    let body = if p.body.is_null() { json!({}) } else { p.body };
    let changes = preview(&st, guild, &p.action, target, &body).await?;
    let shown = lines(&changes);
    let n = shown.chars().count();
    if n > DM_MAX {
        return Err(bad(format!(
            "this change is {n} characters as a DM, more than the {DM_MAX} the owner can review in one: split it into smaller proposals"
        )));
    }
    let owner: i64 = st.discord.bot_owner().await?.parse()?;
    let code = format!("{:06}", rand::random_range(0..1_000_000));
    // Count and insert under one lock, or parallel proposals all see room and pass the cap.
    let mut tx = st.db.begin().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('mcp_writes:', 0))")
        .execute(&mut *tx)
        .await?;
    let open: i64 = sqlx::query_scalar("select count(*) from mcp_writes where status = 'pending'")
        .fetch_one(&mut *tx)
        .await?;
    if open >= MAX_OPEN {
        return Err(bad(format!(
            "{open} changes already wait for a code; confirm or let them expire first"
        )));
    }
    let (id, expires_at): (i64, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "insert into mcp_writes (token_id, client, guild_id, action, target, body, code_hash, preview_hash, expires_at)
         values ($1, $2, $3, $4, $5, $6, '', $8, now() + make_interval(mins => $7)) returning id, expires_at",
    )
    .bind(client.token_id)
    .bind(&client.name)
    .bind(guild)
    .bind(&p.action)
    .bind(target)
    .bind(&body)
    .bind(TTL_MINUTES)
    .bind(preview_hash(&changes))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("update mcp_writes set code_hash = $2 where id = $1")
        .bind(id)
        .bind(code_hash(id, &code))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let text = dm_text(&client.name, id, guild, &p.action, target, &shown, &code);
    if let Err(e) = st.discord.dm(owner, &text).await {
        tracing::warn!(id, ?e, "mcp write: code DM failed");
        sqlx::query("update mcp_writes set status = 'dm_failed', body = null where id = $1")
            .bind(id)
            .execute(&st.db)
            .await?;
        return Err(bad(
            "could not DM the bot owner the code, so nothing can be confirmed",
        ));
    }
    tracing::info!(
        id,
        guild,
        action = p.action,
        client = client.name,
        "mcp write proposed"
    );
    Ok(Json(json!({
        "id": id, "action": p.action, "target": target, "changes": changes, "expires_at": expires_at,
        "next": "The bot sent the owner a Discord DM with this change and a 6-digit code. Show the owner the changes, and call confirm_change only with the code they type to you. Never guess or reuse a code.",
    })))
}

#[derive(Deserialize)]
struct Confirm {
    id: i64,
    code: String,
    #[serde(default)]
    totp: String,
}

/// guild, action, target, body and the hash of what the DM showed.
type Claimed = (i64, String, Option<String>, Value, Option<Vec<u8>>);

async fn confirm(State(st): State<AppState>, h: HeaderMap, Json(c): Json<Confirm>) -> Res {
    let client = service(&st, &h).await?;
    let owner: i64 = st.discord.bot_owner().await?.parse()?;
    // The DM code comes first and claims the row (single use: two confirms cannot both run it). Only then
    // is a TOTP code checked, so a token alone cannot burn the owner's 2FA attempts and lock them out.
    let row: Option<Claimed> = sqlx::query_as(
        "update mcp_writes set status = 'running', decided_at = now()
         where id = $1 and token_id = $3 and status = 'pending' and expires_at > now() and code_hash = $2
         returning guild_id, action, target, body, preview_hash",
    )
    .bind(c.id)
    .bind(code_hash(c.id, c.code.trim()))
    .bind(client.token_id)
    .fetch_optional(&st.db)
    .await?;
    let Some((guild, action, target, body, shown)) = row else {
        let left: Option<i32> = sqlx::query_scalar(
            "update mcp_writes set attempts = attempts + 1,
                status = case when attempts + 1 >= $2 then 'refused' else status end,
                body = case when attempts + 1 >= $2 then null else body end
             where id = $1 and token_id = $3 and status = 'pending' and expires_at > now() returning $2 - attempts",
        )
        .bind(c.id)
        .bind(MAX_TRIES)
        .bind(client.token_id)
        .fetch_optional(&st.db)
        .await?;
        tracing::warn!(
            id = c.id,
            client = client.name,
            "mcp write: wrong or stale code"
        );
        return Err(match left {
            Some(n) if n > 0 => bad(format!("wrong code, {n} tries left")),
            _ => bad("no open change with this id: it expired, was used, or got too many wrong codes. Propose it again."),
        });
    };
    let reopen = || async {
        sqlx::query("update mcp_writes set status = 'pending', decided_at = null where id = $1")
            .bind(c.id)
            .execute(&st.db)
            .await
    };
    let stepped_up = !c.totp.trim().is_empty();
    if stepped_up {
        if let Err(e) = crate::auth::check_totp(&st, owner, c.totp.trim()).await {
            reopen().await?;
            return Err(e);
        }
    }
    // Only what the owner reviewed may run: the stored embed, a live permit or the config may have moved since.
    let now = preview(&st, guild, &action, target.as_deref(), &body).await;
    if shown.is_none_or(|h| now.as_ref().map(preview_hash).ok() != Some(h)) {
        sqlx::query("update mcp_writes set status = 'stale', body = null where id = $1")
            .bind(c.id)
            .execute(&st.db)
            .await?;
        return Err(bad("the server changed since this was proposed, so it is not what the owner reviewed: propose it again"));
    }
    let (method, path) = route(&action, guild, target.as_deref())?;
    let (status, out) = match run(&st, owner, stepped_up, method, &path, &body).await {
        Ok(r) => r,
        Err(e) => {
            sqlx::query("update mcp_writes set status = 'failed', body = null where id = $1")
                .bind(c.id)
                .execute(&st.db)
                .await?;
            return Err(e);
        }
    };
    if out["error"] == "step_up_required" {
        reopen().await?;
        return Err(bad("this change also needs a code from the owner's authenticator app: ask for it and confirm again with the same code and totp"));
    }
    let state = if (200..300).contains(&status) {
        "done"
    } else {
        "failed"
    };
    sqlx::query("update mcp_writes set status = $2, http_status = $3, body = null where id = $1")
        .bind(c.id)
        .bind(state)
        .bind(status as i32)
        .execute(&st.db)
        .await?;
    tracing::info!(
        id = c.id,
        guild,
        action,
        status,
        client = client.name,
        "mcp write {state}"
    );
    Ok(Json(
        json!({"status": status, "ok": state == "done", "result": out}),
    ))
}

/// The security score, read through the dashboard like the page does.
async fn score(State(st): State<AppState>, h: HeaderMap, Path(guild): Path<i64>) -> Res {
    let client = service(&st, &h).await?;
    tracing::info!(guild, client = client.name, "mcp score");
    let owner: i64 = st.discord.bot_owner().await?.parse()?;
    let (status, out) = run(
        &st,
        owner,
        false,
        Method::GET,
        &format!("/api/guilds/{guild}/score"),
        &Value::Null,
    )
    .await?;
    Ok(Json(json!({"status": status, "result": out})))
}

/// One request through the dashboard's own router, as the bot owner.
async fn run(
    st: &AppState,
    owner: i64,
    stepped_up: bool,
    method: Method,
    path: &str,
    body: &Value,
) -> Result<(u16, Value), ApiError> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })?;
    req.extensions_mut().insert(Sess {
        id: String::new(),
        user_id: owner,
        username: "Claude (MCP)".into(),
        access_token: String::new(),
        stepped_up,
        csrf: String::new(),
    });
    let res = crate::router(st.clone(), "/nonexistent")
        .oneshot(req)
        .await
        .unwrap_or_else(|e| match e {});
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_routes_and_never_the_forbidden_ones() {
        for (name, m, p) in ACTIONS {
            assert!(matches!(*m, "PUT" | "POST"), "{name}");
            for no in [
                "update", "purge", "backup", "revert", "rescue", "panic", "delete", "import", "mcp",
            ] {
                assert!(!p.contains(no), "{name} reaches {no}");
            }
        }
        assert_eq!(
            route("config_section", 1, Some("automod")).unwrap().1,
            "/api/guilds/1/config/automod"
        );
        assert_eq!(
            route("rollback", 1, Some("7")).unwrap().1,
            "/api/guilds/1/history/7/rollback"
        );
        assert!(route("config_section", 1, Some("nope")).is_err());
        assert!(route("note_edit", 1, Some("../../update")).is_err());
        assert!(route("note_edit", 1, Some("a/b")).is_err());
        assert!(route("note_edit", 1, None).is_err());
        assert!(route("update", 1, None).is_err());
        // A target on an action without one is ignored, not appended.
        assert_eq!(
            route("permits", 1, Some("x")).unwrap().1,
            "/api/guilds/1/permits"
        );
    }

    #[test]
    fn dm_fits_discord_and_keeps_its_markup() {
        let full = json!([{"path": "a", "old": "x".repeat(DM_MAX - 20), "new": "```"}]);
        let shown = lines(&full);
        assert!(shown.chars().count() <= DM_MAX);
        let t = dm_text(
            &"Mac**`@everyone".repeat(5),
            i64::MAX,
            i64::MAX,
            "config_section",
            Some("discord_automod"),
            &shown,
            "123456",
        );
        assert!(t.chars().count() <= 2000 && t.contains("**123456**"));
        assert!(t.contains(&"x".repeat(DM_MAX - 20)), "never cut");
        assert_eq!(t.matches("```").count(), 2);
        assert!(t.contains("token Maceveryone"));
        assert_eq!(lines(&json!([])), "(no change)");
        assert_ne!(
            preview_hash(&json!({"embed": {"title": "a"}})),
            preview_hash(&json!({"embed": {"title": "b"}}))
        );
    }

    /// The token cap is global, so tests that make tokens run one at a time.
    static TOKENS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Runs only when DATABASE_URL points at a scratch Postgres. Discord is a local mock.
    #[tokio::test]
    async fn change_needs_a_live_token_the_dm_code_once_and_totp_where_the_page_does() {
        use std::sync::{Arc, Mutex};
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _one = TOKENS.lock().await;
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let owner = 7_700_000_000 + std::process::id() as i64;
        let g = 9_450_000_000 + std::process::id() as i64;
        let dms: Arc<Mutex<Vec<String>>> = Default::default();
        let sent = dms.clone();
        let mock = Router::new()
            .route(
                "/applications/@me",
                get(move || async move { Json(json!({"owner": {"id": owner.to_string()}})) }),
            )
            .route(
                "/guilds/{g}",
                get(move || async move { Json(json!({"owner_id": owner.to_string()})) }),
            )
            .route(
                "/users/@me/channels",
                post(|| async { Json(json!({"id": "500"})) }),
            )
            .route(
                "/channels/500/messages",
                post(move |Json(b): Json<Value>| async move {
                    sent.lock()
                        .unwrap()
                        .push(b["content"].as_str().unwrap().to_string());
                    Json(json!({"id": "1"}))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, mock).await });
        let st = AppState {
            db: db.clone(),
            discord: crate::discord::Discord {
                http: reqwest::Client::builder().no_proxy().build().unwrap(),
                client_id: String::new(),
                client_secret: String::new(),
                bot_token: String::new(),
                redirect_uri: String::new(),
                api,
                memo: Default::default(),
            },
            secure: false,
            totp: Default::default(),
            rate: Default::default(),
            ips: Default::default(),
            vpn_logged: Default::default(),
        };
        // A call as the MCP server (auth = token) or as a dashboard session (sess).
        let send = |path: String, auth: Option<String>, sess: Option<Sess>, body: Value| {
            let mut req = Request::post(path).header("content-type", "application/json");
            if let Some(a) = auth {
                req = req.header("authorization", format!("Bearer {a}"));
            }
            let mut req = req.body(Body::from(body.to_string())).unwrap();
            if let Some(s) = sess {
                req.extensions_mut().insert(s);
            }
            let app = crate::router(st.clone(), "/nonexistent");
            async move {
                let res = app.oneshot(req).await.unwrap();
                let code = res.status().as_u16();
                let b = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (
                    code,
                    serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
                )
            }
        };
        let sess = |user: i64, stepped_up: bool| Sess {
            id: String::new(),
            user_id: user,
            username: "t".into(),
            access_token: String::new(),
            stepped_up,
            csrf: String::new(),
        };

        // Making a token: bot owner only, and only after 2FA. The token is shown once and not stored.
        let make = json!({"name": "Mac"});
        let p = |s: &str| s.to_string();
        assert_eq!(
            send(
                p("/api/mcp-tokens"),
                None,
                Some(sess(owner + 1, true)),
                make.clone()
            )
            .await
            .0,
            403
        );
        let (code, r) = send(
            p("/api/mcp-tokens"),
            None,
            Some(sess(owner, false)),
            make.clone(),
        )
        .await;
        assert_eq!((code, r["error"].as_str()), (403, Some("step_up_required")));
        let (code, r) = send(p("/api/mcp-tokens"), None, Some(sess(owner, true)), make).await;
        assert_eq!(code, 200, "{r}");
        let tok = r["token"].as_str().unwrap().to_string();
        let tok_id = r["id"].as_i64().unwrap();
        let stored: Vec<u8> = sqlx::query_scalar("select token_hash from mcp_tokens where id = $1")
            .bind(tok_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, token_hash(&tok));
        assert!(!String::from_utf8_lossy(&stored).contains(&tok[6..]));

        let call =
            |path: &str, auth: &str, body: Value| send(path.into(), Some(auth.into()), None, body);
        let last_code = || {
            let d = dms.lock().unwrap();
            let t = d.last().unwrap();
            t.split("**").nth(3).unwrap().to_string()
        };

        let section = json!({"guild_id": g.to_string(), "action": "config_section", "target": "joingate",
            "body": {"value": {"enabled": true}, "expected_version": 0}});
        assert_eq!(
            call("/api/mcp/propose", &"w".repeat(40), section.clone())
                .await
                .0,
            401
        );
        let from_net = Request::post("/api/mcp/propose")
            .header("authorization", format!("Bearer {tok}"))
            .header("x-forwarded-for", "1.2.3.4")
            .body(Body::empty())
            .unwrap();
        let res = crate::router(st.clone(), "/nonexistent")
            .oneshot(from_net)
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "not reachable through the tunnel");
        let (code, pr) = call("/api/mcp/propose", &tok, section).await;
        assert_eq!(code, 200, "{pr}");
        assert!(
            pr["changes"].as_array().is_some_and(|c| !c.is_empty()),
            "{pr}"
        );
        assert!(
            !pr.to_string().contains(&last_code()),
            "the code never goes back to Claude"
        );
        assert!(dms.lock().unwrap().last().unwrap().contains("token Mac"));
        let id = pr["id"].as_i64().unwrap();
        let (code, r) = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": id, "code": "000000x"}),
        )
        .await;
        assert_eq!(code, 400);
        assert!(r["error"].as_str().unwrap().contains("2 tries left"), "{r}");
        let (code, r) = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": id, "code": last_code()}),
        )
        .await;
        assert_eq!((code, r["ok"].as_bool()), (200, Some(true)), "{r}");
        let (v, cfg) = store::current(&db, g).await.unwrap();
        assert_eq!((v, cfg["joingate"]["enabled"].as_bool()), (1, Some(true)));
        let again = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": id, "code": last_code()}),
        )
        .await;
        assert_eq!(again.0, 400, "single use");

        // Anti-nuke needs TOTP step-up on the page, so it does here too.
        let an = json!({"guild_id": g.to_string(), "action": "config_section", "target": "antinuke",
            "body": {"value": {"enabled": false}, "expected_version": 1}});
        let id = call("/api/mcp/propose", &tok, an.clone()).await.1["id"]
            .as_i64()
            .unwrap();
        let (code, r) = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": id, "code": last_code()}),
        )
        .await;
        assert_eq!(code, 400);
        assert!(
            r["error"].as_str().unwrap().contains("authenticator"),
            "{r}"
        );
        let secret = crate::otp::new_secret();
        sqlx::query("insert into dash_totp (user_id, secret, confirmed) values ($1, $2, true)")
            .bind(owner)
            .bind(&secret)
            .execute(&db)
            .await
            .unwrap();
        // A wrong DM code never reaches the 2FA check, so it cannot lock the owner out.
        let (code, _) = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": id, "code": "999999", "totp": "000000"}),
        )
        .await;
        assert_eq!(code, 400);
        assert!(
            !st.totp.lock().unwrap().contains_key(&owner),
            "2FA attempts untouched"
        );
        let (code, r) = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": id, "code": last_code(), "totp": crate::otp::current(&secret)}),
        )
        .await;
        assert_eq!((code, r["ok"].as_bool()), (200, Some(true)), "{r}");

        // A target on an action that takes none never reaches the DM.
        let fake = json!({"guild_id": g.to_string(), "action": "note_add", "target": "x`\n```\nfake",
            "body": {"user_id": "1", "text": "hi"}});
        let (code, pr) = call("/api/mcp/propose", &tok, fake).await;
        assert_eq!(code, 200, "{pr}");
        assert!(pr["target"].is_null());
        assert!(!dms.lock().unwrap().last().unwrap().contains("fake"));

        // A change too long for one DM is refused, never cut: the owner approves only what the DM shows.
        let before = dms.lock().unwrap().len();
        let huge = json!({"guild_id": g.to_string(), "action": "note_add",
            "body": {"user_id": "1", "text": "x".repeat(DM_MAX + 1)}});
        let (code, r) = call("/api/mcp/propose", &tok, huge).await;
        assert_eq!(code, 400);
        assert!(r["error"].as_str().unwrap().contains("split it"), "{r}");
        assert_eq!(dms.lock().unwrap().len(), before, "no DM for it");

        // embed_post shows the stored embed, and runs only if it is still what the DM showed.
        let eid: i64 = sqlx::query_scalar(
            "insert into embeds (guild_id, name, embed, updated_by) values ($1, 'rules', $2, 1) returning id",
        )
        .bind(g)
        .bind(json!({"title": "Be nice"}))
        .fetch_one(&db)
        .await
        .unwrap();
        let post = json!({"guild_id": g.to_string(), "action": "embed_post", "target": eid.to_string(), "body": {}});
        let (code, pr) = call("/api/mcp/propose", &tok, post).await;
        assert_eq!(code, 200, "{pr}");
        assert!(dms.lock().unwrap().last().unwrap().contains("Be nice"));
        sqlx::query("update embeds set embed = $2 where id = $1")
            .bind(eid)
            .bind(json!({"title": "@everyone free nitro"}))
            .execute(&db)
            .await
            .unwrap();
        let (code, r) = call(
            "/api/mcp/confirm",
            &tok,
            json!({"id": pr["id"], "code": last_code()}),
        )
        .await;
        assert_eq!(code, 400);
        assert!(
            r["error"].as_str().unwrap().contains("propose it again"),
            "{r}"
        );

        // Revoking closes the token and what it still has waiting.
        let id = call("/api/mcp/propose", &tok, an).await.1["id"]
            .as_i64()
            .unwrap();
        let revoke = format!("/api/mcp-tokens/{tok_id}/revoke");
        assert_eq!(
            send(revoke.clone(), None, Some(sess(owner, false)), json!({}))
                .await
                .0,
            403
        );
        assert_eq!(
            send(revoke, None, Some(sess(owner, true)), json!({}))
                .await
                .0,
            200
        );
        assert_eq!(
            call(
                "/api/mcp/confirm",
                &tok,
                json!({"id": id, "code": last_code()})
            )
            .await
            .0,
            401
        );
        let mut get = Request::get("/api/mcp-tokens").body(Body::empty()).unwrap();
        get.extensions_mut().insert(sess(owner, false));
        let res = crate::router(st.clone(), "/nonexistent")
            .oneshot(get)
            .await
            .unwrap();
        let b = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let list = String::from_utf8_lossy(&b);
        assert!(
            list.contains("\"Mac\"") && list.contains("revoked_at") && !list.contains(&tok[6..]),
            "{list}"
        );

        let rows: Vec<(String, String, bool)> = sqlx::query_as(
            "select status, client, body is null from mcp_writes where guild_id = $1 order by id",
        )
        .bind(g)
        .fetch_all(&db)
        .await
        .unwrap();
        let want = |s: &str| (s.to_string(), "Mac".to_string(), true);
        assert_eq!(
            rows,
            [
                want("done"),
                want("done"),
                want("revoked"),
                want("stale"),
                want("revoked")
            ]
        );
        for t in ["mcp_writes", "guild_config", "embeds"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "delete from {t} where guild_id = $1"
            )))
            .bind(g)
            .execute(&db)
            .await
            .unwrap();
        }
        sqlx::query("delete from mcp_tokens where id = $1")
            .bind(tok_id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("delete from dash_totp where user_id = $1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
    }

    /// Parallel creates at 9 live tokens make exactly one more. Runs only with DATABASE_URL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_token_creates_stop_at_the_cap() {
        use std::sync::Arc;
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _one = TOKENS.lock().await;
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let owner = 7_800_000_000 + std::process::id() as i64;
        let mock = Router::new().route(
            "/applications/@me",
            get(move || async move { Json(json!({"owner": {"id": owner.to_string()}})) }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, mock).await });
        let st = AppState {
            db: db.clone(),
            discord: crate::discord::Discord {
                http: reqwest::Client::builder().no_proxy().build().unwrap(),
                client_id: String::new(),
                client_secret: String::new(),
                bot_token: String::new(),
                redirect_uri: String::new(),
                api,
                memo: Default::default(),
            },
            secure: false,
            totp: Default::default(),
            rate: Default::default(),
            ips: Default::default(),
            vpn_logged: Default::default(),
        };
        let live = || async {
            sqlx::query_scalar::<_, i64>("select count(*) from mcp_tokens where revoked_at is null")
                .fetch_one(&db)
                .await
                .unwrap()
        };
        // Fill up to one below the cap with rows of this test's owner.
        let fill = (MAX_TOKENS - 1 - live().await).max(0);
        sqlx::query(
            "insert into mcp_tokens (name, token_hash, created_by)
             select 'fill', sha256(($1 || ':' || i)::bytea), $1 from generate_series(1, $2) i",
        )
        .bind(owner)
        .bind(fill)
        .execute(&db)
        .await
        .unwrap();
        let before = live().await;
        // Warm the owner lookup and the pool's connections, so the requests reach the count together.
        st.discord.bot_owner().await.unwrap();
        let mut warm = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let db = db.clone();
            warm.spawn(async move { sqlx::query("select pg_sleep(0.05)").execute(&db).await });
        }
        warm.join_all().await;
        let app = crate::router(st.clone(), "/nonexistent");
        // A few rounds, as one round of 20 does not always hit the gap.
        let mut rounds = vec![];
        for _ in 0..5 {
            let go = Arc::new(tokio::sync::Barrier::new(20));
            let mut runs = tokio::task::JoinSet::new();
            for _ in 0..20 {
                let (app, go) = (app.clone(), go.clone());
                let mut req = Request::post("/api/mcp-tokens")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "race"}).to_string()))
                    .unwrap();
                req.extensions_mut().insert(Sess {
                    id: String::new(),
                    user_id: owner,
                    username: "t".into(),
                    access_token: String::new(),
                    stepped_up: true,
                    csrf: String::new(),
                });
                runs.spawn(async move {
                    go.wait().await;
                    app.oneshot(req).await.unwrap().status().as_u16()
                });
            }
            let codes = runs.join_all().await;
            rounds.push((
                codes.iter().filter(|c| **c == 200).count() as i64,
                live().await,
            ));
            sqlx::query("delete from mcp_tokens where created_by = $1 and name = 'race'")
                .bind(owner)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("delete from mcp_tokens where created_by = $1")
            .bind(owner)
            .execute(&db)
            .await
            .unwrap();
        let room = (MAX_TOKENS - before).max(0);
        assert_eq!(rounds, vec![(room, before.max(MAX_TOKENS)); 5]);
    }
}
