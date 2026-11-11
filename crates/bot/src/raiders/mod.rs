//! Raider List: a global list of raider accounts. Evidence comes from every server, the bot owner
//! reviews it on the dashboard (strong evidence from 3 different owners lists a user by itself),
//! and servers that turn the list on act on listed users when they join.
use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Mutex};
use tommybot_automod::window::Window;
use tommybot_core::{
    jobs,
    limits::{RAIDER_AUTO_OWNERS, RAIDER_MIN_DAYS, RAIDER_MIN_MEMBERS},
    raiders,
    sections::{RaiderAction, RaiderCategory},
};
use twilight_model::id::{
    marker::{GuildMarker, MessageMarker, UserMarker},
    Id,
};

use crate::{
    app::{now_secs, App},
    http_err,
};

/// Start of the reason of every action the list takes; a lift undoes only bans that still carry it.
pub const PREFIX: &str = "Raider List:";
const BUTTON: &str = "raider:";
/// Members per page of the scan.
const PAGE: u16 = if cfg!(test) { 2 } else { 1000 };
/// Kinds of evidence that count towards listing a user by itself.
const STRONG: [&str; 3] = ["auto_nuke", "auto_raid", "auto_spam"];
/// Scam deletions from one user in one server within this window make evidence.
const SCAM_HITS: usize = 3;
const SCAM_WINDOW_MS: i64 = 600_000;
/// While the bot owner has not verified an automatic listing, servers only get an alert for it,
/// whatever their action. Set to false to enforce unverified listings with the server's action.
const UNVERIFIED_ALERT_ONLY: bool = true;

/// How a server's detection reached the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Nuke,
    Raid,
    Scam,
    Spam,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Nuke => "auto_nuke",
            Kind::Raid => "auto_raid",
            Kind::Scam => "auto_scam",
            Kind::Spam => "auto_spam",
        }
    }

    fn category(self) -> RaiderCategory {
        match self {
            Kind::Nuke => RaiderCategory::Nuke,
            Kind::Raid => RaiderCategory::Raid,
            Kind::Scam => RaiderCategory::Scam,
            Kind::Spam => RaiderCategory::Spam,
        }
    }
}

fn cat_name(c: RaiderCategory) -> &'static str {
    match c {
        RaiderCategory::Nuke => "nuke",
        RaiderCategory::Raid => "raid",
        RaiderCategory::Scam => "scam",
        RaiderCategory::Spam => "spam",
    }
}

fn parse_cat(s: &str) -> Option<RaiderCategory> {
    RaiderCategory::ALL
        .into_iter()
        .find(|c| cat_name(*c).eq_ignore_ascii_case(s))
}

fn action_name(a: RaiderAction) -> &'static str {
    match a {
        RaiderAction::Alert => "alert",
        RaiderAction::Quarantine => "quarantine",
        RaiderAction::Kick => "kick",
        RaiderAction::Ban => "ban",
    }
}

/// What evidence keeps: counts, ids and fixed names only. It has no `String` on purpose, so no
/// member's message text can end up in the global table. `members` and `bot_joined` describe the
/// source server, so only established servers count towards an automatic listing.
#[derive(Serialize, Default, Clone, Copy, Debug)]
pub struct Detail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub case: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<u64>,
    /// Unix time the bot joined the source server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_joined: Option<i64>,
}

/// The server evidence comes from, looked up live once per batch.
struct Source {
    gid: Id<GuildMarker>,
    owner: Id<UserMarker>,
    members: u64,
    bot_joined: Option<i64>,
}

