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
use tommybot_core::sections::{AppealField, AppealsCfg, MAX_APPEAL_FIELDS};

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

type MineRow = (
    i64,
    String,
    String,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
);

const MAX_ANSWER: usize = 1000;

/// Validates one answer with Wick's texts and returns what is stored. A multi choice answer is the
/// picked options, one per line.
fn check(f: &AppealField, a: &str) -> Result<String, String> {
    let a = a.trim();
    let none = || {
        if f.required {
            Err("This field must have a selected item.".to_string())
        } else {
            Ok("Not answered.".to_string())
        }
    };
    if !f.options.is_empty() {
        let picked: Vec<&str> = a.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if picked.is_empty() {
            return none();
        }
        if (picked.len() > 1 && !f.multi)
            || picked.iter().any(|p| !f.options.iter().any(|o| o == p))
        {
            return Err("Check what is highlighted in red.".into());
        }
        return Ok(picked.join(", "));
    }
    let max = f.max.unwrap_or(MAX_ANSWER).min(MAX_ANSWER);
    let min = f.min.unwrap_or(usize::from(f.required)).min(max);
    let n = a.chars().count();
    if n == 0 && !f.required {
        return Ok("Not answered.".into());
    }
    if n < min || n > max {
        return Err(format!(
            "This content has to be between {min} and {max} characters long."
        ));
    }
    Ok(a.into())
}

pub fn routes() -> Router<AppState> {
    let g = "/api/guilds/{guild}/appeals";
    Router::new()
        .route("/api/appeal/{guild}", get(form).post(submit))
        .route("/api/appeal/notifications", get(unread).post(mark_seen))
        .route("/api/appeal/{guild}/mine", get(mine))
        .route("/api/appeal/{guild}/withdraw", post(withdraw))
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
        .map(|f| json!({"label": f.label, "long": f.long, "required": f.required, "options": f.options, "multi": f.multi, "min": f.min, "max": f.max}))
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
    let mut given = Vec::new();
    for (f, a) in fields.iter().zip(&b.answers) {
        given.push(check(f, a).map_err(|m| ApiError::BadRequest(format!("{}: {m}", f.label)))?);
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
        .zip(&given)
        .map(|(f, a)| json!({"label": f.label, "answer": a}))
        .collect();
    let id: Option<i64> = sqlx::query_scalar(
        "insert into appeals (guild_id, user_id, username, kind, answers) values ($1,$2,$3,$4,$5) on conflict do nothing returning id")
        .bind(guild).bind(s.user_id).bind(&s.username).bind(kind).bind(Value::Array(answers)).fetch_optional(&st.db).await?;
    let id = id.ok_or(ApiError::Forbidden("you already have an open appeal"))?;
    let (_, config) = crate::store::current(&st.db, guild).await?;
    let lc: tommybot_logging::LogConfig = config
        .get("logging")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let channel = lc
        .routes
        .get(&tommybot_logging::LogKind::Appeal)
        .copied()
        .or(c.channel)
        .or(lc.fallback);
    if let Some(ch) = channel {
        use tommybot_logging::{embed, Log};
        let mut log = Log::new(
            &format!("A {kind} appeal by {} has been sent!", s.username),
            embed::ORANGE,
        )
        .line("Member", &format!("{} [{}]", s.username, s.user_id))
        .line("Appeal", &format!("`#{id}`"))
        .text(&format!("{} Review it on the dashboard.", embed::ARROW));
        for (f, a) in fields.iter().zip(&given) {
            log = log.field(&format!("{}:", f.label), a);
        }
        let button = |label: &str, act: &str, style: u8| json!({"type": 2, "style": style, "label": label, "custom_id": format!("appeal:{id}:{act}")});
        let row = json!([{"type": 1, "components": [
            button("Approve", "accept", 3),
            button("Deny", "deny", 4),
            button("Deny + Blacklist", "blacklist", 4),
            button("Dismiss", "dismiss", 2),
        ]}]);
        match st.discord.post_log_with(&ch.to_string(), log, row).await {
            Ok(msg) => {
                sqlx::query("update appeals set log_channel = $2, log_message = $3 where id = $1")
                    .bind(id)
                    .bind(ch.get() as i64)
                    .bind(msg.parse::<i64>().unwrap_or_default())
                    .execute(&st.db)
                    .await?;
            }
            Err(e) => tracing::warn!(?e, "appeal notice failed"),
        }
    }
    Ok(Json(json!({"id": id})))
}

/// The member's own appeals with their status and the staff reply.
async fn mine(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let rows: Vec<MineRow> = sqlx::query_as(
        "select id, kind, status, reply, created_at from appeals
         where guild_id = $1 and user_id = $2 order by created_at desc limit 20",
    )
    .bind(guild)
    .bind(s.user_id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(id, kind, status, reply, at)| json!({"id": id, "kind": kind, "status": status, "reply": reply, "at": at}))
        .collect::<Vec<_>>())))
}

/// Verdicts the member has not seen yet, across servers, for the header bell.
async fn unread(State(st): State<AppState>, s: Sess) -> Res {
    let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
        "select id, guild_id, kind, status from appeals
         where user_id = $1 and status in ('accepted', 'denied') and seen_at is null
         order by created_at desc limit 20",
    )
    .bind(s.user_id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(id, guild, kind, status)| json!({"id": id, "guild": guild.to_string(), "kind": kind, "status": status}))
        .collect::<Vec<_>>())))
}

