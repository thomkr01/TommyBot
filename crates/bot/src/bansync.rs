//! Ban Sync: a ban in one server is copied to the owner's other servers that have Ban Sync on.
//! It listens to the audit log, so every ban path (commands, dashboard, Discord's own button) is
//! seen, and every step is a job, so the 60 s hold, the hourly cap and restarts are covered.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Mutex};
use tommybot_core::{
    cases, jobs,
    limits::{BAN_SYNC_HOLD_S, BAN_SYNC_HOURLY},
    permits::Level,
    sections::{BanSyncCfg, BanSyncReceive},
};
use tommybot_logging::{
    embed::{self, ORANGE},
    emoji::WARNING,
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    channel::message::component::{ButtonStyle, Component},
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    commands::{self, ModKind},
    http_err,
    prefix::Flags,
    reply, verify,
};

#[path = "bansync_actions.rs"]
mod actions;
pub(crate) use actions::answer_click;
use actions::paused;
pub use actions::{handle, import_job, panel_text, send_rest_job};

/// Start of the reason every copy carries; an unban lifts only bans that still start with it.
pub const PREFIX: &str = "Ban Sync from";
const BUTTON: &str = "bansync:";
const REST: &str = "rest:";
/// A bot ban is a staff command when a staff member's case for it is at most this old.
const CASE_WINDOW_S: i64 = 300;
/// Bans per page of `GET /guilds/{id}/bans` during an import.
const PAGE: u16 = if cfg!(test) { 3 } else { 1000 };

/// (receiving server, user, source server): the key of a `ban_sync_links` row.
type Key = (Id<GuildMarker>, Id<UserMarker>, Id<GuildMarker>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    StaffCmd,
    Discord,
    JoinRaid,
    Automod,
    JoinGate,
    Verify,
}

impl Origin {
    fn name(self) -> &'static str {
        match self {
            Origin::StaffCmd => "staff_cmd",
            Origin::Discord => "discord",
            Origin::JoinRaid => "joinraid",
            Origin::Automod => "automod",
            Origin::JoinGate => "joingate",
            Origin::Verify => "verify",
        }
    }
}

/// Staff and Discord bans always go out; each kind of automatic ban has its own switch.
fn sends(c: &BanSyncCfg, o: Origin) -> bool {
    match o {
        Origin::StaffCmd | Origin::Discord => true,
        Origin::JoinRaid => c.share_joinraid,
        Origin::Automod => c.share_automod,
        Origin::JoinGate => c.share_joingate,
        Origin::Verify => c.share_verify,
    }
}

/// The `joingate_actions` reason of a Join Gate ban, as `joingate::punish` writes it.
fn gate_origin(reason: &str) -> Origin {
    match reason {
        "join raid" => Origin::JoinRaid,
        "discord automod rule" => Origin::Automod,
        _ => Origin::JoinGate,
    }
}

/// Whose containment by anti-nuke stops a sync. For a ban the bot made for staff, the audit actor
/// is the bot, so the staff member comes from the case; a ban the bot made on its own is held
/// while anyone in the server is contained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behind {
    Staff(Id<UserMarker>),
    Bot,
    Unknown,
}

fn behind(me: Id<UserMarker>, actor: Option<u64>, moderator: Option<i64>) -> Behind {
    match (actor, moderator) {
        (_, Some(m)) if m as u64 != me.get() => Behind::Staff(Id::new(m as u64)),
        (Some(a), _) if a == me.get() => Behind::Bot,
        (Some(a), _) => Behind::Staff(Id::new(a)),
        (None, _) => Behind::Unknown,
    }
}

/// The staff member stored with a row: the human behind the ban, if there was one.
fn staff(b: Behind) -> Option<i64> {
    match b {
        Behind::Staff(u) => Some(u.get() as i64),
        _ => None,
    }
}

/// Anti-nuke contained whoever is behind the ban since `at`. A merged audit entry names no actor,
/// so that check is skipped then.
async fn stopped(app: &App, gid: Id<GuildMarker>, b: Behind, at: Option<i64>) -> Result<bool> {
    let Some(at) = at else {
        return Ok(false);
    };
    match b {
        Behind::Staff(u) => crate::antinuke::contained_since(app, gid, Some(u), at).await,
        Behind::Bot => crate::antinuke::contained_since(app, gid, None, at).await,
        Behind::Unknown => {
            tracing::info!(%gid, "ban sync: actor unknown, containment not checked");
            Ok(false)
        }
    }
}

/// Why a user is never banned by a copy in this server, if they are not.
pub(crate) fn exempt(
    level: &Level,
    user: Id<UserMarker>,
    me: Id<UserMarker>,
    bot_owner: Option<Id<UserMarker>>,
) -> Option<&'static str> {
    if user == me {
        return Some("the bot itself");
    }
    if Some(user) == bot_owner {
        return Some("the bot owner");
    }
    match level {
        Level::Owner => Some("server owner"),
        Level::ExtraOwner => Some("extra owner"),
        Level::TrustedAdmin => Some("trusted admin"),
        l if l.whitelisted("antinuke") => Some("on the anti-nuke whitelist"),
        _ => None,
    }
}

/// User text for an embed line: one line, markdown and mentions shown as typed.
pub(crate) fn md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\r' => out.push(' '),
            '\\' | '[' | ']' | '*' | '_' | '~' | '|' | '>' | '<' | '`' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// The unix time of an audit entry, from its snowflake id, a little early so nothing at the edge is missed.
