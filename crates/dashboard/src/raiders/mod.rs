//! Raider List: the bot owner's review (queue, listed entries, appeals and the decisions), the server
//! owner's Report button on a case, and a listed user's own appeal page. Nothing here bans or unbans:
//! a decision queues a `raider_check` or `raider_lift` job for the bot in the same transaction as the
//! status change, so the bot's exemptions and checks apply to dashboard decisions too.
use crate::{api::Res, auth::Sess, ApiError, AppState};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tommybot_core::{jobs, limits};

type At = chrono::DateTime<chrono::Utc>;

const NOTE_MAX: usize = 200;
const APPEAL_MAX: usize = 1000;
const APPEAL_WAIT_DAYS: i32 = 30;
/// Evidence rows shown per entry; the newest ones.
const EVIDENCE_SHOWN: i64 = 25;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/raiders/queue", get(queue))
        .route("/api/raiders/listed", get(listed))
        .route("/api/raiders/appeals", get(appeals))
        .route("/api/raiders/appeals/{id}/{decision}", post(appeal::decide))
        .route("/api/raiders/{user}/{decision}", post(decide))
        .route(
            "/api/raider-appeal",
            get(appeal::my_entry).post(appeal::my_appeal),
        )
        .route(
            "/api/guilds/{guild}/cases/{no}/raider_report",
            post(report::report),
        )
}

/// Every review route: the bot owner only, and a fresh 2FA step-up for every write.
async fn bot_owner(st: &AppState, s: &Sess, step_up: bool) -> Result<(), ApiError> {
    if !crate::update::is_bot_owner(st, s).await {
        return Err(ApiError::Forbidden(
            "only the bot owner can review the Raider List",
        ));
    }
    if step_up && !s.stepped_up {
        return Err(ApiError::StepUp);
    }
    Ok(())
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::BadRequest(m.into())
}

fn category(c: &str) -> Result<(), ApiError> {
    if ["nuke", "raid", "scam", "spam"].contains(&c) {
        Ok(())
    } else {
        Err(bad("category must be nuke, raid, scam or spam"))
    }
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    page: i64,
    #[serde(default)]
    q: String,
}

/// When a Discord id was made (its snowflake time), as RFC 3339.
fn id_time(id: i64) -> Option<String> {
    let ms = (id >> 22) + 1_420_070_400_000;
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

/// The user (name, avatar, account age), every shown evidence row with its server's name and size,
/// and the number of distinct server owners, for each of `users`.
async fn describe(st: &AppState, users: &[i64]) -> Result<HashMap<i64, Value>, ApiError> {
    type Ev = (i64, i64, i64, String, String, Value, At, Option<String>);
    let rows: Vec<Ev> = sqlx::query_as(
        "select user_id, source_guild, source_owner, kind, ref, detail, created_at, note from (
            select *, row_number() over (partition by user_id order by created_at desc, id desc) n
            from raider_evidence where user_id = any($1)) e
         where n <= $2 order by user_id, created_at desc, id desc",
    )
    .bind(users)
    .bind(EVIDENCE_SHOWN)
    .fetch_all(&st.db)
    .await?;
    let counts: Vec<(i64, i64, i64)> = sqlx::query_as(
        "select user_id, count(distinct source_owner), count(*) from raider_evidence
         where user_id = any($1) group by user_id",
    )
    .bind(users)
    .fetch_all(&st.db)
    .await?;
    let mut servers = HashMap::new();
    for g in rows.iter().map(|r| r.1) {
        if let std::collections::hash_map::Entry::Vacant(e) = servers.entry(g) {
            let (name, members) = st
                .discord
                .guild_size(g)
                .await?
                .unwrap_or_else(|| (g.to_string(), 0));
            e.insert((name, members));
        }
    }
    let mut out = HashMap::new();
    for &u in users {
        let who = st.discord.user(u).await?.unwrap_or(Value::Null);
        let evidence: Vec<Value> = rows
            .iter()
            .filter(|r| r.0 == u)
            .map(|(_, g, _, kind, r, detail, at, note)| {
                let (name, members) = &servers[g];
                // The reporter's raw words: the page shows them as text, never as HTML.
                json!({"server_id": g.to_string(), "server": name, "members": members, "kind": kind,
                    "case_no": detail["case_no"], "ref": r, "detail": detail, "at": at, "note": note})
            })
            .collect();
        let (owners, total) = counts
            .iter()
            .find(|c| c.0 == u)
            .map_or((0, 0), |c| (c.1, c.2));
        out.insert(
            u,
            json!({
                "name": who["global_name"].as_str().or(who["username"].as_str()),
                "username": who["username"], "avatar": who["avatar"], "bot": who["bot"] == true,
                "created_at": id_time(u), "evidence": evidence, "evidence_total": total, "owners": owners,
            }),
        );
    }
    Ok(out)
}

