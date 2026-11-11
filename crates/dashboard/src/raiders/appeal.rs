//! Appeals: the bot owner's decision, and the listed user's own page. On that page the user is
//! always the session's, never one from the request.
use super::{bad, bot_owner, job, notify, now, review, At, APPEAL_MAX, APPEAL_WAIT_DAYS, CHANGED};
use crate::{api::Res, auth::Sess, ApiError, AppState};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use tommybot_core::jobs;

/// Accept (takes the user off the list and lifts what it did) or Deny (the next appeal opens in 30 days).
pub(super) async fn decide(
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
    let result = if to_appeal == "accepted" {
        "Your Raider List appeal was accepted: you are off the list, and what it did in servers is being lifted.".to_string()
    } else {
        format!(
            "Your Raider List appeal was denied. You can appeal again in {APPEAL_WAIT_DAYS} days."
        )
    };
    notify(&mut tx, user, result).await?;
    let log = format!(
        "Raider List log: the appeal of <@{user}> ({user}) was {to_appeal}, by <@{}>.",
        s.user_id
    );
    notify(&mut tx, s.user_id, log).await?;
    tx.commit().await?;
    Ok(Json(json!({"ok": true})))
}

pub(super) async fn my_entry(State(st): State<AppState>, s: Sess) -> Res {
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
pub(super) struct Appeal {
    text: String,
}

pub(super) async fn my_appeal(State(st): State<AppState>, s: Sess, Json(b): Json<Appeal>) -> Res {
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
    let Some((_, waited)) = row.filter(|r| r.0 == "confirmed") else {
        return Err(bad("you are not on the Raider List"));
    };
    if !waited {
        return Err(bad(
            "your last appeal was denied; you can appeal again 30 days after that",
        ));
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