/// None when the bot cannot see the server.
async fn source(app: &App, gid: Id<GuildMarker>) -> Result<Option<Source>> {
    let guild = match app.http.guild(gid).with_counts(true).await {
        Ok(r) => r.model().await?,
        Err(e) if http_err::no_access(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let me = http_err::member(&app.http, gid, app.me).await?;
    Ok(Some(Source {
        gid,
        owner: guild.owner_id,
        // shortcut: this count includes bots and Discord has no cheap human-only one; unverified
        // listings only alert until the bot owner reviews them, which covers a server padded with bots.
        members: guild.approximate_member_count.unwrap_or(0),
        bot_joined: me.and_then(|m| m.joined_at).map(|t| t.as_secs()),
    }))
}

/// Collects evidence about `users`; a failure is logged and never stops the caller. `fresh`: the
/// users just joined, so they hold no roles and their member lookup is skipped (a quarantine's
/// saved roles still count).
pub async fn add_evidence(
    app: &App,
    gid: Id<GuildMarker>,
    users: &[u64],
    (kind, r#ref): (Kind, &str),
    detail: Detail,
    fresh: bool,
) {
    let src = match source(app, gid).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => return tracing::warn!(?e, %gid, "raider list: server lookup failed"),
    };
    for &u in users {
        let user = Id::new(u);
        let roles = if fresh { Some(vec![]) } else { None };
        if let Err(e) = collect(app, &src, user, (kind, r#ref), detail, roles).await {
            tracing::warn!(?e, %gid, %user, "raider list: collecting evidence failed");
        }
    }
}

async fn collect(
    app: &App,
    src: &Source,
    user: Id<UserMarker>,
    (kind, r#ref): (Kind, &str),
    mut detail: Detail,
    roles: Option<Vec<i64>>,
) -> Result<bool> {
    if user == app.me || Some(user) == app.bot_owner {
        return Ok(false);
    }
    let roles = match roles {
        Some(r) => r,
        None => member_roles(app, src.gid, user).await?,
    };
    let (g, o, u) = (
        src.gid.get() as i64,
        src.owner.get() as i64,
        user.get() as i64,
    );
    if raiders::not_collectable(&app.pool, g, o, u, &roles).await? {
        return Ok(false);
    }
    detail.members = Some(src.members);
    detail.bot_joined = src.bot_joined;
    let e = raiders::Evidence {
        user: u,
        guild: g,
        owner: o,
        kind: kind.name(),
        category: cat_name(kind.category()),
        r#ref,
        detail: json!(detail),
        reporter: None,
        note: None,
    };
    record(app, &e).await
}

/// The member's roles; none when they are not in the server. Other errors return Err.
async fn member_roles(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<Vec<i64>> {
    Ok(http_err::member(&app.http, gid, user)
        .await?
        .iter()
        .flat_map(|m| m.roles.iter().map(|r| r.get() as i64))
        .collect())
}

/// Writes the evidence and its entry, then applies the auto-listing rule. True when the evidence
/// is new.
async fn record(app: &App, e: &raiders::Evidence<'_>) -> Result<bool> {
    let mut tx = app.pool.begin().await?;
    let fresh = raiders::add(&mut tx, e).await?;
    tx.commit().await?;
    if fresh && STRONG.contains(&e.kind) {
        let user = Id::new(e.user as u64);
        if let Err(err) = auto_list(app, user).await {
            // A Discord or DB failure must not lose the listing: a job tries again until it works.
            tracing::warn!(?err, %user, "raider list: auto-listing failed, retrying in a job");
            fan(&app.pool, "raider_autolist", user).await?;
        }
    }
    Ok(fresh)
}

/// Strong evidence that counts: only from servers with enough members that had the bot for a
/// while, so three throwaway servers are not three independent owners, and only evidence newer
/// than the bot owner's last review, so a removed or rejected entry is not listed again by itself
/// from the evidence the bot owner already judged.
const QUALIFIED: &str = "user_id = $1 AND kind = ANY($2)
    AND COALESCE((detail->>'members')::bigint, 0) >= $3
    AND COALESCE((detail->>'bot_joined')::bigint, 9223372036854775807)
        <= extract(epoch FROM now())::bigint - $4 * 86400
    AND created_at > COALESCE((SELECT reviewed_at FROM raiders WHERE user_id = $1), '-infinity')";

/// A pending entry backed by qualified strong evidence from enough different owners is confirmed
/// by itself, unverified, with the category most of those owners saw, and every enabled server
/// checks for it. A bot account always waits for the bot owner. The user lookup happens outside
/// any transaction; an error makes the caller retry in a `raider_autolist` job.
async fn auto_list(app: &App, user: Id<UserMarker>) -> Result<()> {
    let u = user.get() as i64;
    let owners: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(DISTINCT source_owner) FROM raider_evidence WHERE {QUALIFIED}"
    )))
    .bind(u)
    .bind(STRONG.as_slice())
    .bind(RAIDER_MIN_MEMBERS as i64)
    .bind(RAIDER_MIN_DAYS)
    .fetch_one(&app.pool)
    .await?;
    if owners < RAIDER_AUTO_OWNERS {
        return Ok(());
    }
    // A deleted account (404) is never listed; any other error retries.
    match app.http.user(user).await {
        Err(e) if http_err::gone(&e) => return Ok(()),
        r => {
            if r?.model().await?.bot {
                return Ok(());
            }
        }
    }
    let mut tx = app.pool.begin().await?;
    raiders::lock_user(&mut tx, u).await?;
    // The count is checked again under the lock: a review may have come in since.
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH s AS (SELECT kind, count(DISTINCT source_owner) AS n FROM raider_evidence
                    WHERE {QUALIFIED} GROUP BY kind)
         UPDATE raiders SET status = 'confirmed', verified = false, expires_at = NULL, updated_at = now(),
             category = (SELECT CASE kind WHEN 'auto_nuke' THEN 'nuke' WHEN 'auto_raid' THEN 'raid' ELSE 'spam' END
                         FROM s ORDER BY n DESC, kind LIMIT 1)
         WHERE user_id = $1 AND status = 'pending'
           AND (SELECT count(DISTINCT source_owner) FROM raider_evidence WHERE {QUALIFIED}) >= $5"
    )))
    .bind(u)
    .bind(STRONG.as_slice())
    .bind(RAIDER_MIN_MEMBERS as i64)
    .bind(RAIDER_MIN_DAYS)
    .bind(RAIDER_AUTO_OWNERS)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !done {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO raider_reviews (actor, user_id, from_status, to_status, note)
         VALUES ($1, $2, 'pending', 'confirmed', $3)",
    )
    .bind(app.me.get() as i64)
    .bind(u)
    .bind(format!(
        "listed by itself: strong evidence from {owners} server owners"
    ))
    .execute(&mut *tx)
    .await?;
    fan(&mut *tx, "raider_check", user).await?;
    tx.commit().await?;
    listed_dm(app, user).await;
    Ok(())
}

