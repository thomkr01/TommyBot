//! Report to Raider List, from a case.
use super::{bad, category};
use crate::{
    access::Level,
    api::{log_change, require, Res},
    auth::Sess,
    store, ApiError, AppState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use tommybot_core::raiders;

#[derive(Deserialize)]
pub(super) struct Report {
    category: String,
    /// The reporter's own words, shown to the bot owner only.
    #[serde(default)]
    reason: Option<String>,
}

/// Adds a report to the user's entry through the same checks as `t!raider report`
/// (tommybot_core::raiders), so both refuse the same reports. A report never confirms.
pub(super) async fn report(
    State(st): State<AppState>,
    s: Sess,
    Path((guild, no)): Path<(i64, i32)>,
    Json(b): Json<Report>,
) -> Res {
    require(&st, &s, guild, Level::Owner, true).await?;
    category(&b.category)?;
    let case: Option<(String, i64)> =
        sqlx::query_as("select kind, user_id from cases where guild_id = $1 and case_no = $2")
            .bind(guild)
            .bind(no)
            .fetch_optional(&st.db)
            .await?;
    let Some((kind, user)) = case else {
        return Err(ApiError::NotFound);
    };
    let id = |t: String| {
        t.parse::<i64>()
            .map_err(|_| bad("an id from Discord is unknown"))
    };
    let owner = id(st.discord.guild_owner(guild).await?)?;
    let members = st.discord.guild_size(guild).await?.map_or(0, |g| g.1);
    let days = st
        .discord
        .bot_joined(guild)
        .await?
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
        .map_or(0, |t| (chrono::Utc::now() - t.to_utc()).num_days());
    let roles: Vec<i64> = st
        .discord
        .member(guild, &user.to_string())
        .await?
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.parse().ok())
        .collect();
    let r = raiders::Report {
        guild,
        owner,
        user,
        reporter: s.user_id,
        case: Some((no, kind.as_str())),
        category: &b.category,
        reason: b.reason.as_deref(),
        members,
        days_with_bot: days,
        roles: &roles,
        protected: [
            id(st.discord.bot_id().await?)?,
            id(st.discord.bot_owner().await?)?,
        ],
    };
    raiders::report(&st.db, &r).await?.map_err(bad)?;
    let (_, config) = store::current(&st.db, guild).await?;
    let text = format!(
        "{} (<@{}>) reported <@{user}> (case #{no}) to the Raider List as {}",
        s.username, s.user_id, b.category
    );
    log_change(&st, guild, &config, text).await;
    Ok(Json(json!({"ok": true})))
}
