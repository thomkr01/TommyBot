//! Staff records: cases, notes and reports (with the reporter blacklist). Each page needs its permit.
use crate::{api, auth::Sess, ApiError, AppState};
use axum::{
    extract::{Path, Query, State},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tommybot_core::{
    jobs,
    notes::{self, NoteAction, NoteLog},
};

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
        .route("/raid/{code}", get(raid_page))
        .route(&format!("{g}/cases"), get(cases))
        .route(&format!("{g}/cases/{{no}}"), post(case_edit))
        .route(&format!("{g}/cases/{{no}}/delete"), post(case_delete))
        .route(&format!("{g}/mod_stats"), get(mod_stats))
        .route(&format!("{g}/mod_stats/{{user}}"), get(mod_actions))
        .route(&format!("{g}/raid_log"), get(raid_log))
        .route(&format!("{g}/joingate_log"), get(joingate_log))
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
struct Page {
    #[serde(default)]
    page: i64,
}

/// Join Gate punishments, newest first: what matched, the bot and who added it, and Discord's refusal.
async fn joingate_log(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(p): Query<Page>,
) -> Res {
    api::staff(&st, &s, guild, &["cases"]).await?;
    type Row = (
        String,
        bool,
        Option<String>,
        Value,
        String,
        String,
        bool,
        Option<String>,
        At,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select user_id::text, is_bot, adder_id::text, filters, reason, action, ok, error, created_at
         from joingate_actions where guild_id = $1 order by id desc limit 50 offset $2",
    )
    .bind(guild)
    .bind(offset(p.page))
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(user, bot, adder, filters, reason, action, ok, error, at)| json!({
            "user_id": user, "is_bot": bot, "adder_id": adder, "filters": filters, "reason": reason,
            "action": action, "ok": ok, "error": error, "at": at
        }))
        .collect::<Vec<_>>())))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The public report of one join raid, open to anyone who has its code.
async fn raid_page(State(st): State<AppState>, Path(code): Path<String>) -> Html<String> {
    let rows: Vec<(i64, i64, String, Option<String>, i64)> = sqlx::query_as(
        "select guild_id, user_id, name, invite_code, raid_ms from raid_log where code = $1 order by created_at",
    )
    .bind(&code)
    .fetch_all(&st.db)
    .await
    .unwrap_or_default();
    let body = match rows.first() {
        None => "<h1>Raid not found</h1>".to_string(),
        Some(first) => {
            let ids = rows
                .iter()
                .map(|r| r.1.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let list: String = rows
                .iter()
                .map(|(_, id, name, inv, _)| {
                    let inv = inv
                        .as_deref()
                        .map_or(String::new(), |i| format!(" via <code>{}</code>", esc(i)));
                    format!("<li><code>{id}</code> {}{inv}</li>", esc(name))
                })
                .collect();
            format!(
                "<h1>Join Raid Report</h1><p><code>{}</code></p><p>Server <code>{}</code> &middot; {}</p>\
                 <h2>{} flagged accounts</h2><button onclick=\"navigator.clipboard.writeText(this.dataset.ids)\" data-ids=\"{ids}\">Copy IDs</button><ul>{list}</ul>",
                esc(&code),
                first.0,
                chrono::DateTime::from_timestamp_millis(first.4).map_or_else(String::new, |d| d.format("%Y-%m-%d %H:%M UTC").to_string()),
                rows.len()
            )
        }
    };
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Join Raid Report</title><style>body{{background:#1c1c1c;color:#eee;font:16px system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem}}code{{background:#2f2f2f;padding:.1rem .4rem;border-radius:.3rem}}li{{margin:.3rem 0}}button{{background:#1be725;border:0;border-radius:.5rem;padding:.5rem 1rem;font-weight:600;cursor:pointer}}</style>{body}"#
    ))
}

/// Lists come in pages of 50 (the dashboard's `Pager`).
pub fn offset(page: i64) -> i64 {
    page.clamp(0, 10_000) * 50
}

#[derive(Deserialize)]
struct CaseFilter {
    q: Option<String>,
    #[serde(default)]
    page: i64,
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
             order by case_no desc limit 50 offset $5",
        )
        .bind(guild)
        .bind(f.q.unwrap_or_default().trim())
        .bind(f.kind.unwrap_or_default())
        .bind(f.state.unwrap_or_default())
        .bind(offset(f.page))
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
    let mut tx = st.db.begin().await?;
    // The old values come from the same row lock, so only a real change is counted.
    let changed: Option<(i64, bool)> = sqlx::query_as(
        "with old as (select reason, proof, active from cases where guild_id = $1 and case_no = $2 for update)
         update cases c set reason = $3, proof = $4, active = $5 from old
         where c.guild_id = $1 and c.case_no = $2
         returning c.user_id, (old.reason, old.proof, old.active) is distinct from ($3, $4, $5)",
    )
    .bind(guild)
    .bind(no)
    .bind(text(&b.reason))
    .bind(text(&b.proof))
    .bind(b.active)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user, changed)) = changed else {
        return Err(ApiError::NotFound);
    };
    if changed {
        staff_action(&mut tx, guild, &s, "case_edit", user, no).await?;
    }
    tx.commit().await?;
    ok()
}