/// Pending entries and automatic listings the bot owner has not verified yet (these first), then by
/// the number of distinct owners and the newest evidence.
async fn queue(State(st): State<AppState>, s: Sess, Query(p): Query<Page>) -> Res {
    bot_owner(&st, &s, false).await?;
    let rows: Vec<(i64, String, String, bool, At)> = sqlx::query_as(
        "select r.user_id, r.status, r.category, r.verified, r.created_at from raiders r
         where r.status = 'pending' or (r.status = 'confirmed' and not r.verified)
         order by r.status = 'confirmed' desc,
            (select count(distinct source_owner) from raider_evidence e where e.user_id = r.user_id) desc,
            (select max(created_at) from raider_evidence e where e.user_id = r.user_id) desc nulls last,
            r.user_id
         limit 50 offset $1",
    )
    .bind(crate::records::offset(p.page))
    .fetch_all(&st.db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let mut info = describe(&st, &ids).await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(u, status, category, verified, at)| {
            let mut v = info.remove(&u).unwrap_or_default();
            v["user_id"] = json!(u.to_string());
            v["status"] = json!(status);
            v["category"] = json!(category);
            v["verified"] = json!(verified);
            v["listed_at"] = json!(at);
            v
        })
        .collect::<Vec<_>>())))
}

/// Confirmed entries, newest change first; `q` matches the user id or the public note.
async fn listed(State(st): State<AppState>, s: Sess, Query(p): Query<Page>) -> Res {
    bot_owner(&st, &s, false).await?;
    type Row = (
        String,
        String,
        bool,
        String,
        Option<At>,
        Option<At>,
        At,
        i64,
        i64,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select r.user_id::text, r.category, r.verified, r.public_note, r.reviewed_at, r.expires_at, r.updated_at,
            (select count(*) from raider_evidence e where e.user_id = r.user_id),
            (select count(distinct source_owner) from raider_evidence e where e.user_id = r.user_id)
         from raiders r
         where r.status = 'confirmed'
           and ($1 = '' or r.user_id::text = $1 or r.public_note ilike '%' || $1 || '%')
         order by r.updated_at desc, r.user_id limit 50 offset $2",
    )
    .bind(p.q.trim())
    .bind(crate::records::offset(p.page))
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(u, category, verified, note, reviewed, expires, at, evidence, owners)| json!({
            "user_id": u, "category": category, "verified": verified, "public_note": note,
            "reviewed_at": reviewed, "expires_at": expires, "updated_at": at, "evidence": evidence, "owners": owners
        }))
        .collect::<Vec<_>>())))
}

/// Open appeals, oldest first, next to the entry and its evidence.
async fn appeals(State(st): State<AppState>, s: Sess, Query(p): Query<Page>) -> Res {
    bot_owner(&st, &s, false).await?;
    type Row = (
        i64,
        i64,
        String,
        At,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select a.id, a.user_id, a.text, a.created_at, r.status, r.category, r.public_note
         from raider_appeals a left join raiders r on r.user_id = a.user_id
         where a.status = 'open' order by a.created_at, a.id limit 50 offset $1",
    )
    .bind(crate::records::offset(p.page))
    .fetch_all(&st.db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.1).collect();
    let info = describe(&st, &ids).await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(id, u, text, at, status, category, note)| {
            let mut v = info.get(&u).cloned().unwrap_or_default();
            v["id"] = json!(id);
            v["user_id"] = json!(u.to_string());
            v["text"] = json!(text);
            v["at"] = json!(at);
            v["status"] = json!(status);
            v["category"] = json!(category);
            v["public_note"] = json!(note);
            v
        })
        .collect::<Vec<_>>())))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Decision {
    category: String,
    note: String,
}

fn job(user: i64) -> Value {
    json!({"user": user.to_string()})
}

