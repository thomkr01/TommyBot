//! Raider List: the bot owner's review (queue, listed entries, appeals and the decisions), the server
//! owner's Report button on a case, and a listed user's own appeal page. Nothing here bans or unbans:
//! a decision queues a `raider_check` or `raider_lift` job for the bot in the same transaction as the
//! status change, so the bot's exemptions and checks apply to dashboard decisions too.
use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    store, ApiError, AppState,
};
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

const CATEGORIES: [&str; 4] = ["nuke", "raid", "scam", "spam"];
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
        .route("/api/raiders/appeals/{id}/{decision}", post(appeal_decide))
        .route("/api/raiders/{user}/{decision}", post(decide))
        .route("/api/raider-appeal", get(my_entry).post(my_appeal))
        .route("/api/guilds/{guild}/cases/{no}/raider_report", post(report))
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
    type Ev = (i64, i64, i64, String, String, Value, At);
    let rows: Vec<Ev> = sqlx::query_as(
        "select user_id, source_guild, source_owner, kind, ref, detail, created_at from (
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
            .map(|(_, g, _, kind, r, detail, at)| {
                let (name, members) = &servers[g];
                json!({"server_id": g.to_string(), "server": name, "members": members, "kind": kind,
                    "case_no": detail["case_no"], "ref": r, "detail": detail, "at": at})
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
            if !CATEGORIES.contains(&b.category.as_str()) {
                return Err(bad("category must be nuke, raid, scam or spam"));
            }
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
                    expires_at = case when $2 = 'scam' then coalesce(
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
    tx.commit().await?;
    Ok(Json(json!({"ok": true, "status": to})))
}

/// Accept (takes the user off the list and lifts what it did) or Deny (the next appeal opens in 30 days).
async fn appeal_decide(
    State(st): State<AppState>,
    s: Sess,
    Path((id, what)): Path<(i64, String)>,
) -> Res {
    bot_owner(&st, &s, true).await?;
    let to_appeal = match what.as_str() {
        "accept" => "accepted",
        "deny" => "denied",
        _ => return Err(ApiError::NotFound),
    };
    let mut tx = st.db.begin().await?;
    let user: i64 = sqlx::query_scalar(
        "update raider_appeals set status = $2, decided_by = $3, decided_at = now()
         where id = $1 and status = 'open' returning user_id",
    )
    .bind(id)
    .bind(to_appeal)
    .bind(s.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(CHANGED)?;
    let from: Option<String> =
        sqlx::query_scalar("select status from raiders where user_id = $1 for update")
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    let to = if to_appeal == "accepted" {
        sqlx::query(
            "update raiders set status = 'removed', reviewed_by = $2, reviewed_at = now(), updated_at = now() where user_id = $1",
        )
        .bind(user)
        .bind(s.user_id)
        .execute(&mut *tx)
        .await?;
        if from.as_deref() == Some("confirmed") {
            jobs::enqueue(&mut *tx, 0, "raider_lift", job(user), now()).await?;
        }
        "removed".to_string()
    } else {
        sqlx::query(
            "update raiders set appeal_after = now() + make_interval(days => $2), updated_at = now() where user_id = $1",
        )
        .bind(user)
        .bind(APPEAL_WAIT_DAYS)
        .execute(&mut *tx)
        .await?;
        from.clone().unwrap_or_else(|| "removed".into())
    };
    let note = format!("appeal {to_appeal}");
    review(&mut tx, s.user_id, user, from.as_deref(), &to, &note).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

// ---- The listed user's own page. The user is always the session's, never one from the request. ----

async fn my_entry(State(st): State<AppState>, s: Sess) -> Res {
    type Entry = (String, String, String, Option<At>, Option<At>);
    let row: Option<Entry> = sqlx::query_as(
        "select status, category, public_note, appeal_after, expires_at from raiders where user_id = $1",
    )
    .bind(s.user_id)
    .fetch_optional(&st.db)
    .await?;
    // Pending and rejected entries are not enforced, so they are shown as "not listed".
    let Some((status, category, note, after, expires)) =
        row.filter(|r| matches!(r.0.as_str(), "confirmed" | "removed"))
    else {
        return Ok(Json(json!({"listed": false})));
    };
    let last: Option<(String, At)> = sqlx::query_as(
        "select status, created_at from raider_appeals where user_id = $1 order by id desc limit 1",
    )
    .bind(s.user_id)
    .fetch_optional(&st.db)
    .await?;
    let listed = status == "confirmed";
    let open = last.as_ref().is_some_and(|l| l.0 == "open");
    let waited = after.is_none_or(|a| a <= chrono::Utc::now());
    Ok(Json(json!({
        "listed": listed, "status": status, "category": category, "public_note": note,
        "expires_at": expires, "appeal_after": after, "can_appeal": listed && !open && waited,
        "appeal": last.map(|(st, at)| json!({"status": st, "at": at})),
    })))
}

/// Only `text` is read: a `user_id` in the body is ignored.
#[derive(Deserialize)]
struct Appeal {
    text: String,
}

async fn my_appeal(State(st): State<AppState>, s: Sess, Json(b): Json<Appeal>) -> Res {
    let text = b.text.trim();
    let n = text.chars().count();
    if n == 0 || n > APPEAL_MAX {
        return Err(bad(format!(
            "the appeal must be 1 to {APPEAL_MAX} characters"
        )));
    }
    let mut tx = st.db.begin().await?;
    let row: Option<(String, bool)> = sqlx::query_as(
        "select status, appeal_after is null or appeal_after <= now() from raiders where user_id = $1 for update",
    )
    .bind(s.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    match row {
        Some((status, _)) if status != "confirmed" => {
            return Err(bad("you are not on the Raider List"))
        }
        None => return Err(bad("you are not on the Raider List")),
        Some((_, false)) => {
            return Err(bad(
                "your last appeal was denied; you can appeal again 30 days after that",
            ))
        }
        Some(_) => {}
    }
    let id: Option<i64> = sqlx::query_scalar(
        "insert into raider_appeals (user_id, text) values ($1, $2)
         on conflict (user_id) where status = 'open' do nothing returning id",
    )
    .bind(s.user_id)
    .bind(text)
    .fetch_optional(&mut *tx)
    .await?;
    if id.is_none() {
        return Err(bad("you already have an open appeal"));
    }
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

// ---- Report to Raider List, from a case ----

#[derive(Deserialize)]
struct Report {
    category: String,
}

/// Adds a report to the user's entry, which stays (or becomes) pending: only the bot owner confirms a
/// report. Needs a ban, kick or quarantine case here, a server with 100 members that has had the bot
/// for 7 days, and at most 10 reports per Discord owner per day across all their servers.
async fn report(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, no)): Path<(i64, i32)>,
    Json(b): Json<Report>,
) -> Res {
    require(&st, &s, guild, Level::Owner, true).await?;
    if !CATEGORIES.contains(&b.category.as_str()) {
        return Err(bad("category must be nuke, raid, scam or spam"));
    }
    let case: Option<(String, i64)> =
        sqlx::query_as("select kind, user_id from cases where guild_id = $1 and case_no = $2")
            .bind(guild)
            .bind(no)
            .fetch_optional(&st.db)
            .await?;
    let Some((kind, user)) = case else {
        return Err(ApiError::NotFound);
    };
    if !matches!(kind.as_str(), "ban" | "kick" | "quarantine") {
        return Err(bad("only ban, kick and quarantine cases can be reported"));
    }
    let owner = st.discord.guild_owner(guild).await?;
    let staff: bool = sqlx::query_scalar(
        "select exists(select 1 from extra_owners where guild_id = $1 and user_id = $2)
            or exists(select 1 from trusted_admins where guild_id = $1 and user_id = $2)",
    )
    .bind(guild)
    .bind(user)
    .fetch_one(&st.db)
    .await?;
    let u = user.to_string();
    if staff || u == owner || u == st.discord.bot_owner().await? || u == st.discord.bot_id().await?
    {
        return Err(bad(
            "the server's owners, trusted admins, the bot owner and TommyBot cannot be reported",
        ));
    }
    let members = st.discord.guild_size(guild).await?.map_or(0, |g| g.1);
    if members < limits::RAIDER_MIN_MEMBERS {
        return Err(bad(format!(
            "a server needs at least {} members to report",
            limits::RAIDER_MIN_MEMBERS
        )));
    }
    let joined = st
        .discord
        .bot_joined(guild)
        .await?
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok());
    let since = chrono::Utc::now() - chrono::Duration::days(limits::RAIDER_MIN_DAYS);
    if joined.is_none_or(|t| t > since) {
        return Err(bad(format!(
            "TommyBot needs to be in the server for {} days before it can report",
            limits::RAIDER_MIN_DAYS
        )));
    }
    let owner: i64 = owner
        .parse()
        .map_err(|_| bad("this server's owner is unknown"))?;
    let mut tx = st.db.begin().await?;
    // One owner's reports are counted one at a time, so parallel reports cannot pass the cap.
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    let today: i64 = sqlx::query_scalar(
        "select count(*) from raider_evidence
         where source_owner = $1 and kind = 'report' and created_at > now() - interval '1 day'",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    if today >= limits::RAIDER_REPORTS_DAY {
        return Err(bad(format!(
            "the owner of this server already sent {} reports in the last 24 hours",
            limits::RAIDER_REPORTS_DAY
        )));
    }
    let added: Option<i64> = sqlx::query_scalar(
        "insert into raider_evidence (user_id, source_guild, source_owner, kind, ref, detail, reporter_id)
         values ($1, $2, $3, 'report', $4, $5, $6) on conflict do nothing returning id",
    )
    .bind(user)
    .bind(guild)
    .bind(owner)
    .bind(no.to_string())
    .bind(json!({"case_no": no, "case_kind": kind}))
    .bind(s.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if added.is_none() {
        return Err(bad("this case was already reported"));
    }
    // A report never confirms: a new entry is pending, and a rejected or removed one is pending again.
    sqlx::query(
        "insert into raiders (user_id, status, category) values ($1, 'pending', $2)
         on conflict (user_id) do update set
            verified = raiders.verified and raiders.status not in ('rejected', 'removed'),
            status = case when raiders.status in ('rejected', 'removed') then 'pending' else raiders.status end,
            updated_at = now()",
    )
    .bind(user)
    .bind(&b.category)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let (_, config) = store::current(&st.db, guild).await?;
    let text = format!(
        "{} (<@{}>) reported <@{user}> (case #{no}) to the Raider List as {}",
        s.username, s.user_id, b.category
    );
    log_change(&st, guild, &config, text).await;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[test]
    fn id_time_reads_the_snowflake() {
        // Discord's documented example id.
        assert!(id_time(175_928_847_299_117_063)
            .unwrap()
            .starts_with("2016-04-30T11:18:25.796"));
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres. Discord is a local mock.
    #[tokio::test]
    async fn review_report_and_appeal() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let db = tommybot_core::db::connect(&url).await.unwrap();
        let pid = std::process::id() as i64;
        let base = 9_610_000_000_000 + pid * 100;
        let (owner, server_owner, bot) = (base + 1, base + 2, base + 3);
        // big and old; too small; too new
        let (g1, g2, g3) = (base + 10, base + 11, base + 12);
        let users: Vec<i64> = (base + 50..base + 70).collect();
        let uid0 = users[0].to_string();
        let mock = Router::new()
            .route(
                "/applications/@me",
                get(move || async move { Json(json!({"owner": {"id": owner.to_string()}})) }),
            )
            .route(
                "/users/@me",
                get(move || async move { Json(json!({"id": bot.to_string()})) }),
            )
            .route(
                "/users/{u}",
                get(|Path(u): Path<String>| async move {
                    Json(json!({"id": u, "username": format!("u{u}")}))
                }),
            )
            .route(
                "/guilds/{g}",
                get(move |Path(g): Path<i64>| async move {
                    let members = if g == g2 { 50 } else { 150 };
                    Json(json!({"id": g.to_string(), "name": format!("Server {g}"),
                        "owner_id": server_owner.to_string(), "approximate_member_count": members}))
                }),
            )
            .route(
                "/guilds/{g}/members/{u}",
                get(move |Path((g, _)): Path<(i64, String)>| async move {
                    let days = if g == g3 { 1 } else { 30 };
                    let at = chrono::Utc::now() - chrono::Duration::days(days);
                    Json(json!({"roles": [], "joined_at": at.to_rfc3339()}))
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
        };
        let send = |m: &str, path: String, user: i64, up: bool, body: Option<Value>| {
            let mut req = Request::builder()
                .method(m)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                .unwrap();
            req.extensions_mut().insert(Sess {
                id: String::new(),
                user_id: user,
                username: "t".into(),
                access_token: String::new(),
                stepped_up: up,
                csrf: String::new(),
            });
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
        let post =
            |path: String, user: i64, body: Value| send("POST", path, user, true, Some(body));
        let get_ = |path: &str, user: i64| send("GET", path.to_string(), user, false, None);
        let exec = |sql: &'static str, a: i64, b: i64| {
            let db = db.clone();
            async move {
                sqlx::query(sql).bind(a).bind(b).execute(&db).await.unwrap();
            }
        };
        let status = |u: i64| {
            let db = db.clone();
            async move {
                sqlx::query_as::<_, (String, bool, Option<At>)>(
                    "select status, verified, expires_at from raiders where user_id = $1",
                )
                .bind(u)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let jobs_for = |u: i64| {
            let db = db.clone();
            async move {
                sqlx::query_as::<_, (i64, String)>(
                    "select guild_id, kind from scheduled_jobs where payload->>'user' = $1 order by id",
                )
                .bind(u.to_string())
                .fetch_all(&db)
                .await
                .unwrap()
            }
        };
        for (i, g) in [g1, g2, g3].into_iter().enumerate() {
            for (no, kind) in [(1, "ban"), (2, "warn"), (3, "kick")] {
                sqlx::query(
                    "insert into cases (guild_id, case_no, kind, user_id, mod_id, reason)
                     values ($1, $2, $3, $4, 7, 'raid')",
                )
                .bind(g)
                .bind(no)
                .bind(kind)
                .bind(users[i])
                .execute(&db)
                .await
                .unwrap();
            }
        }

        // S13: every review route is the bot owner's, and every write needs 2FA.
        for p in ["queue", "listed", "appeals"] {
            let path = format!("/api/raiders/{p}");
            assert_eq!(get_(&path, server_owner).await.0, 403, "{p}");
            assert_eq!(get_(&path, owner).await.0, 200, "{p}");
        }
        for w in [
            format!("/api/raiders/{}/confirm", users[0]),
            format!("/api/raiders/{}/verify", users[0]),
            format!("/api/raiders/{}/reject", users[0]),
            format!("/api/raiders/{}/remove", users[0]),
            "/api/raiders/appeals/1/accept".to_string(),
            "/api/raiders/appeals/1/deny".to_string(),
        ] {
            let body = json!({"category": "raid"});
            assert_eq!(
                post(w.clone(), server_owner, body.clone()).await.0,
                403,
                "{w}"
            );
            let (code, r) = send("POST", w.clone(), owner, false, Some(body)).await;
            assert_eq!(
                (code, r["error"].as_str()),
                (403, Some("step_up_required")),
                "{w}"
            );
        }

        // Report: owner + 2FA, a ban/kick/quarantine case, 100 members, 7 days, and never confirmed.
        let report = |g: i64, no: i32| format!("/api/guilds/{g}/cases/{no}/raider_report");
        let raid = json!({"category": "raid"});
        let (code, r) = send(
            "POST",
            report(g1, 1),
            server_owner,
            false,
            Some(raid.clone()),
        )
        .await;
        assert_eq!((code, r["error"].as_str()), (403, Some("step_up_required")));
        assert_eq!(
            post(report(g1, 1), owner, raid.clone()).await.0,
            403,
            "not an owner here"
        );
        assert_eq!(
            post(report(g1, 2), server_owner, raid.clone()).await.0,
            400,
            "a warn case"
        );
        assert_eq!(
            post(report(g2, 1), server_owner, raid.clone()).await.0,
            400,
            "50 members"
        );
        let (code, r) = post(report(g3, 1), server_owner, raid.clone()).await;
        assert_eq!(code, 400, "bot added yesterday: {r}");
        assert_eq!(
            post(report(g1, 1), server_owner, json!({"category": "x"}))
                .await
                .0,
            400
        );
        let (code, r) = post(report(g1, 1), server_owner, raid.clone()).await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(status(users[0]).await.0, "pending");
        assert_eq!(
            post(report(g1, 1), server_owner, raid.clone()).await.0,
            400,
            "same case twice"
        );
        // S5: ten reports a day per owner, across all their servers.
        for u in &users[10..19] {
            exec(
                "insert into raider_evidence (user_id, source_guild, source_owner, kind, ref) values ($1, 1, $2, 'report', 'x')",
                *u,
                server_owner,
            )
            .await;
        }
        let (code, r) = post(report(g1, 3), server_owner, raid.clone()).await;
        assert_eq!(code, 400, "{r}");
        assert!(r["error"].as_str().unwrap().contains("10 reports"), "{r}");
        exec(
            "delete from raider_evidence where source_owner = $2 and ref = 'x' and user_id <> $1",
            0,
            server_owner,
        )
        .await;

        // The queue shows the entry with its evidence (server name and size, case) and distinct owners.
        let (_, q) = get_("/api/raiders/queue", owner).await;
        let e = q
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["user_id"] == uid0)
            .unwrap()
            .clone();
        let ev = &e["evidence"][0];
        assert_eq!(e["owners"].as_i64(), Some(1));
        assert_eq!(ev["server"].as_str(), Some(format!("Server {g1}").as_str()));
        assert_eq!(
            (ev["case_no"].as_i64(), ev["members"].as_i64()),
            (Some(1), Some(150))
        );
        assert_eq!(e["username"], format!("u{}", users[0]));

        // Confirm: status, review row and the bot's job together. Scam listings expire.
        let confirm = format!("/api/raiders/{}/confirm", users[0]);
        let long = json!({"category": "raid", "note": "x".repeat(201)});
        assert_eq!(post(confirm.clone(), owner, long).await.0, 400);
        let scam = json!({"category": "scam", "note": "Phishing links"});
        let (code, r) = post(confirm.clone(), owner, scam).await;
        assert_eq!(code, 200, "{r}");
        let (now_status, verified, expires) = status(users[0]).await;
        assert!(now_status == "confirmed" && verified && expires.is_some());
        assert_eq!(jobs_for(users[0]).await, [(0, "raider_check".to_string())]);
        assert_eq!(
            post(confirm, owner, raid.clone()).await.0,
            409,
            "already confirmed"
        );
        // A report on a confirmed entry keeps it confirmed.
        exec(
            "delete from raider_evidence where user_id = $1 and source_guild = $2",
            users[0],
            g1,
        )
        .await;
        assert_eq!(post(report(g1, 1), server_owner, raid.clone()).await.0, 200);
        assert_eq!(status(users[0]).await.0, "confirmed");

        // Verify works only on an automatic listing.
        exec(
            "insert into raiders (user_id, status, category, verified) values ($1, 'confirmed', 'nuke', $2 = 1)",
            users[1],
            0,
        )
        .await;
        let verify = format!("/api/raiders/{}/verify", users[1]);
        assert_eq!(post(verify.clone(), owner, json!({})).await.0, 200);
        assert!(status(users[1]).await.1);
        assert_eq!(post(verify, owner, json!({})).await.0, 409);
        // Reject only from pending.
        exec(
            "insert into raiders (user_id, status, category) values ($1, 'pending', 'spam') on conflict do nothing",
            users[2],
            0,
        )
        .await;
        let reject = format!("/api/raiders/{}/reject", users[2]);
        assert_eq!(post(reject, owner, json!({})).await.0, 200);
        assert_eq!(status(users[2]).await.0, "rejected");
        assert!(jobs_for(users[2]).await.is_empty());

        // The listed user's page: only their own entry, read from the session.
        let (_, mine) = get_("/api/raider-appeal", users[0]).await;
        assert_eq!(mine["listed"], true);
        assert_eq!(
            (mine["category"].as_str(), mine["can_appeal"].as_bool()),
            (Some("scam"), Some(true))
        );
        assert!(mine.get("evidence").is_none() && !mine.to_string().contains("Server"));
        assert_eq!(
            get_("/api/raider-appeal", users[2]).await.1,
            json!({"listed": false})
        );
        // S12: a user_id in the body is ignored; the appeal is always the session user's.
        let appeal =
            |u: i64, body: Value| send("POST", "/api/raider-appeal".into(), u, false, Some(body));
        let spoof = json!({"text": "hi", "user_id": users[0].to_string()});
        assert_eq!(appeal(users[3], spoof).await.0, 400);
        assert_eq!(appeal(users[0], json!({"text": " "})).await.0, 400);
        assert_eq!(
            appeal(users[0], json!({"text": "y".repeat(1001)})).await.0,
            400
        );
        assert_eq!(
            appeal(users[0], json!({"text": "I was hacked"})).await.0,
            200
        );
        assert_eq!(
            appeal(users[0], json!({"text": "again"})).await.0,
            400,
            "one open appeal"
        );
        let open_appeal = || async {
            let (_, a) = get_("/api/raiders/appeals", owner).await;
            a.as_array()
                .unwrap()
                .iter()
                .find(|a| a["user_id"] == uid0)
                .unwrap()["id"]
                .as_i64()
                .unwrap()
        };
        // Deny: the next appeal opens after 30 days.
        let id = open_appeal().await;
        let deny = format!("/api/raiders/appeals/{id}/deny");
        assert_eq!(post(deny.clone(), owner, json!({})).await.0, 200);
        assert_eq!(post(deny, owner, json!({})).await.0, 409);
        assert_eq!(
            get_("/api/raider-appeal", users[0]).await.1["can_appeal"],
            false
        );
        assert_eq!(appeal(users[0], json!({"text": "please"})).await.0, 400);
        exec(
            "update raiders set appeal_after = now() - interval '1 minute' where user_id = $1 and $2 = 0",
            users[0],
            0,
        )
        .await;
        assert_eq!(appeal(users[0], json!({"text": "please"})).await.0, 200);
        // Accept: off the list, and the bot lifts what the list did.
        let id = open_appeal().await;
        let accept = format!("/api/raiders/appeals/{id}/accept");
        assert_eq!(post(accept, owner, json!({})).await.0, 200);
        assert_eq!(status(users[0]).await.0, "removed");
        assert_eq!(
            jobs_for(users[0]).await.last().unwrap(),
            &(0, "raider_lift".to_string())
        );
        // Remove queues a lift too.
        let remove = format!("/api/raiders/{}/remove", users[1]);
        assert_eq!(post(remove, owner, json!({})).await.0, 200);
        assert_eq!(jobs_for(users[1]).await, [(0, "raider_lift".to_string())]);
        let reviews: i64 =
            sqlx::query_scalar("select count(*) from raider_reviews where user_id = any($1)")
                .bind(&users)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(reviews, 6, "confirm, verify, reject, deny, accept, remove");

        // Matches: this server's actions with only the category and the public note.
        exec(
            "insert into raider_actions (guild_id, user_id, action) values ($1, $2, 'ban')",
            g1,
            users[1],
        )
        .await;
        let (code, m) = get_(&format!("/api/guilds/{g1}/raider_matches"), server_owner).await;
        assert_eq!(code, 200, "{m}");
        assert_eq!(
            (m[0]["category"].as_str(), m[0]["action"].as_str()),
            (Some("nuke"), Some("ban"))
        );
        assert!(!m.to_string().contains("evidence") && !m.to_string().contains("reporter"));

        for sql in [
            "delete from raider_evidence where user_id = any($1)",
            "delete from raider_reviews where user_id = any($1)",
            "delete from raider_appeals where user_id = any($1)",
            "delete from raiders where user_id = any($1)",
            "delete from scheduled_jobs where payload->>'user' = any($1::bigint[]::text[])",
        ] {
            sqlx::query(sql).bind(&users).execute(&db).await.unwrap();
        }
        for g in [g1, g2, g3] {
            exec("delete from cases where guild_id = $1 and $2 = 0", g, 0).await;
            exec(
                "delete from raider_actions where guild_id = $1 and $2 = 0",
                g,
                0,
            )
            .await;
        }
    }
}