fn audit_secs(id: u64) -> i64 {
    (((id >> 22) + 1_420_070_400_000) / 1000) as i64 - 5
}

/// The rest of a temp ban, or why there is nothing left to copy.
fn remaining(expires: Option<i64>) -> Result<Option<i64>, &'static str> {
    match expires {
        Some(t) if t <= now_secs() => Err("temp ban already ended"),
        Some(t) => Ok(Some(t - now_secs())),
        None => Ok(None),
    }
}

pub(crate) fn id_of(p: &Value, k: &str) -> Option<u64> {
    p[k].as_u64().or_else(|| p[k].as_str()?.parse().ok())
}

pub(crate) fn need(p: &Value, k: &str) -> Result<u64> {
    id_of(p, k).with_context(|| format!("job payload has no {k}"))
}

/// A pending or done copy: such a ban was made by Ban Sync and is never shared on.
async fn is_copy(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM ban_sync_links WHERE guild_id = $1 AND user_id = $2
                        AND state IN ('pending', 'banned'))",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .fetch_one(&app.pool)
    .await?)
}

/// A ban reason that a copy from `src` wrote.
fn ours(reason: Option<&str>, src: Id<GuildMarker>) -> bool {
    reason.is_some_and(|r| r.starts_with(PREFIX) && r.contains(&format!("({src}): ")))
}

/// The current ban there is a copy from `src`.
async fn banned_by_us(app: &App, key: Key) -> Result<bool> {
    let (to, user, _) = key;
    let ban = http_err::ban(&app.http, to, user).await?;
    let Some(row) = load(app, key).await? else {
        return Ok(false);
    };
    owned(app, key, &row, ban.as_ref().map(|b| b.reason.as_deref())).await
}

/// `ban` (None: no ban; Some(reason)) is the copy from `src`: our reason, or a copy laid over a
/// local temp ban, which keeps that ban's reason (Discord ignores a second ban). The row remembers
/// that reason, and the copy's ban dropped the temp ban's unban job, so that job still pending
/// means the copy's ban never happened.
async fn owned(
    app: &App,
    (to, user, src): Key,
    row: &Row,
    ban: Option<Option<&str>>,
) -> Result<bool> {
    let Some(reason) = ban else {
        return Ok(false);
    };
    // A second ban keeps the first ban's reason, so a staff ban over a done copy (a TommyBot
    // ban or tempban case by someone else since) leaves the reason as it was: it is theirs now.
    if row.state == "banned" && hand_ban_since(app, (to, user, src)).await? {
        return Ok(false);
    }
    if ours(reason, src) {
        return Ok(true);
    }
    if row.over_reason.as_deref() != Some(reason.unwrap_or("")) {
        return Ok(false);
    }
    Ok(temp_end(app, to, user).await? != row.temp_end)
}

/// A ban or tempban case for the user in the receiving server, by someone other than the bot,
/// newer than the copy (its `done_at`). The copy's own case is the bot's, or a Ban here click's,
/// which is written before the row becomes banned.
async fn hand_ban_since(app: &App, (to, user, src): Key) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM ban_sync_links l JOIN cases c
             ON c.guild_id = l.guild_id AND c.user_id = l.user_id
         WHERE l.guild_id = $1 AND l.user_id = $2 AND l.source_guild = $3
           AND c.kind IN ('ban', 'tempban') AND c.mod_id <> $4 AND c.created_at > l.done_at)",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(app.me.get() as i64)
    .fetch_one(&app.pool)
    .await?)
}