/// A dashboard change to a case counts as an action of its editor, so cases cannot be quietly rewritten.
async fn staff_action(
    tx: &mut sqlx::PgConnection,
    guild: i64,
    s: &Sess,
    kind: &str,
    user: i64,
    no: i32,
) -> Result<(), ApiError> {
    tommybot_core::cases::record(
        tx,
        tommybot_core::cases::ModAction {
            guild_id: guild,
            mod_id: s.user_id,
            kind,
            source: "dashboard",
            target_id: Some(user),
            case_no: Some(no),
            audit_id: None,
        },
    )
    .await?;
    Ok(())
}

/// Deleting erases the audit trail, so it stays with the trusted admins and owners.
async fn case_delete(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, no)): Path<(i64, i32)>,
) -> Res {
    api::require(&st, &s, guild, crate::access::Level::Admin, false).await?;
    let mut tx = st.db.begin().await?;
    let user: i64 = sqlx::query_scalar(
        "delete from cases where guild_id = $1 and case_no = $2 returning user_id",
    )
    .bind(guild)
    .bind(no)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    staff_action(&mut tx, guild, &s, "case_delete", user, no).await?;
    tx.commit().await?;
    ok()
}

/// Owners, and roles an owner gave View Mod Stats; checked before anything in the request is read.
async fn stats_access(st: &AppState, s: &Sess, guild: i64) -> Result<(), ApiError> {
    let l = api::require(st, s, guild, crate::access::Level::Viewer, false).await?;
    let perms = if l == crate::access::Level::Owner {
        vec![]
    } else {
        api::permit_perms(st, s, guild).await?
    };
    if crate::access::mod_stats(l, &perms) {
        Ok(())
    } else {
        Err(ApiError::Forbidden("needs the View Mod Stats permit"))
    }
}

#[derive(Deserialize)]
struct StatsQuery {
    days: Option<String>,
}

/// Days in the period and days per bar: daily bars up to a month, weekly after.
fn period(raw: Option<&str>) -> Result<(i32, i32), ApiError> {
    match raw.unwrap_or("30") {
        "7" => Ok((7, 1)),
        "30" => Ok((30, 1)),
        "90" => Ok((90, 7)),
        "365" => Ok((365, 7)),
        _ => Err(ApiError::BadRequest("days must be 7, 30, 90 or 365".into())),
    }
}

/// Actions per staff member in the period, against the period before, with bars over time.
async fn mod_stats(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(q): Query<StatsQuery>,
) -> Res {
    stats_access(&st, &s, guild).await?;
    let (days, step) = period(q.days.as_deref())?;
    // The bot's own automatic actions are not staff work.
    let me = st.discord.bot_user().await?;
    // One transaction, so all three queries see the same now().
    let mut tx = st.db.begin().await?;
    let counts: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "select mod_id::text, kind,
                count(*) filter (where created_at >= now() - make_interval(days => $3)),
                count(*) filter (where created_at < now() - make_interval(days => $3))
         from mod_actions
         where guild_id = $1 and mod_id <> $2 and created_at >= now() - make_interval(days => $3 * 2)
         group by 1, 2",
    )
    .bind(guild)
    .bind(me)
    .bind(days)
    .fetch_all(&mut *tx)
    .await?;
    let last: Vec<(String, At)> = sqlx::query_as(
        "select mod_id::text, max(created_at) from mod_actions
         where guild_id = $1 and mod_id <> $2 group by 1",
    )
    .bind(guild)
    .bind(me)
    .fetch_all(&mut *tx)
    .await?;
    let bars: Vec<(String, i32, i64)> = sqlx::query_as(
        "select mod_id::text,
                floor(extract(epoch from now() - created_at) / ($4 * 86400))::int, count(*)
         from mod_actions
         where guild_id = $1 and mod_id <> $2 and created_at >= now() - make_interval(days => $3)
         group by 1, 2",
    )
    .bind(guild)
    .bind(me)
    .bind(days)
    .bind(step)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(stats_json(days, step, counts, last, bars)))
}

#[derive(Default, serde::Serialize)]
struct ModRow {
    mod_id: String,
    by_kind: std::collections::BTreeMap<String, i64>,
    total: i64,
    prev_total: i64,
    last_at: Option<At>,
    series: Vec<i64>,
}