async fn mark_seen(State(st): State<AppState>, s: Sess) -> Res {
    sqlx::query("update appeals set seen_at = now() where user_id = $1 and seen_at is null and status <> 'open'")
        .bind(s.user_id)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({"ok": true})))
}

/// The member takes back their open appeal.
async fn withdraw(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    let id: Option<i64> = sqlx::query_scalar(
        "update appeals set status = 'withdrawn' where guild_id = $1 and user_id = $2 and status = 'open' returning id",
    )
    .bind(guild)
    .bind(s.user_id)
    .fetch_optional(&st.db)
    .await?;
    let id = id.ok_or(ApiError::NotFound)?;
    close_log(&st, id, "Withdrawn by the member").await;
    Ok(Json(json!({"ok": true})))
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
    #[serde(default)]
    accept: bool,
    #[serde(default)]
    blacklist: bool,
    /// Closes the appeal without telling the appellant.
    #[serde(default)]
    dismiss: bool,
    /// Optional staff reply, sent to the appellant.
    #[serde(default)]
    reply: String,
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
    let accept = b.accept && !b.dismiss;
    if accept {
        accept_appeal(&st, &s, guild, user, &kind, &mut tx).await?;
    }
    let status = match (b.dismiss, b.accept) {
        (true, _) => "dismissed",
        (_, true) => "accepted",
        _ => "denied",
    };
    let reply = b.reply.trim().chars().take(1000).collect::<String>();
    sqlx::query(
        "update appeals set status = $2, decided_by = $3, reply = nullif($4, '') where id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(s.user_id)
    .bind(&reply)
    .execute(&mut *tx)
    .await?;
    if b.blacklist && status == "denied" {
        add_blacklist(&mut tx, guild, user, s.user_id).await?;
    }
    tx.commit().await?;
    let word = match status {
        "dismissed" => "Dismissed",
        "accepted" => "Approved",
        _ if b.blacklist => "Denied and blacklisted",
        _ => "Denied",
    };
    let mut decision = format!("{word} by <@{}>", s.user_id);
    if !reply.is_empty() {
        decision.push_str(&format!("\n{reply}"));
    }
    close_log(&st, id, &decision).await;
    if !b.dismiss {
        let verdict = if b.accept { "approved" } else { "denied" };
        let mut text = format!("Your {kind} appeal (#{id}) was {verdict}.");
        if !reply.is_empty() {
            text += &format!("\nReply from the staff: {reply}");
        }
        let dm = st
            .discord
            .call(
                Method::POST,
                "/users/@me/channels",
                Some(json!({"recipient_id": user.to_string()})),
            )
            .await;
        if let Some(ch) = dm.ok().and_then(|c| c["id"].as_str().map(String::from)) {
            let body = json!({"content": text, "allowed_mentions": {"parse": []}});
            if let Err(e) = st
                .discord
                .call(
                    Method::POST,
                    &format!("/channels/{ch}/messages"),
                    Some(body),
                )
                .await
            {
                tracing::warn!(?e, "appeal verdict DM failed");
            }
        }
    }
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

/// Closes the Discord log of an appeal decided elsewhere: buttons off, a Decision field added.
async fn close_log(st: &AppState, id: i64, decision: &str) {
    let row: Option<(Option<i64>, Option<i64>)> =
        sqlx::query_as("select log_channel, log_message from appeals where id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await
            .ok()
            .flatten();
    let Some((Some(ch), Some(msg))) = row else {
        return;
    };
    let path = format!("/channels/{ch}/messages/{msg}");
    let Ok(mut m) = st.discord.call(Method::GET, &path, None).await else {
        return;
    };
    let mut embeds = m["embeds"].take();
    if let Some(fields) = embeds[0]["fields"].as_array_mut() {
        fields.push(json!({"name": "Decision", "value": decision, "inline": false}));
    }
    let body = json!({"embeds": embeds, "components": []});
    if let Err(e) = st.discord.call(Method::PATCH, &path, Some(body)).await {
        tracing::warn!(?e, "closing the appeal log failed");
    }
}

/// Lifts the ban or timeout and logs it with its undo.
async fn accept_appeal(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn field(options: &[&str], multi: bool, required: bool) -> AppealField {
        AppealField {
            label: "q".into(),
            long: false,
            required,
            options: options.iter().map(|o| o.to_string()).collect(),
            multi,
            min: Some(4),
            max: Some(10),
        }
    }

    #[test]
    fn text_length_and_optional() {
        let f = field(&[], false, true);
        assert_eq!(check(&f, "abcd").unwrap(), "abcd");
        assert_eq!(
            check(&f, "abc").unwrap_err(),
            "This content has to be between 4 and 10 characters long."
        );
        assert_eq!(
            check(&field(&[], false, false), " ").unwrap(),
            "Not answered."
        );
    }

    #[test]
    fn min_above_max_still_accepts_a_max_length_answer() {
        let mut f = field(&[], false, true);
        f.min = Some(20);
        assert!(check(&f, "0123456789").is_ok());
    }

    #[test]
    fn choices() {
        let single = field(&["a", "b"], false, true);
        assert_eq!(
            check(&single, "").unwrap_err(),
            "This field must have a selected item."
        );
        assert!(check(&single, "a\nb").is_err());
        assert_eq!(
            check(&field(&["a", "b"], true, true), "a\nb").unwrap(),
            "a, b"
        );
        assert!(check(&single, "c").is_err());
    }
}