/// Deletes the unban job of the local temp ban a copy was laid over, with `?`: the ban's own
/// `forget_jobs` is best effort, and a job left behind would make `owned` disown the copy and
/// then lift it at the old end. A failure leaves the row pending for the retry.
async fn drop_temp_job(app: &App, (to, user, src): Key) -> Result<()> {
    sqlx::query(
        "DELETE FROM scheduled_jobs j USING ban_sync_links l
         WHERE l.guild_id = $1 AND l.user_id = $2 AND l.source_guild = $3
           AND j.guild_id = $1 AND j.kind = 'unban' AND j.payload->>'user' = $2::text
           AND extract(epoch FROM j.run_at)::bigint = extract(epoch FROM l.temp_end)::bigint",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// Remembers the local temp ban a copy is about to be laid over (`owned`).
async fn over_temp(app: &App, (to, user, src): Key, reason: &str, end: Option<i64>) -> Result<()> {
    sqlx::query(
        "UPDATE ban_sync_links SET over_reason = $4, temp_end = to_timestamp($5::float8)
         WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3 AND state = 'pending'",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(reason)
    .bind(end.map(|e| e as f64))
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// Audit `MemberBanAdd`, any actor: queues the share after the hold.
pub async fn on_ban(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (A::MemberBanAdd, Some(gid), Some(user)) = (e.action_type, e.guild_id, e.target_id) else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    let c = &cfg.ban_sync;
    if !(c.enabled && c.share) || is_copy(app, gid, user.cast()).await? {
        return Ok(());
    }
    queue(app, gid, "ban_sync_share", e).await
}

/// Audit `MemberBanRemove`, any actor: a lifted copy is marked so, and the source queues the lift.
pub async fn on_unban(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (A::MemberBanRemove, Some(gid), Some(user)) = (e.action_type, e.guild_id, e.target_id)
    else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE ban_sync_links SET state = 'lifted', done_at = now()
         WHERE guild_id = $1 AND user_id = $2 AND state = 'banned'",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .execute(&app.pool)
    .await?;
    let cfg = app.cfg(gid.get()).await?;
    let c = &cfg.ban_sync;
    if !(c.enabled && c.sync_unbans) {
        return Ok(());
    }
    queue(app, gid, "ban_sync_lift", e).await
}

async fn queue(app: &App, gid: Id<GuildMarker>, kind: &str, e: &AuditLogEntry) -> Result<()> {
    let job = json!({
        "user": e.target_id.map(|t| t.get()),
        "actor": e.user_id.map(|a| a.get()),
        "reason": e.reason,
        "at": audit_secs(e.id.get()),
    });
    let at = now_secs() + BAN_SYNC_HOLD_S;
    jobs::enqueue(&app.pool, gid.get() as i64, kind, job, at).await?;
    Ok(())
}

/// The newest case of `kinds` for the user, since `since` when given, as (case, moderator).
async fn last_case(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    kinds: &[&str],
    since: Option<i64>,
) -> Result<Option<(i32, i64)>> {
    Ok(sqlx::query_as(
        "SELECT case_no, mod_id FROM cases WHERE guild_id = $1 AND user_id = $2 AND kind = ANY($3)
           AND ($4::float8 IS NULL OR created_at >= to_timestamp($4::float8))
         ORDER BY case_no DESC LIMIT 1",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(kinds)
    .bind(since.map(|s| s as f64))
    .fetch_optional(&app.pool)
    .await?)
}

/// The case behind an action the bot made, if the audit actor is the bot.
async fn bot_case(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    actor: Option<u64>,
    kinds: &[&str],
    at: i64,
) -> Result<Option<(i32, i64)>> {
    if actor != Some(app.me.get()) {
        return Ok(None);
    }
    last_case(app, gid, user, kinds, Some(at - CASE_WINDOW_S)).await
}

/// What made the ban. `actor` is None for an import, where the audit entry is gone and only the
/// bot's own records and the ban reason tell.
async fn origin(
    app: &App,
    (src, user): (Id<GuildMarker>, Id<UserMarker>),
    actor: Option<u64>,
    case: Option<(i32, i64)>,
    since: Option<i64>,
    reason: &str,
) -> Result<Origin> {
    if actor.is_some_and(|a| a != app.me.get()) {
        return Ok(Origin::Discord);
    }
    // The newer record of the two made the ban, when the user has both.
    let gate: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM joingate_actions WHERE guild_id = $1 AND user_id = $2 AND action = 'Ban'
           AND ($3::float8 IS NULL OR created_at >= to_timestamp($3::float8))
           AND ($4::int IS NULL OR created_at > (SELECT created_at FROM cases WHERE guild_id = $1 AND case_no = $4))
         ORDER BY id DESC LIMIT 1",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .bind(since.map(|s| s as f64))
    .bind(case.map(|c| c.0))
    .fetch_optional(&app.pool)
    .await?;
    if let Some(r) = gate {
        return Ok(gate_origin(&r));
    }
    if let Some((_, by)) = case {
        // Automod filters and warn thresholds write cases with the bot as moderator.
        return Ok(if by as u64 == app.me.get() {
            Origin::Automod
        } else {
            Origin::StaffCmd
        });
    }
    // Verification bans leave no case and no Join Gate record. They carry AUDIT_REASON, or no
    // reason when the server switched that off.
    let verify = actor.is_some()
        || reason == verify::AUDIT_REASON
        || (reason.is_empty() && !app.cfg(src.get()).await?.verification.verify.audit_reason);
    Ok(if verify {
        Origin::Verify
    } else {
        Origin::Discord
    })
}

/// A copy ending at `copy` (None: never) outlasts a local ban ending at `local` (None: never).
fn outlasts(copy: Option<i64>, local: Option<i64>) -> bool {
    local.is_some_and(|l| copy.is_none_or(|c| c > l))
}

/// The end of a TommyBot temp ban: its pending unban job, also while a failed try holds its lease
/// (`owned` must not read a leased job as gone).
async fn temp_end(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT extract(epoch FROM run_at)::bigint FROM scheduled_jobs
         WHERE guild_id = $1 AND kind = 'unban' AND payload->>'user' = $2 ORDER BY run_at LIMIT 1",
    )
    .bind(gid.get() as i64)
    .bind(user.get().to_string())
    .fetch_optional(&app.pool)
    .await?)
}

/// A temp ban's unban job is due or held by a running try.
async fn temp_ending(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM scheduled_jobs
         WHERE guild_id = $1 AND kind = 'unban' AND payload->>'user' = $2
           AND (run_at <= now() OR locked_until > now()))",
    )
    .bind(gid.get() as i64)
    .bind(user.get().to_string())
    .fetch_one(&app.pool)
    .await?)
}