/// The bot DMs `to` this text (`raider_notify`), queued with the decision so it is sent only if the
/// decision commits.
async fn notify(tx: &mut sqlx::PgConnection, to: i64, text: String) -> Result<(), ApiError> {
    let p = json!({"user": to.to_string(), "text": text});
    jobs::enqueue(&mut *tx, 0, "raider_notify", p, now()).await?;
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

async fn review(
    tx: &mut sqlx::PgConnection,
    actor: i64,
    user: i64,
    from: Option<&str>,
    to: &str,
    note: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "insert into raider_reviews (actor, user_id, from_status, to_status, note) values ($1, $2, $3, $4, $5)",
    )
    .bind(actor)
    .bind(user)
    .bind(from)
    .bind(to)
    .bind(note)
    .execute(tx)
    .await?;
    Ok(())
}

const CHANGED: ApiError = ApiError::Busy("this entry changed since the page loaded; reload it");

/// Confirm, Verify, Reject or Remove one entry. The status change, its review row and the bot's job
/// commit together, or not at all.
async fn decide(
    State(st): State<AppState>,
    s: Sess,
    Path((user, what)): Path<(i64, String)>,
    Json(b): Json<Decision>,
) -> Res {
    bot_owner(&st, &s, true).await?;
    let mut tx = st.db.begin().await?;
    let cur: Option<(String, bool)> =
        sqlx::query_as("select status, verified from raiders where user_id = $1 for update")
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((from, verified)) = cur else {
        return Err(ApiError::NotFound);
    };
    let ok = |want: bool| if want { Ok(()) } else { Err(CHANGED) };
    let set = |to: &'static str| {
        sqlx::query(
            "update raiders set status = $2, reviewed_by = $3, reviewed_at = now(), updated_at = now() where user_id = $1",
        )
        .bind(user)
        .bind(to)
        .bind(s.user_id)
    };
    let (to, note) = match what.as_str() {
        // An automatic listing can be confirmed too, to set its category and note.
        "confirm" => {
            ok(from == "pending" || (from == "confirmed" && !verified))?;
            category(&b.category)?;
            let note = b.note.trim();
            if note.chars().count() > NOTE_MAX {
                return Err(bad(format!(
                    "the public note is longer than {NOTE_MAX} characters"
                )));
            }
            // A scam listing ends by itself after its last evidence (often a hacked account).
            sqlx::query(
                "update raiders set status = 'confirmed', verified = true, category = $2, public_note = $3,
                    reviewed_by = $4, reviewed_at = now(), updated_at = now(),
                    expires_at = case when $2 = 'scam' then greatest(
                        (select max(created_at) from raider_evidence where user_id = $1), now()) + make_interval(days => $5) end
                 where user_id = $1",
            )
            .bind(user)
            .bind(&b.category)
            .bind(note)
            .bind(s.user_id)
            .bind(limits::RAIDER_SCAM_DAYS as i32)
            .execute(&mut *tx)
            .await?;
            jobs::enqueue(&mut *tx, 0, "raider_check", job(user), now()).await?;
            ("confirmed", note.to_string())
        }
        "verify" => {
            ok(from == "confirmed" && !verified)?;
            sqlx::query(
                "update raiders set verified = true, reviewed_by = $2, reviewed_at = now(), updated_at = now() where user_id = $1",
            )
            .bind(user)
            .bind(s.user_id)
            .execute(&mut *tx)
            .await?;
            ("confirmed", "verified".to_string())
        }
        "reject" => {
            ok(from == "pending")?;
            set("rejected").execute(&mut *tx).await?;
            ("rejected", String::new())
        }
        "remove" => {
            ok(from == "confirmed")?;
            set("removed").execute(&mut *tx).await?;
            jobs::enqueue(&mut *tx, 0, "raider_lift", job(user), now()).await?;
            ("removed", String::new())
        }
        _ => return Err(ApiError::NotFound),
    };
    review(&mut tx, s.user_id, user, Some(&from), to, &note).await?;
    // The bot owner's log of the decisions that change who is enforced.
    if matches!(what.as_str(), "confirm" | "remove") {
        let text = format!(
            "Raider List log: <@{user}> ({user}) is now {to}, by <@{}>.",
            s.user_id
        );
        notify(&mut tx, s.user_id, text).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "status": to})))
}

mod appeal;
mod report;
#[cfg(test)]
mod tests;
