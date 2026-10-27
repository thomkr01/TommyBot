//! Staff records: cases, notes and reports (with the reporter blacklist). Each page needs its permit.
use crate::{api, auth::Sess, ApiError, AppState};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

type Res = Result<Json<Value>, ApiError>;
type At = chrono::DateTime<chrono::Utc>;
type CaseRow = (
    i32,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i32,
    bool,
    At,
);

const NOTE_MAX: usize = 500;
const CASE_TEXT_MAX: usize = 1000;
fn ok() -> Res {
    Ok(Json(json!({"ok": true})))
}

pub fn routes() -> Router<AppState> {
    let g = "/api/guilds/{guild}";
    Router::new()
        .route(&format!("{g}/cases"), get(cases))
        .route(&format!("{g}/cases/{{no}}"), post(case_edit))
        .route(&format!("{g}/cases/{{no}}/delete"), post(case_delete))
        .route(&format!("{g}/raid_log"), get(raid_log))
        .route(&format!("{g}/notes"), get(notes).post(note_add))
        .route(&format!("{g}/notes/sweep"), post(note_sweep))
        .route(&format!("{g}/notes/{{id}}"), post(note_edit))
        .route(&format!("{g}/notes/{{id}}/delete"), post(note_delete))
        .route(&format!("{g}/reports"), get(reports))
        .route(&format!("{g}/reports/{{id}}/status"), post(report_status))
        .route(
            &format!("{g}/reports/blacklist"),
            get(blacklist).post(blacklist_add),
        )
        .route(
            &format!("{g}/reports/blacklist/{{user}}/remove"),
            post(blacklist_remove),
        )
}

fn user_id(raw: &str) -> Result<i64, ApiError> {
    raw.trim()
        .parse()
        .map_err(|_| ApiError::BadRequest("user ID must be a number".into()))
}

fn clip(text: &str, max: usize) -> Result<String, ApiError> {
    let t = text.trim();
    match t.chars().count() {
        0 => Err(ApiError::BadRequest("text is required".into())),
        n if n > max => Err(ApiError::BadRequest(format!(
            "longer than {max} characters"
        ))),
        _ => Ok(t.into()),
    }
}

/// The last 20 join raids, newest first, with every account each one caught.
async fn raid_log(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::staff(&st, &s, guild, &["cases"]).await?;
    let rows: Vec<(i64, String, String, At)> = sqlx::query_as(
        "select raid_ms, user_id::text, name, created_at from raid_log
         where guild_id = $1 and raid_ms in
           (select distinct raid_ms from raid_log where guild_id = $1 order by raid_ms desc limit 20)
         order by raid_ms desc, created_at",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    let mut raids: Vec<Value> = vec![];
    for (raid, user, name, at) in rows {
        if raids.last().is_none_or(|r| r["raid_ms"] != raid) {
            raids.push(json!({"raid_ms": raid, "accounts": []}));
        }
        let accounts = raids.last_mut().unwrap()["accounts"]
            .as_array_mut()
            .unwrap();
        accounts.push(json!({"user_id": user, "name": name, "at": at}));
    }
    Ok(Json(json!(raids)))
}

#[derive(Deserialize)]
struct CaseFilter {
    q: Option<String>,
    kind: Option<String>,
    /// "open", "closed" or anything else for both
    state: Option<String>,
}

async fn cases(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(f): Query<CaseFilter>,
) -> Res {
    api::staff(&st, &s, guild, &["cases"]).await?;
    let rows: Vec<CaseRow> = sqlx::query_as(
            "select case_no, kind, user_id::text, mod_id::text, reason, proof, points, active, created_at
             from cases where guild_id = $1
               and ($2 = '' or user_id::text = $2 or mod_id::text = $2)
               and ($3 = '' or kind = $3)
               and ($4 = '' or active = ($4 = 'open'))
             order by case_no desc limit 200",
        )
        .bind(guild)
        .bind(f.q.unwrap_or_default().trim())
        .bind(f.kind.unwrap_or_default())
        .bind(f.state.unwrap_or_default())
        .fetch_all(&st.db)
        .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(
            |(no, kind, user, moderator, reason, proof, points, active, at)| json!({
                "case_no": no, "kind": kind, "user_id": user, "mod_id": moderator, "reason": reason,
                "proof": proof, "points": points, "active": active, "at": at
            })
        )
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct CaseEdit {
    reason: String,
    proof: String,
    active: bool,
}

/// Close/reopen is the `active` flag, which is also what counts warn points.
async fn case_edit(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, no)): Path<(i64, i32)>,
    Json(b): Json<CaseEdit>,
) -> Res {
    api::staff(&st, &s, guild, &["cases"]).await?;
    let text = |t: &str| {
        (!t.trim().is_empty()).then(|| t.trim().chars().take(CASE_TEXT_MAX).collect::<String>())
    };
    let n = sqlx::query("update cases set reason = $3, proof = $4, active = $5 where guild_id = $1 and case_no = $2")
        .bind(guild)
        .bind(no)
        .bind(text(&b.reason))
        .bind(text(&b.proof))
        .bind(b.active)
        .execute(&st.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    ok()
}

/// Deleting erases the audit trail, so it stays with the trusted admins and owners.
async fn case_delete(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, no)): Path<(i64, i32)>,
) -> Res {
    api::require(&st, &s, guild, crate::access::Level::Admin, false).await?;
    sqlx::query("delete from cases where guild_id = $1 and case_no = $2")
        .bind(guild)
        .bind(no)
        .execute(&st.db)
        .await?;
    ok()
}

