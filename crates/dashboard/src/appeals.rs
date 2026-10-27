//! Appeals: the public form a banned or timed-out user fills in, and the staff list and blacklist.
use crate::{access::Level, api, auth::Sess, ApiError, AppState};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use tommybot_core::sections::{AppealsCfg, MAX_APPEAL_FIELDS};

type Res = Result<Json<Value>, ApiError>;

type Row = (
    i64,
    String,
    String,
    String,
    Value,
    String,
    chrono::DateTime<chrono::Utc>,
);

const MAX_ANSWER: usize = 1000;

pub fn routes() -> Router<AppState> {
    let g = "/api/guilds/{guild}/appeals";
    Router::new()
        .route("/api/appeal/{guild}", get(form).post(submit))
        .route(g, get(list))
        .route(&format!("{g}/{{id}}/decide"), post(decide))
        .route(
            &format!("{g}/blacklist"),
            get(blacklist).post(blacklist_add),
        )
        .route(&format!("{g}/blacklist/purge"), post(blacklist_purge))
        .route(
            &format!("{g}/blacklist/{{user}}/remove"),
            post(blacklist_remove),
        )
}

async fn cfg(st: &AppState, guild: i64) -> Result<AppealsCfg, ApiError> {
    let (_, config) = crate::store::current(&st.db, guild).await?;
    Ok(config
        .get("appeals")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().and_then(|e| e.status())
        == Some(reqwest::StatusCode::NOT_FOUND)
}

/// What the user can appeal in this server: "ban", "timeout" or None.
async fn punishment(
    st: &AppState,
    guild: i64,
    user: i64,
) -> Result<Option<&'static str>, ApiError> {
    match st
        .discord
        .call(Method::GET, &format!("/guilds/{guild}/bans/{user}"), None)
        .await
    {
        Ok(_) => return Ok(Some("ban")),
        Err(e) if not_found(&e) => {}
        Err(e) => return Err(e.into()),
    }
    match st
        .discord
        .call(
            Method::GET,
            &format!("/guilds/{guild}/members/{user}"),
            None,
        )
        .await
    {
        Ok(m) => {
            let until = m["communication_disabled_until"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            Ok(until.filter(|t| *t > chrono::Utc::now()).map(|_| "timeout"))
        }
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn blacklisted(st: &AppState, guild: i64, user: i64) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar(
        "select exists(select 1 from appeal_blacklist where guild_id = $1 and user_id = $2)",
    )
    .bind(guild)
    .bind(user)
    .fetch_one(&st.db)
    .await?)
}

/// The appellant's view: the form fields and whether they can submit.
async fn form(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let c = cfg(&st, guild).await?;
    if !c.enabled {
        return Err(ApiError::NotFound);
    }
    let kind = punishment(&st, guild, s.user_id).await?;
    let fields: Vec<Value> = c
        .fields
        .iter()
        .take(MAX_APPEAL_FIELDS)
        .map(|f| json!({"label": f.label, "long": f.long, "required": f.required}))
        .collect();
    Ok(Json(
        json!({"fields": fields, "kind": kind, "blacklisted": blacklisted(&st, guild, s.user_id).await?}),
    ))
}

#[derive(Deserialize)]
struct Submit {
    answers: Vec<String>,
}

async fn submit(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Submit>,
) -> Res {
    let c = cfg(&st, guild).await?;
    if !c.enabled {
        return Err(ApiError::NotFound);
    }
    let fields: Vec<_> = c.fields.iter().take(MAX_APPEAL_FIELDS).collect();
    if b.answers.len() != fields.len() {
        return Err(ApiError::BadRequest(
            "answer count does not match the form".into(),
        ));
    }
    let bad = |m: String| ApiError::BadRequest(m);
    for (f, a) in fields.iter().zip(&b.answers) {
        if a.chars().count() > MAX_ANSWER {
            return Err(bad(format!(
                "\"{}\" is longer than {MAX_ANSWER} characters",
                f.label
            )));
        }
        if f.required && a.trim().is_empty() {
            return Err(bad(format!("\"{}\" is required", f.label)));
        }
    }
    if blacklisted(&st, guild, s.user_id).await? {
        return Err(ApiError::Forbidden(
            "you can no longer appeal in this server",
        ));
    }
    let kind = punishment(&st, guild, s.user_id)
        .await?
        .ok_or(ApiError::Forbidden("you are not banned or timed out here"))?;
    let answers: Vec<Value> = fields
        .iter()
        .zip(&b.answers)
        .map(|(f, a)| json!({"label": f.label, "answer": a}))
        .collect();
    let id: Option<i64> = sqlx::query_scalar(
        "insert into appeals (guild_id, user_id, username, kind, answers) values ($1,$2,$3,$4,$5) on conflict do nothing returning id")
        .bind(guild).bind(s.user_id).bind(&s.username).bind(kind).bind(Value::Array(answers)).fetch_optional(&st.db).await?;
    let id = id.ok_or(ApiError::Forbidden("you already have an open appeal"))?;
    if let Some(ch) = c.channel {
        use tommybot_logging::{embed, Log};
        let mut log = Log::new(
            &format!("A {kind} appeal by {} has been sent!", s.username),
            embed::ORANGE,
        )
        .line("Member", &format!("{} [{}]", s.username, s.user_id))
        .line("Appeal", &format!("`#{id}`"))
        .text(&format!("{} Review it on the dashboard.", embed::ARROW));
        for (f, a) in fields.iter().zip(&b.answers) {
            log = log.field(&format!("{}:", f.label), a);
        }
        if let Err(e) = st.discord.post_log(&ch.to_string(), log).await {
            tracing::warn!(?e, "appeal notice failed");
        }
    }
    Ok(Json(json!({"id": id})))
}

#[derive(Deserialize)]
struct Filter {
    kind: Option<String>,
    q: Option<String>,
}

async fn list(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(f): Query<Filter>,
) -> Res {
    reviewer(&st, &s, guild).await?;
    let q = f.q.unwrap_or_default();
    let rows: Vec<Row> = sqlx::query_as(
        "select id, user_id::text, username, kind, answers, status, created_at from appeals
         where guild_id = $1 and ($2::text is null or kind = $2)
           and ($3 = '' or user_id::text = $3 or username ilike '%' || $3 || '%')
         order by (status = 'open') desc, created_at desc limit 200",
    )
    .bind(guild)
    .bind(f.kind.filter(|k| k == "ban" || k == "timeout"))
    .bind(q.trim())
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows.into_iter().map(|(id, user_id, username, kind, answers, status, at)|
        json!({"id": id, "user_id": user_id, "username": username, "kind": kind, "answers": answers, "status": status, "at": at})).collect::<Vec<_>>())))
}