/// Builds the page's rows: oldest bar first, busiest member first, at most 200.
fn stats_json(
    days: i32,
    step: i32,
    counts: Vec<(String, String, i64, i64)>,
    last: Vec<(String, At)>,
    bars: Vec<(String, i32, i64)>,
) -> Value {
    use std::collections::{BTreeMap, BTreeSet};
    // Whole bars only: the days left over join the oldest bar instead of making a short one.
    let n = (days / step) as usize;
    let mut kinds = BTreeSet::new();
    let mut rows: BTreeMap<String, ModRow> = BTreeMap::new();
    fn row(rows: &mut BTreeMap<String, ModRow>, id: String, n: usize) -> &mut ModRow {
        rows.entry(id.clone()).or_insert_with(|| ModRow {
            mod_id: id,
            series: vec![0; n],
            ..Default::default()
        })
    }
    for (id, at) in last {
        row(&mut rows, id, n).last_at = Some(at);
    }
    for (id, kind, now, prev) in counts {
        let r = row(&mut rows, id, n);
        if now > 0 {
            r.by_kind.insert(kind.clone(), now);
            kinds.insert(kind);
        }
        r.total += now;
        r.prev_total += prev;
    }
    for (id, ago, c) in bars {
        // `ago` counts bars back from now; anything past the last whole bar joins the oldest.
        let i = n - 1 - (ago.max(0) as usize).min(n - 1);
        row(&mut rows, id, n).series[i] += c;
    }
    let mut out: Vec<ModRow> = rows.into_values().collect();
    out.sort_by(|a, b| b.total.cmp(&a.total).then(b.last_at.cmp(&a.last_at)));
    out.truncate(200);
    json!({"days": days, "step": step, "kinds": kinds, "mods": out})
}

/// One staff member's last 50 actions.
async fn mod_actions(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, user)): Path<(i64, String)>,
) -> Res {
    stats_access(&st, &s, guild).await?;
    let user = user_id(&user)?;
    let rows: Option<Value> = sqlx::query_scalar(
        "select jsonb_agg(r order by r.at desc, r.id desc) from (
            select id, kind, source, target_id::text as target_id, case_no, created_at as at from mod_actions
            where guild_id = $1 and mod_id = $2 order by created_at desc, id desc limit 50) r",
    )
    .bind(guild)
    .bind(user)
    .fetch_one(&st.db)
    .await?;
    Ok(Json(rows.unwrap_or_else(|| json!([]))))
}

#[derive(Deserialize)]
struct NoteFilter {
    q: Option<String>,
    #[serde(default)]
    page: i64,
}

async fn notes(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Query(f): Query<NoteFilter>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_view", "notes_manage"]).await?;
    let rows: Vec<(String, String, String, String, At, Value)> = sqlx::query_as(
        "select short_id, user_id::text, author_id::text, text, created_at,
                (select coalesce(json_agg(json_build_object('at', e.edited_at, 'by', e.editor_id::text,
                        'old', e.old_text, 'new', e.new_text) order by e.id), '[]'::json)
                 from note_edits e where e.note_id = notes.id)
         from notes
         where guild_id = $1 and ($2 = '' or user_id::text = $2) order by id desc limit 50 offset $3",
    )
    .bind(guild)
    .bind(f.q.unwrap_or_default().trim())
    .bind(offset(f.page))
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(id, user, author, text, at, edits)| json!({"id": id, "user_id": user, "author_id": author, "text": text, "at": at, "edits": edits}))
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct NoteBody {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    text: String,
    /// Sweep: every note of the server instead of one member's.
    #[serde(default)]
    all: bool,
}

/// The bot sends the private mod-log entry (the dashboard has no Discord connection of its own).
/// The note change is already saved, so a failed enqueue is only logged: a retry would repeat it.
async fn note_log(st: &AppState, guild: i64, log: NoteLog) {
    let at = chrono::Utc::now().timestamp();
    if let Err(e) = jobs::enqueue(&st.db, guild, "note_log", json!(log), at).await {
        tracing::warn!(?e, guild, "queueing the note log failed");
    }
}

async fn note_add(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    let (user, text) = (user_id(&b.user_id)?, clip(&b.text, NOTE_MAX)?);
    let (id,): (String,) = sqlx::query_as(
        "insert into notes (guild_id, user_id, author_id, text) values ($1,$2,$3,$4) returning short_id",
    )
    .bind(guild)
    .bind(user)
    .bind(s.user_id)
    .bind(&text)
    .fetch_one(&st.db)
    .await?;
    let log = NoteLog {
        action: NoteAction::Add,
        id: Some(id),
        user: Some(user as u64),
        by: s.user_id as u64,
        text: Some(text),
        ..NoteLog::default()
    };
    note_log(&st, guild, log).await;
    ok()
}