#[derive(Deserialize)]
struct NoteFilter {
    q: Option<String>,
}

async fn notes(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(f): Query<NoteFilter>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_view", "notes_manage"]).await?;
    let rows: Vec<(i64, String, String, String, At)> = sqlx::query_as(
        "select id, user_id::text, author_id::text, text, created_at from notes
         where guild_id = $1 and ($2 = '' or user_id::text = $2) order by id desc limit 200",
    )
    .bind(guild)
    .bind(f.q.unwrap_or_default().trim())
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(id, user, author, text, at)| json!({"id": id, "user_id": user, "author_id": author, "text": text, "at": at}))
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct NoteBody {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    text: String,
}

async fn note_add(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    sqlx::query("insert into notes (guild_id, user_id, author_id, text) values ($1,$2,$3,$4)")
        .bind(guild)
        .bind(user_id(&b.user_id)?)
        .bind(s.user_id)
        .bind(clip(&b.text, NOTE_MAX)?)
        .execute(&st.db)
        .await?;
    ok()
}

async fn note_edit(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, i64)>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    let n = sqlx::query("update notes set text = $3 where guild_id = $1 and id = $2")
        .bind(guild)
        .bind(id)
        .bind(clip(&b.text, NOTE_MAX)?)
        .execute(&st.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    ok()
}

async fn note_delete(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, i64)>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    sqlx::query("delete from notes where guild_id = $1 and id = $2")
        .bind(guild)
        .bind(id)
        .execute(&st.db)
        .await?;
    ok()
}

async fn note_sweep(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    let n = sqlx::query("delete from notes where guild_id = $1 and user_id = $2")
        .bind(guild)
        .bind(user_id(&b.user_id)?)
        .execute(&st.db)
        .await?
        .rows_affected();
    Ok(Json(json!({"removed": n})))
}

#[derive(Deserialize)]
struct ReportFilter {
    /// "open", "dismissed", "actioned" or anything else for all
    state: Option<String>,
}

async fn reports(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(f): Query<ReportFilter>,
) -> Res {
    api::staff(&st, &s, guild, &["reports"]).await?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, String, String, String, String, Option<String>, Option<String>, At)> = sqlx::query_as(
        "select id, reporter_id::text, target_id::text, channel_id::text, message_id::text, content, status,
                action, handled_by::text, created_at
         from reports where guild_id = $1 and ($2 = '' or status = $2)
         order by (status = 'open') desc, id desc limit 200",
    )
    .bind(guild)
    .bind(f.state.unwrap_or_default())
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(
            |(id, reporter, target, channel, message, content, status, action, by, at)| json!({
                "id": id, "reporter_id": reporter, "target_id": target, "channel_id": channel,
                "message_id": message, "content": content, "status": status, "action": action,
                "handled_by": by, "at": at
            })
        )
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct Status {
    open: bool,
}

/// Dismiss or reopen. Bans, kicks and the like happen with the buttons in Discord.
async fn report_status(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, i64)>,
    Json(b): Json<Status>,
) -> Res {
    api::staff(&st, &s, guild, &["reports"]).await?;
    let n = sqlx::query(
        "update reports set status = $3, handled_by = case when $3 = 'open' then null else $4 end,
                handled_at = case when $3 = 'open' then null else now() end, action = null
         where guild_id = $1 and id = $2",
    )
    .bind(guild)
    .bind(id)
    .bind(if b.open { "open" } else { "dismissed" })
    .bind(s.user_id)
    .execute(&st.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    ok()
}

async fn blacklist(State(st): State<AppState>, s: Sess, Path(guild): Path<i64>) -> Res {
    api::staff(&st, &s, guild, &["reports"]).await?;
    let rows: Vec<(String, At)> = sqlx::query_as(
        "select user_id::text, added_at from report_blacklist where guild_id = $1 order by added_at desc",
    )
    .bind(guild)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(u, at)| json!({"user_id": u, "at": at}))
        .collect::<Vec<_>>())))
}

async fn blacklist_add(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["reports"]).await?;
    sqlx::query("insert into report_blacklist (guild_id, user_id, added_by) values ($1,$2,$3) on conflict do nothing")
        .bind(guild)
        .bind(user_id(&b.user_id)?)
        .bind(s.user_id)
        .execute(&st.db)
        .await?;
    ok()
}

async fn blacklist_remove(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, user)): Path<(i64, i64)>,
) -> Res {
    api::staff(&st, &s, guild, &["reports"]).await?;
    sqlx::query("delete from report_blacklist where guild_id = $1 and user_id = $2")
        .bind(guild)
        .bind(user)
        .execute(&st.db)
        .await?;
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_checks() {
        assert!(user_id(" 123 ").is_ok());
        assert!(user_id("abc").is_err());
        assert!(clip("  ", 5).is_err());
        assert!(clip("123456", 5).is_err());
        assert_eq!(clip(" hi ", 5).unwrap(), "hi");
    }
}