/// Queues `kind` for the user as a fan-out job (server 0), which queues one per server.
async fn fan(db: impl sqlx::PgExecutor<'_>, kind: &str, user: Id<UserMarker>) -> Result<()> {
    let job = json!({"user": user.get().to_string()});
    jobs::enqueue(db, 0, kind, job, now_secs()).await?;
    Ok(())
}

/// Anti-nuke stopped a nuker (`ref` = the nuke case).
pub async fn nuked(
    app: &App,
    gid: Id<GuildMarker>,
    actor: Id<UserMarker>,
    (case, nuke, ops): (i32, &str, usize),
    stopped: bool,
) {
    if stopped {
        let d = Detail {
            count: Some(ops as u32),
            case: Some(case),
            filter: Some("anti-nuke"),
            ..Default::default()
        };
        add_evidence(app, gid, &[actor.get()], (Kind::Nuke, nuke), d, false).await;
    }
}

/// Join Raid flagged these accounts (`ref` = the raid code). They make an entry only once a second
/// owner flagged them, and count towards a listing only from 3 different owners.
pub async fn raid_flagged(app: &App, gid: Id<GuildMarker>, users: &[u64], code: &str) {
    let d = Detail {
        count: Some(users.len() as u32),
        filter: Some("join raid"),
        ..Default::default()
    };
    add_evidence(app, gid, users, (Kind::Raid, code), d, true).await;
}

/// The members of a coordinated spam crowd (`ref` = the message that tripped it).
pub async fn spam_crowd(app: &App, gid: Id<GuildMarker>, users: &[u64], msg: Id<MessageMarker>) {
    let d = Detail {
        count: Some(users.len() as u32),
        filter: Some("coordinated spam"),
        ..Default::default()
    };
    add_evidence(app, gid, users, (Kind::Spam, &msg.to_string()), d, false).await;
}

/// Automod deleted a scam link or QR login; the third within 10 minutes makes evidence.
pub async fn scam_hit(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    msg: Id<MessageMarker>,
) {
    static HITS: Mutex<BTreeMap<(u64, u64), Window>> = Mutex::new(BTreeMap::new());
    let n = {
        let mut hits = HITS.lock().unwrap();
        // shortcut: past 10k tracked users everything is forgotten; a scammer then needs 3 hits again.
        if hits.len() > 10_000 {
            hits.clear();
        }
        let w = hits.entry((gid.get(), user.get())).or_default();
        w.push(crate::automod::now_ms(), SCAM_WINDOW_MS)
    };
    if n == SCAM_HITS {
        let d = Detail {
            count: Some(n as u32),
            filter: Some("scam link"),
            ..Default::default()
        };
        let r = msg.to_string();
        add_evidence(app, gid, &[user.get()], (Kind::Scam, &r), d, false).await;
    }
}

mod join;
mod review;
mod tasks;

pub use join::on_join;
pub(crate) use join::{action_log, alert_log, alert_rows};
use review::listed_dm;
pub(crate) use review::listed_text;
pub use review::{command, handle, sweep};
pub(crate) use tasks::lift_log;
pub use tasks::{check_job, fan_out, lift_gave_up, lift_job, lifted, on_saved, on_unban, scan_job};

#[cfg(test)]
mod tests;