async fn note_edit(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, String)>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    let text = clip(&b.text, NOTE_MAX)?;
    let (user, old) = notes::edit(&st.db, guild, &id, &text, s.user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let log = NoteLog {
        action: NoteAction::Modify,
        id: Some(id),
        user: Some(user as u64),
        by: s.user_id as u64,
        text: Some(text),
        old: Some(old),
        ..NoteLog::default()
    };
    note_log(&st, guild, log).await;
    ok()
}

async fn note_delete(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, id)): Path<(i64, String)>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    let gone: Option<(i64, String)> = sqlx::query_as(
        "delete from notes where guild_id = $1 and short_id = $2 returning user_id, text",
    )
    .bind(guild)
    .bind(&id)
    .fetch_optional(&st.db)
    .await?;
    if let Some((user, text)) = gone {
        let log = NoteLog {
            action: NoteAction::Remove,
            id: Some(id),
            user: Some(user as u64),
            by: s.user_id as u64,
            text: Some(text),
            ..NoteLog::default()
        };
        note_log(&st, guild, log).await;
    }
    ok()
}

async fn note_sweep(
    State(st): State<AppState>,
    s: Sess,
    Path(guild): Path<i64>,
    Json(b): Json<NoteBody>,
) -> Res {
    api::staff(&st, &s, guild, &["notes_manage"]).await?;
    // The page asks before a sweep of the whole server; `all` must be explicit, an empty id is an error.
    let user = if b.all {
        None
    } else {
        Some(user_id(&b.user_id)?)
    };
    let n = sqlx::query(
        "delete from notes where guild_id = $1 and ($2::bigint is null or user_id = $2)",
    )
    .bind(guild)
    .bind(user)
    .execute(&st.db)
    .await?
    .rows_affected();
    let log = NoteLog {
        action: NoteAction::Sweep,
        user: user.map(|u| u as u64),
        by: s.user_id as u64,
        count: Some(n),
        ..NoteLog::default()
    };
    note_log(&st, guild, log).await;
    Ok(Json(json!({"removed": n})))
}

#[derive(Deserialize)]
struct ReportFilter {
    /// "open", "dismissed", "actioned" or anything else for all
    state: Option<String>,
    #[serde(default)]
    page: i64,
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
         order by (status = 'open') desc, id desc limit 50 offset $3",
    )
    .bind(guild)
    .bind(f.state.unwrap_or_default())
    .bind(offset(f.page))
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
    fn mod_stats_rows() {
        let at = |d: i64| chrono::Utc::now() - chrono::Duration::days(d);
        let v = stats_json(
            7,
            1,
            vec![
                ("1".into(), "ban".into(), 3, 1),
                ("1".into(), "warn".into(), 0, 2),
                ("2".into(), "kick".into(), 5, 0),
            ],
            vec![
                ("1".into(), at(0)),
                ("2".into(), at(1)),
                ("3".into(), at(200)),
            ],
            vec![("1".into(), 0, 2), ("1".into(), 6, 1), ("2".into(), 9, 5)],
        );
        assert_eq!(
            stats_json(
                365,
                7,
                vec![],
                vec![("1".into(), chrono::Utc::now())],
                vec![("1".into(), 52, 4)]
            )["mods"][0]["series"]
                .as_array()
                .map(|s| (s.len(), s[0].clone())),
            Some((52, json!(4))),
            "a year is 52 whole weeks; the extra day joins the oldest"
        );
        let m = v["mods"].as_array().unwrap();
        assert_eq!(
            v["kinds"],
            json!(["ban", "kick"]),
            "a kind with nothing in the period gets no column"
        );
        assert_eq!(m[0]["mod_id"], "2");
        assert_eq!(
            m[0]["series"],
            json!([5, 0, 0, 0, 0, 0, 0]),
            "a late bar joins the oldest"
        );
        assert_eq!(m[1]["mod_id"], "1");
        assert_eq!(m[1]["by_kind"], json!({"ban": 3}));
        assert_eq!(
            (m[1]["total"].as_i64(), m[1]["prev_total"].as_i64()),
            (Some(3), Some(3))
        );
        assert_eq!(m[1]["series"], json!([1, 0, 0, 0, 0, 0, 2]));
        assert_eq!(
            m[2]["mod_id"], "3",
            "quiet staff stay listed, with nothing in the period"
        );
        assert_eq!(m[2]["total"], 0);
    }

    #[test]
    fn mod_stats_periods() {
        assert_eq!(period(None).ok(), Some((30, 1)));
        assert_eq!(period(Some("365")).ok(), Some((365, 7)));
        assert!(period(Some("31")).is_err());
        assert!(period(Some("-7")).is_err());
    }

    #[test]
    fn input_checks() {
        assert!(user_id(" 123 ").is_ok());
        assert!(user_id("abc").is_err());
        assert!(clip("  ", 5).is_err());
        assert!(clip("123456", 5).is_err());
        assert_eq!(clip(" hi ", 5).unwrap(), "hi");
    }
}