/// The owner from Discord, not the 5-minute cache; None when the bot cannot see the server.
pub(crate) async fn live_owner(app: &App, gid: Id<GuildMarker>) -> Result<Option<Id<UserMarker>>> {
    match app.http.guild(gid).await {
        Ok(r) => Ok(Some(r.model().await?.owner_id)),
        Err(e) if http_err::no_access(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Live owners looked up in one job, so each server is asked once.
#[derive(Default)]
struct Owners(Mutex<BTreeMap<u64, Option<Id<UserMarker>>>>);

impl Owners {
    async fn of(&self, app: &App, gid: Id<GuildMarker>) -> Result<Option<Id<UserMarker>>> {
        let known = self.0.lock().unwrap().get(&gid.get()).copied();
        if let Some(o) = known {
            return Ok(o);
        }
        let o = live_owner(app, gid).await?;
        self.0.lock().unwrap().insert(gid.get(), o);
        Ok(o)
    }

    /// The owner of a server the bot is in.
    async fn need(&self, app: &App, gid: Id<GuildMarker>) -> Result<Id<UserMarker>> {
        self.of(app, gid)
            .await?
            .with_context(|| format!("server {gid} is not visible"))
    }
}

/// Same Discord owner in `guild_owners`, then confirmed live for both servers.
async fn same_owner(
    app: &App,
    owners: &Owners,
    a: Id<GuildMarker>,
    b: Id<GuildMarker>,
) -> Result<bool> {
    let stored: Vec<i64> =
        sqlx::query_scalar("SELECT owner_id FROM guild_owners WHERE guild_id = ANY($1)")
            .bind([a.get() as i64, b.get() as i64].as_slice())
            .fetch_all(&app.pool)
            .await?;
    // A server the bot left has no row any more: no Discord call for it.
    if stored.len() != 2 || stored[0] != stored[1] {
        return Ok(false);
    }
    let owner = Some(Id::new(stored[0] as u64));
    Ok(owners.of(app, a).await? == owner && owners.of(app, b).await? == owner)
}

/// `src` sends and `to` receives, and they have the same owner.
async fn grouped(
    app: &App,
    owners: &Owners,
    src: Id<GuildMarker>,
    to: Id<GuildMarker>,
) -> Result<bool> {
    let (a, b) = (app.cfg(src.get()).await?, app.cfg(to.get()).await?);
    let on = a.ban_sync.enabled
        && a.ban_sync.share
        && b.ban_sync.enabled
        && b.ban_sync.receive != BanSyncReceive::Off;
    Ok(on && same_owner(app, owners, src, to).await?)
}

/// The other servers of `gid`'s owner, as `guild_owners` lists them.
async fn group(app: &App, gid: Id<GuildMarker>) -> Result<Vec<Id<GuildMarker>>> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT o.guild_id FROM guild_owners o JOIN guild_owners s ON s.owner_id = o.owner_id
         WHERE s.guild_id = $1 AND o.guild_id <> $1 ORDER BY o.guild_id",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    Ok(ids.into_iter().map(|g| Id::new(g as u64)).collect())
}

struct Copy {
    origin: Origin,
    case: Option<i32>,
    reason: String,
    expires: Option<i64>,
    actor: Option<u64>,
    moderator: Option<i64>,
    banned_at: Option<i64>,
}

/// Writes the row (pending, or skipped when the source shared [`BAN_SYNC_HOURLY`] other users in
/// the last hour), then queues the copy. An active pending or banned row is left alone; a finished
/// one starts over, so a re-ban syncs again. Returns (copy queued, over the cap).
async fn offer(app: &App, (to, user, src): Key, c: &Copy) -> Result<(bool, bool)> {
    // One transaction, so a failure never leaves a pending row without its copy job. The cap is
    // counted under a per-source lock, so a share job and an import running at once cannot both
    // pass it.
    let mut tx = app.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('ban_sync_cap:' || $1, 0))")
        .bind(src.get() as i64)
        .execute(&mut *tx)
        .await?;
    let others: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT user_id) FROM ban_sync_links WHERE source_guild = $1 AND user_id <> $2
           AND created_at > now() - interval '1 hour' AND skip_reason IS DISTINCT FROM 'rate cap'",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .fetch_one(&mut *tx)
    .await?;
    let capped = others >= BAN_SYNC_HOURLY;
    let (state, skip) = if capped {
        ("skipped", Some("rate cap"))
    } else {
        ("pending", None)
    };
    let written = sqlx::query(
        "INSERT INTO ban_sync_links (guild_id, user_id, source_guild, source_case, origin, reason,
             expires_at, state, skip_reason, actor_id, mod_id, banned_at)
         VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7::float8), $8, $9, $10, $11, to_timestamp($12::float8))
         ON CONFLICT (guild_id, user_id, source_guild) DO UPDATE SET
             source_case = excluded.source_case, origin = excluded.origin, reason = excluded.reason,
             expires_at = excluded.expires_at, state = excluded.state, skip_reason = excluded.skip_reason,
             actor_id = excluded.actor_id, mod_id = excluded.mod_id, banned_at = excluded.banned_at,
             case_no = NULL, created_at = now(), done_at = NULL, over_reason = NULL, temp_end = NULL
         WHERE ban_sync_links.state IN ('lifted', 'cancelled', 'skipped', 'alerted')",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(c.case)
    .bind(c.origin.name())
    .bind(&c.reason)
    .bind(c.expires.map(|t| t as f64))
    .bind(state)
    .bind(skip)
    .bind(c.actor.map(|a| a as i64))
    .bind(c.moderator)
    .bind(c.banned_at.map(|t| t as f64))
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if written && !capped {
        enqueue_copy(&mut *tx, (to, user, src), now_secs()).await?;
    }
    tx.commit().await?;
    Ok((written && !capped, capped))
}