async fn reviewer(st: &AppState, s: &Sess, guild: i64) -> Result<(), ApiError> {
    api::staff(st, s, guild, &["appeals"]).await
}

async fn has_perm(st: &AppState, s: &Sess, guild: i64, perm: &str) -> Result<bool, ApiError> {
    Ok(api::permit_perms(st, s, guild)
        .await?
        .iter()
        .any(|p| p == perm))
}

#[derive(Deserialize)]
struct Decide {
    accept: bool,
    #[serde(default)]
    blacklist: bool,
}

/// Accepting lifts the ban or timeout; denying can also blacklist the user from appealing again.
async fn decide(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, i64)>,
    Json(b): Json<Decide>,
) -> Res {
    reviewer(&st, &s, guild).await?;
    let mut tx = st.db.begin().await?;
    let (user, kind): (i64, String) = sqlx::query_as("select user_id, kind from appeals where id = $1 and guild_id = $2 and status = 'open' for update")
        .bind(id).bind(guild).fetch_optional(&mut *tx).await?.ok_or(ApiError::NotFound)?;
    if b.accept {
        accept(&st, &s, guild, user, &kind, &mut tx).await?;
    }
    sqlx::query("update appeals set status = $2, decided_by = $3 where id = $1")
        .bind(id)
        .bind(if b.accept { "accepted" } else { "denied" })
        .bind(s.user_id)
        .execute(&mut *tx)
        .await?;
    if b.blacklist && !b.accept {
        add_blacklist(&mut tx, guild, user, s.user_id).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

/// Runs a Discord call that lifts a punishment; a 404 means it was already lifted by hand.
async fn lift(
    st: &AppState,
    method: Method,
    path: String,
    body: Option<Value>,
) -> Result<(), ApiError> {
    match st.discord.call(method, &path, body).await {
        Ok(_) => Ok(()),
        Err(e) if not_found(&e) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Lifts the ban or timeout and logs it with its undo.
async fn accept(
    st: &AppState,
    s: &Sess,
    guild: i64,
    user: i64,
    kind: &str,
    tx: &mut sqlx::PgConnection,
) -> Result<(), ApiError> {
    // Lifting a punishment takes its permit ("ban" or "timeout"), not just Review Appeals (spec 2).
    if api::level_of(st, s, guild).await? < Level::Admin && !has_perm(st, s, guild, kind).await? {
        return Err(ApiError::Forbidden(
            "accepting needs the ban or timeout permit",
        ));
    }
    let inverse = if kind == "ban" {
        lift(
            st,
            Method::DELETE,
            format!("/guilds/{guild}/bans/{user}"),
            None,
        )
        .await?;
        Some(json!({"op": "ban", "user": user}))
    } else {
        let path = format!("/guilds/{guild}/members/{user}");
        let until = match st.discord.call(Method::GET, &path, None).await {
            Ok(m) => m["communication_disabled_until"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.timestamp()),
            Err(e) if not_found(&e) => None,
            Err(e) => return Err(e.into()),
        };
        lift(
            st,
            Method::PATCH,
            path,
            Some(json!({"communication_disabled_until": null})),
        )
        .await?;
        until.map(|until| json!({"op": "timeout", "user": user, "until": until}))
    };
    sqlx::query("insert into action_log (guild_id, actor_id, action, target_id, inverse) values ($1,$2,$3,$4,$5)")
        .bind(guild).bind(s.user_id).bind(if kind == "ban" { "unban" } else { "untimeout" }).bind(user).bind(inverse).execute(&mut *tx).await?;
    Ok(())
}

async fn add_blacklist(
    c: &mut sqlx::PgConnection,
    guild: i64,
    user: i64,
    by: i64,
) -> Result<(), ApiError> {
    sqlx::query("insert into appeal_blacklist (guild_id, user_id, added_by) values ($1,$2,$3) on conflict do nothing")
        .bind(guild).bind(user).bind(by).execute(c).await?;
    Ok(())
}

async fn blacklist(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::require(&st, &s, guild, Level::Viewer, false).await?;
    let rows: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as("select user_id::text, added_at from appeal_blacklist where guild_id = $1 order by added_at desc")
        .bind(guild).fetch_all(&st.db).await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(u, at)| json!({"user_id": u, "at": at}))
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct Add {
    user_id: String,
}

async fn blacklist_add(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<Add>,
) -> Res {
    api::require(&st, &s, guild, Level::Editor, false).await?;
    let user = b
        .user_id
        .trim()
        .parse::<i64>()
        .map_err(|_| ApiError::BadRequest("user ID must be a number".into()))?;
    add_blacklist(&mut *st.db.acquire().await?, guild, user, s.user_id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn blacklist_remove(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, user)): Path<(i64, i64)>,
) -> Res {
    api::require(&st, &s, guild, Level::Editor, false).await?;
    sqlx::query("delete from appeal_blacklist where guild_id = $1 and user_id = $2")
        .bind(guild)
        .bind(user)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({"ok": true})))
}

/// Purge all: clears the whole blacklist, so it needs an admin.
async fn blacklist_purge(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::require(&st, &s, guild, Level::Admin, false).await?;
    let n = sqlx::query("delete from appeal_blacklist where guild_id = $1")
        .bind(guild)
        .execute(&st.db)
        .await?
        .rows_affected();
    Ok(Json(json!({"removed": n})))
}