async fn enqueue_copy(db: impl sqlx::PgExecutor<'_>, (to, user, src): Key, at: i64) -> Result<()> {
    let job = json!({"user": user.get(), "source": src.get()});
    jobs::enqueue(db, to.get() as i64, "ban_sync_copy", job, at).await?;
    Ok(())
}

/// The scheduler dropped a copy job after its last try: the row is skipped with the error, so it
/// no longer counts as a copy and a new ban there shares again.
pub async fn copy_gave_up(app: &App, job: &jobs::Job, e: &anyhow::Error) {
    let key = (|| {
        let p = &job.payload;
        Some((
            Id::new_checked(job.guild_id as u64)?,
            Id::new_checked(id_of(p, "user")?)?,
            Id::new_checked(id_of(p, "source")?)?,
        ))
    })();
    let Some(key) = key else { return };
    let why = format!("gave up: {}", http_err::why(e));
    if let Err(e) = mark(app, key, &["pending"], "skipped", Some(&why)).await {
        tracing::warn!(?e, "ban sync: marking a dropped copy failed");
    }
}

/// The servers of the group that `src` sends to now. One failing server is logged and skipped,
/// and its error returned with the rest so the job runs again for it.
async fn receivers(
    app: &App,
    src: Id<GuildMarker>,
) -> (Vec<Id<GuildMarker>>, Option<anyhow::Error>) {
    let (mut out, mut failed) = (vec![], None);
    let all = match group(app, src).await {
        Ok(g) => g,
        Err(e) => return (out, Some(e)),
    };
    let owners = Owners::default();
    for to in all {
        match grouped(app, &owners, src, to).await {
            Ok(true) => out.push(to),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(?e, %src, %to, "ban sync: group check failed");
                failed = Some(e);
            }
        }
    }
    (out, failed)
}

/// `ban_sync_share` in the source server, after the hold.
pub async fn share_job(app: &App, src: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(need(p, "user")?);
    let cfg = app.cfg(src.get()).await?;
    let c = &cfg.ban_sync;
    if !(c.enabled && c.share) || is_copy(app, src, user).await? {
        return Ok(());
    }
    // Only a 404 means the ban was lifted or reverted in the hold; other errors retry the job.
    let Some(ban) = http_err::ban(&app.http, src, user).await? else {
        return Ok(());
    };
    // A Raider List ban stays in its server: every server decides through its own Raider List
    // setting, which sharing would go around.
    if ban
        .reason
        .is_some_and(|r| r.starts_with(crate::raiders::PREFIX))
    {
        return Ok(());
    }
    let at = p["at"].as_i64().unwrap_or_else(now_secs);
    let actor = id_of(p, "actor");
    let case = bot_case(app, src, user, actor, &["ban", "tempban"], at).await?;
    let who = behind(app.me, actor, case.map(|c| c.1));
    if stopped(app, src, who, Some(at)).await? {
        return Ok(());
    }
    let reason = p["reason"].as_str().unwrap_or_default();
    let since = Some(at - CASE_WINDOW_S);
    let origin = origin(app, (src, user), actor, case, since, reason).await?;
    if !sends(c, origin) {
        return Ok(());
    }
    let (targets, failed) = receivers(app, src).await;
    if !targets.is_empty() {
        let copy = Copy {
            origin,
            case: case.map(|c| c.0),
            reason: embed::clip(reason, 400),
            expires: temp_end(app, src, user).await?,
            actor,
            moderator: staff(who),
            banned_at: Some(at),
        };
        let mut capped = false;
        for to in targets {
            capped |= offer(app, (to, user, src), &copy).await?.1;
        }
        if capped {
            paused(app, src).await;
        }
    }
    failed.map_or(Ok(()), Err)
}

/// Moves a row on from one of `from`; false when another run already did.
async fn mark(app: &App, key: Key, from: &[&str], state: &str, why: Option<&str>) -> Result<bool> {
    mark_in(&app.pool, key, from, state, why).await
}

async fn mark_in<'e>(
    db: impl sqlx::PgExecutor<'e>,
    (to, user, src): Key,
    from: &[&str],
    state: &str,
    why: Option<&str>,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE ban_sync_links SET state = $5, skip_reason = $6, done_at = now()
         WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3 AND state = ANY($4)",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(from)
    .bind(state)
    .bind(why)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// Pending (or banned without a case, after a failed try) becomes banned; returns the case the
/// row has, `None` inside when it still needs one, or `None` when another run finished it. A ban
/// case for the user made since the row (by a try that stopped before the row was updated, or by
/// a Ban here click) is taken over instead of writing a second one.
async fn claim_banned(
    app: &App,
    (to, user, src): Key,
    case: Option<i32>,
) -> Result<Option<Option<i32>>> {
    Ok(sqlx::query_scalar(
        "UPDATE ban_sync_links l SET state = 'banned', done_at = now(), case_no = COALESCE(l.case_no, $4,
             (SELECT c.case_no FROM cases c WHERE c.guild_id = $1 AND c.user_id = $2
                AND c.kind IN ('ban', 'tempban') AND c.created_at >= l.created_at
              ORDER BY c.case_no DESC LIMIT 1))
         WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3
           AND (state = 'pending' OR (state = 'banned' AND case_no IS NULL))
         RETURNING case_no",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(case)
    .fetch_optional(&app.pool)
    .await?)
}

#[derive(sqlx::FromRow)]
struct Row {
    state: String,
    reason: String,
    expires: Option<i64>,
    source_case: Option<i32>,
    case_no: Option<i32>,
    actor_id: Option<i64>,
    mod_id: Option<i64>,
    banned_at: Option<i64>,
    over_reason: Option<String>,
    temp_end: Option<i64>,
}

impl Row {
    fn behind(&self, me: Id<UserMarker>) -> Behind {
        behind(me, self.actor_id.map(|a| a as u64), self.mod_id)
    }
}

async fn load(app: &App, (to, user, src): Key) -> Result<Option<Row>> {
    Ok(sqlx::query_as(
        "SELECT state, reason, extract(epoch FROM expires_at)::bigint AS expires, source_case, case_no,
                actor_id, mod_id, extract(epoch FROM banned_at)::bigint AS banned_at, over_reason,
                extract(epoch FROM temp_end)::bigint AS temp_end
         FROM ban_sync_links WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .fetch_optional(&app.pool)
    .await?)
}

/// `ban_sync_copy` in the receiving server. A 404 on the way (the user does not exist) skips it.
pub async fn copy_job(app: &App, to: Id<GuildMarker>, p: &Value) -> Result<()> {
    let key = (to, Id::new(need(p, "user")?), Id::new(need(p, "source")?));
    match copy(app, key).await {
        Err(e) if http_err::gone_any(&e) => {
            let why = http_err::why(&e);
            mark(app, key, &["pending"], "skipped", Some(&why))
                .await
                .map(drop)
        }
        r => r,
    }
}

async fn copy(app: &App, key: Key) -> Result<()> {
    let (to, user, src) = key;
    let Some(row) = load(app, key).await? else {
        return Ok(());
    };
    let name = verify::guild_name(app, src).await;
    let why = copy_reason(&name, src.get(), &row.reason);
    // An earlier try banned but its case failed: only the case is left to write.
    if row.state == "banned" && row.case_no.is_none() && banned_by_us(app, key).await? {
        return finish_ban(app, key, None, &why).await;
    }
    // A second run of the same job finds the row done and stops.
    if row.state != "pending" {
        return Ok(());
    }
    // A ban made by a try or a Ban here click that stopped before the row was updated.
    let ban = http_err::ban(&app.http, to, user).await?;
    let reason = ban.as_ref().map(|b| b.reason.as_deref());
    let mine = owned(app, key, &row, reason).await?;
    let owners = Owners::default();
    match blocked(app, &owners, key, &row).await? {
        // The source no longer wants the ban: lifted while the row is still pending, so a failed
        // unban is retried, never left behind.
        Some((_, LIFTED | CONTAINED)) if mine => return lift(app, key, &name).await,
        // A server out of the group keeps its copies as ordinary bans; staff hear about an exempt one.
        Some((state, reason)) if mine => {
            finish_ban(app, key, None, &why).await?;
            if state == "skipped" {
                kept_exempt(app, key, reason).await;
            }
            return Ok(());
        }
        Some((state, reason)) => {
            return mark(app, key, &["pending"], state, Some(reason))
                .await
                .map(drop)
        }
        None if mine => return finish_ban(app, key, None, &why).await,
        None => {}
    }
    let skip = |why: &'static str| mark(app, key, &["pending"], "skipped", Some(why));
    // So a later unban there never lifts a ban this server made itself. A TommyBot temp ban here
    // that ends before the copy would is not a reason to skip: the copy outlasts it (its ban
    // drops the temp ban's unban job), and the row remembers it so a lift can put it back.
    if let Some(reason) = reason {
        let local = temp_end(app, to, user).await?;
        // A temp ban whose end is due or whose unban runs now may be gone in a second: retry later.
        if local.is_some() && temp_ending(app, to, user).await? {
            anyhow::bail!("ban sync: the local temp ban is ending, retrying later");
        }
        if !outlasts(row.expires, local) {
            return skip("already banned").await.map(drop);
        }
        over_temp(app, key, reason.unwrap_or(""), local).await?;
    }
    let duration = match remaining(row.expires) {
        Ok(d) => d,
        Err(why) => return skip(why).await.map(drop),
    };
    if app.cfg(to.get()).await?.ban_sync.receive == BanSyncReceive::Alert {
        if !mark(app, key, &["pending"], "alerted", None).await? {
            return Ok(());
        }
        let log = alert_log(
            &name,
            src.get(),
            user.get(),
            &row.reason,
            row.source_case,
            row.expires,
        );
        let ban_here = embed::button(
            "Ban here",
            &format!("{BUTTON}{src}:{user}"),
            ButtonStyle::Danger,
        );
        let rows = [verify::row(ban_here)];
        let sent = crate::events::send_log_rows(app, to.get(), LogKind::ModCommand, log, &rows);
        if let Err(e) = sent.await {
            tracing::warn!(?e, "ban sync alert log failed");
        }
        return Ok(());
    }
    let owner = owners.need(app, to).await?;
    ban_copy(app, key, owner, &why, duration).await
}

const LIFTED: &str = "source ban lifted";
const CONTAINED: &str = "source moderator contained";

/// A ban made before the member turned out exempt stays; the mod log says so.
async fn kept_exempt(app: &App, (to, user, src): Key, why: &str) {
    let log = Log::new("Ban Sync kept a ban", ORANGE)
        .user("Member", user.get())
        .line("Server", &format!("*`{src}`*"))
        .text(&format!(
            "This member is exempt here ({why}), but the ban was already made. Unban them if it should not stay."
        ));
    let sent = crate::events::send_log(app, to.get(), LogKind::ModCommand, log);
    if let Err(e) = sent.await {
        tracing::warn!(?e, "ban sync: exempt log failed");
    }
}

/// Why a pending copy may not go ahead now, as (state, reason): time has passed since the share,
/// so everything is checked again.
async fn blocked(
    app: &App,
    owners: &Owners,
    (to, user, src): Key,
    row: &Row,
) -> Result<Option<(&'static str, &'static str)>> {
    if !grouped(app, owners, src, to).await? {
        return Ok(Some(("cancelled", "not in the same group")));
    }
    if http_err::ban(&app.http, src, user).await?.is_none() {
        return Ok(Some(("cancelled", LIFTED)));
    }
    if stopped(app, src, row.behind(app.me), row.banned_at).await? {
        return Ok(Some(("cancelled", CONTAINED)));
    }
    let level = app.level(to, owners.need(app, to).await?, user).await?;
    Ok(exempt(&level, user, app.me, app.bot_owner).map(|why| ("skipped", why)))
}

/// Bans through the shared ban path, without a DM. The row stays pending until the ban is made,
/// so a failure leaves it for the job to retry; only a refusal skips it.
async fn ban_copy(
    app: &App,
    key: Key,
    owner: Id<UserMarker>,
    why: &str,
    duration: Option<i64>,
) -> Result<()> {
    let (to, user, _) = key;
    let flags = Flags {
        no_dm: true,
        compact: true,
        ..Default::default()
    };
    let me = (app.me, &Level::Owner);
    let done = commands::apply_dm(
        app,
        to,
        owner,
        me,
        ModKind::Ban,
        user,
        Some(why),
        duration,
        0,
        flags,
        None,
    )
    .await?;
    match done.case {
        Some(n) => finish_ban(app, key, Some(n), why).await,
        // A refused case after a ban that went through still counts as ours.
        None if banned_by_us(app, key).await? => finish_ban(app, key, None, why).await,
        None => mark(app, key, &["pending"], "skipped", Some(&done.text))
            .await
            .map(drop),
    }
}

/// The ban is made: the row becomes banned, then it gets its case if it has none yet.
async fn finish_ban(app: &App, key: Key, case: Option<i32>, why: &str) -> Result<()> {
    let (to, user, src) = key;
    drop_temp_job(app, key).await?;
    let Some(None) = claim_banned(app, key, case).await? else {
        return Ok(());
    };
    let inverse = json!({"op": "unban", "user": user.get()});
    let n = new_case(app, (to, user), "ban", why, Some(inverse)).await?;
    sqlx::query(
        "UPDATE ban_sync_links SET case_no = $4
         WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3 AND case_no IS NULL",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(n)
    .execute(&app.pool)
    .await?;
    commands::mod_log(app, to, (n, "ban"), (app.me, user), None, Some(why)).await;
    Ok(())
}

/// A case with the bot as moderator.
async fn new_case(
    app: &App,
    (to, user): (Id<GuildMarker>, Id<UserMarker>),
    kind: &str,
    why: &str,
    inverse: Option<Value>,
) -> Result<i32> {
    let case = cases::NewCase {
        guild_id: to.get() as i64,
        kind,
        user_id: user.get() as i64,
        mod_id: app.me.get() as i64,
        reason: Some(why),
        points: 0,
        duration_s: None,
        proof: None,
        inverse,
    };
    cases::create(&app.pool, case).await
}

/// The case and audit reason of a copy; the audit header is cut to Discord's limit by `apply_dm`.
fn copy_reason(name: &str, src: u64, reason: &str) -> String {
    format!("{PREFIX} {} ({src}): {}", md(name), reason_md(reason))
}

fn reason_md(reason: &str) -> String {
    if reason.is_empty() {
        "No reason provided".into()
    } else {
        md(reason)
    }
}

/// The mod log alert in a server that receives as Alert, with a Ban here button under it.
pub(crate) fn alert_log(
    name: &str,
    src: u64,
    user: u64,
    reason: &str,
    case: Option<i32>,
    expires: Option<i64>,
) -> Log {
    let mut log = Log::new("Banned in your other server", ORANGE)
        .user("Member", user)
        .line(
            "Server",
            &format!("{} *`{src}`*", md(&embed::clip(name, 100))),
        )
        .line("Reason", &reason_md(reason));
    if let Some(c) = case {
        log = log.line("Case", &format!("`{c}`"));
    }
    if let Some(t) = expires {
        log = log.line("Duration", &format!("until <t:{t}:f>"));
    }
    log.text("Press **Ban here** to ban them in this server too.")
}

/// `ban_sync_lift` in the source server, after the hold.
pub async fn lift_job(app: &App, src: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(need(p, "user")?);
    let cfg = app.cfg(src.get()).await?;
    let c = &cfg.ban_sync;
    if !(c.enabled && c.sync_unbans) {
        return Ok(());
    }
    // Banned again in the hold: nothing to lift.
    if http_err::ban(&app.http, src, user).await?.is_some() {
        return Ok(());
    }
    let at = p["at"].as_i64().unwrap_or_else(now_secs);
    let actor = id_of(p, "actor");
    let case = bot_case(app, src, user, actor, &["unban"], at).await?;
    if stopped(app, src, behind(app.me, actor, case.map(|c| c.1)), Some(at)).await? {
        return Ok(());
    }
    sqlx::query(
        "UPDATE ban_sync_links SET state = 'lifted', done_at = now()
         WHERE source_guild = $1 AND user_id = $2 AND state = 'alerted'",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .execute(&app.pool)
    .await?;
    let copies: Vec<i64> = sqlx::query_scalar(
        "SELECT guild_id FROM ban_sync_links WHERE source_guild = $1 AND user_id = $2 AND state = 'banned'",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    let name = verify::guild_name(app, src).await;
    let (owners, mut failed) = (Owners::default(), None);
    for to in copies {
        let to = Id::new(to as u64);
        // A server that left the group keeps its copies as ordinary bans.
        let done = match same_owner(app, &owners, src, to).await {
            Ok(true) => lift(app, (to, user, src), &name).await,
            r => r.map(drop),
        };
        if let Err(e) = done {
            tracing::warn!(?e, %src, %to, "ban sync: lifting a copy failed");
            failed = Some(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

/// The rows `lift` acts on: copies, and a pending row whose ban was made by a try that stopped.
const LIVE: &[&str] = &["pending", "banned"];

/// Unbans a copy with an unban case, unless staff banned again by hand since. A copy laid over a
/// local temp ban that has not ended yet gives that temp ban back instead: the ban stays, with
/// its unban job at the old end.
async fn lift(app: &App, key: Key, name: &str) -> Result<()> {
    let (to, user, src) = key;
    let Some(ban) = http_err::ban(&app.http, to, user).await? else {
        return mark(app, key, LIVE, "lifted", None).await.map(drop);
    };
    let Some(row) = load(app, key).await? else {
        return Ok(());
    };
    if !owned(app, key, &row, Some(ban.reason.as_deref())).await? {
        let why = Some("banned again by hand");
        return mark(app, key, LIVE, "skipped", why).await.map(drop);
    }
    if let Some(end) = row.temp_end.filter(|e| *e > now_secs()) {
        // One transaction: the old end's job, the copy's own unban job gone, and the row lifted.
        let mut tx = app.pool.begin().await?;
        let g = to.get() as i64;
        let job = jobs::enqueue(&mut *tx, g, "unban", json!({"user": user.get()}), end).await?;
        jobs::cancel_user(&mut *tx, g, &["unban"], user.get(), Some(job)).await?;
        if mark_in(
            &mut *tx,
            key,
            LIVE,
            "lifted",
            Some("the local temp ban runs on"),
        )
        .await?
        {
            tx.commit().await?;
        }
        return Ok(());
    }
    let why = format!("Ban Sync: lifted in {} ({src})", md(name));
    let audit = commands::audit_reason(Some(&why), app.me);
    match app.http.delete_ban(to, user).reason(&audit).await {
        Err(e) if !http_err::gone(&e) => return Err(e.into()),
        _ => {}
    }
    // A temp copy's unban job must not lift a ban staff make later.
    crate::scheduler::forget_jobs(app, to, user.get(), &["unban"], None).await;
    let case_no = new_case(app, (to, user), "unban", &why, None).await?;
    mark(app, key, LIVE, "lifted", None).await?;
    commands::mod_log(
        app,
        to,
        (case_no, "unban"),
        (app.me, user),
        None,
        Some(&why),
    )
    .await;
    Ok(())
}

/// Keeps `guild_owners` current (GuildCreate, GuildUpdate). A new owner takes the server out of
/// its old group at once: its pending copies are cancelled and both owners hear about it.
pub async fn on_owner(app: &App, gid: Id<GuildMarker>, owner: Id<UserMarker>) -> Result<()> {
    let old: Option<i64> = sqlx::query_scalar(
        "WITH old AS (SELECT owner_id FROM guild_owners WHERE guild_id = $1)
         INSERT INTO guild_owners (guild_id, owner_id) VALUES ($1, $2)
         ON CONFLICT (guild_id) DO UPDATE SET owner_id = $2, seen_at = now()
         RETURNING (SELECT owner_id FROM old)",
    )
    .bind(gid.get() as i64)
    .bind(owner.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    let Some(old) = old.map(|o| Id::new(o as u64)).filter(|o| *o != owner) else {
        return Ok(());
    };
    app.invalidate_guild(gid);
    cancel(app, gid, "owner changed").await?;
    if app.cfg(gid.get()).await?.ban_sync.enabled {
        let name = md(&verify::guild_name(app, gid).await);
        let text = format!("**{name}** has a new owner, so it left its Ban Sync group: bans are no longer copied between it and the other servers of its old owner. Copies already made stay as normal bans.");
        reply::dm(app, old, &text).await;
        reply::dm(app, owner, &text).await;
    }
    Ok(())
}

/// The bot left the server: it is out of the group, and its pending copies are cancelled.
pub async fn on_removed(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    sqlx::query("DELETE FROM guild_owners WHERE guild_id = $1")
        .bind(gid.get() as i64)
        .execute(&app.pool)
        .await?;
    cancel(app, gid, "bot removed").await
}

async fn cancel(app: &App, gid: Id<GuildMarker>, why: &str) -> Result<()> {
    sqlx::query(
        "UPDATE ban_sync_links SET state = 'cancelled', skip_reason = $2, done_at = now()
         WHERE state IN ('pending', 'alerted') AND (guild_id = $1 OR source_guild = $1)",
    )
    .bind(gid.get() as i64)
    .bind(why)
    .execute(&app.pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "bansync_tests.rs"]
pub(crate) mod tests;
