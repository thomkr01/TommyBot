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
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, MessageFlags,
    },
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    http::interaction::InteractionResponseType,
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

/// Start of the reason every copy carries; an unban lifts only bans that still start with it.
pub const PREFIX: &str = "Ban Sync from";
const BUTTON: &str = "bansync:";
const REST: &str = "rest:";
/// A bot ban is a staff command when a staff member's case for it is at most this old.
const CASE_WINDOW_S: i64 = 300;

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

/// Why a user is never banned by a copy in this server, if they are not.
fn exempt(
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

fn id_of(p: &Value, k: &str) -> Option<u64> {
    p[k].as_u64().or_else(|| p[k].as_str()?.parse().ok())
}

fn need(p: &Value, k: &str) -> Result<u64> {
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
        "at": now_secs(),
    });
    let at = now_secs() + BAN_SYNC_HOLD_S;
    jobs::enqueue(&app.pool, gid.get() as i64, kind, job, at).await?;
    Ok(())
}

/// After the hold: the ban still exists and its actor was not contained since. A merged audit
/// entry names no actor, so that check is skipped then.
async fn held(
    app: &App,
    src: Id<GuildMarker>,
    user: Id<UserMarker>,
    p: &Value,
    banned: bool,
) -> Result<bool> {
    // Only a 404 means "not banned"; any other error retries the job.
    if http_err::ban(&app.http, src, user).await?.is_some() != banned {
        return Ok(false);
    }
    let at = p["at"].as_i64().unwrap_or_else(now_secs);
    match id_of(p, "actor").map(Id::new) {
        Some(a) => Ok(!crate::antinuke::contained_since(app, src, a, at).await?),
        None => {
            tracing::info!(%src, %user, "ban sync: actor unknown, containment not checked");
            Ok(true)
        }
    }
}

/// What made the ban, and its case. Runs after the hold, so `apply_dm` has written its case.
async fn origin(
    app: &App,
    src: Id<GuildMarker>,
    user: Id<UserMarker>,
    actor: Option<u64>,
    at: i64,
) -> Result<(Origin, Option<i32>)> {
    if actor != Some(app.me.get()) {
        return Ok((Origin::Discord, None));
    }
    let case: Option<(i32, i64)> = sqlx::query_as(
        "SELECT case_no, mod_id FROM cases WHERE guild_id = $1 AND user_id = $2
           AND kind IN ('ban', 'tempban') AND created_at >= to_timestamp($3::float8)
         ORDER BY case_no DESC LIMIT 1",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .bind((at - CASE_WINDOW_S) as f64)
    .fetch_optional(&app.pool)
    .await?;
    if let Some((no, by)) = case {
        // Automod filters and warn thresholds write cases with the bot as moderator.
        let o = if by as u64 == app.me.get() {
            Origin::Automod
        } else {
            Origin::StaffCmd
        };
        return Ok((o, Some(no)));
    }
    let gate: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM joingate_actions WHERE guild_id = $1 AND user_id = $2
           AND action = 'Ban' AND created_at >= to_timestamp($3::float8)
         ORDER BY id DESC LIMIT 1",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .bind((at - CASE_WINDOW_S) as f64)
    .fetch_optional(&app.pool)
    .await?;
    // Verification bans are the only bot bans that leave no case and no Join Gate record.
    Ok((gate.map_or(Origin::Verify, |r| gate_origin(&r)), None))
}

/// The end of a TommyBot temp ban: its pending unban job.
async fn temp_end(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<Option<i64>> {
    Ok(
        jobs::of_user(&app.pool, gid.get() as i64, "unban", user.get())
            .await?
            .map(|j| j.1),
    )
}

/// Same Discord owner, from `guild_owners` and confirmed live for both servers.
async fn same_owner(app: &App, a: Id<GuildMarker>, b: Id<GuildMarker>) -> Result<bool> {
    let stored: Vec<i64> =
        sqlx::query_scalar("SELECT owner_id FROM guild_owners WHERE guild_id = ANY($1)")
            .bind([a.get() as i64, b.get() as i64].as_slice())
            .fetch_all(&app.pool)
            .await?;
    let (oa, ob) = (
        app.guild_info(a).await?.owner,
        app.guild_info(b).await?.owner,
    );
    Ok(oa == ob && stored.len() == 2 && stored.iter().all(|o| *o as u64 == oa.get()))
}

/// `src` sends and `to` receives, and they have the same owner.
async fn grouped(app: &App, src: Id<GuildMarker>, to: Id<GuildMarker>) -> Result<bool> {
    let (a, b) = (app.cfg(src.get()).await?, app.cfg(to.get()).await?);
    let on = a.ban_sync.enabled
        && a.ban_sync.share
        && b.ban_sync.enabled
        && b.ban_sync.receive != BanSyncReceive::Off;
    Ok(on && same_owner(app, src, to).await?)
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

/// Users shared from `src` in the last hour, the ones held back by the cap left out.
async fn shared_last_hour(app: &App, src: Id<GuildMarker>) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(DISTINCT user_id) FROM ban_sync_links WHERE source_guild = $1
           AND created_at > now() - interval '1 hour' AND skip_reason IS DISTINCT FROM 'rate cap'",
    )
    .bind(src.get() as i64)
    .fetch_one(&app.pool)
    .await?)
}

struct Copy {
    origin: Origin,
    case: Option<i32>,
    reason: String,
    expires: Option<i64>,
}

/// Writes the row (pending, or skipped when over the cap), then queues the copy. An active
/// pending or banned row is left alone; a finished one starts over, so a re-ban syncs again.
/// True when a copy was queued.
async fn offer(
    app: &App,
    (to, user, src): (Id<GuildMarker>, Id<UserMarker>, Id<GuildMarker>),
    c: &Copy,
    capped: bool,
) -> Result<bool> {
    let (state, skip) = if capped {
        ("skipped", Some("rate cap"))
    } else {
        ("pending", None)
    };
    let written = sqlx::query(
        "INSERT INTO ban_sync_links
             (guild_id, user_id, source_guild, source_case, origin, reason, expires_at, state, skip_reason)
         VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7::float8), $8, $9)
         ON CONFLICT (guild_id, user_id, source_guild) DO UPDATE SET
             source_case = excluded.source_case, origin = excluded.origin, reason = excluded.reason,
             expires_at = excluded.expires_at, state = excluded.state,
             skip_reason = excluded.skip_reason, case_no = NULL, created_at = now(), done_at = NULL
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
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1;
    if written && !capped {
        enqueue_copy(app, to, user, src).await?;
    }
    Ok(written && !capped)
}

async fn enqueue_copy(
    app: &App,
    to: Id<GuildMarker>,
    user: Id<UserMarker>,
    src: Id<GuildMarker>,
) -> Result<()> {
    let job = json!({"user": user.get(), "source": src.get()});
    jobs::enqueue(&app.pool, to.get() as i64, "ban_sync_copy", job, now_secs()).await?;
    Ok(())
}

/// `ban_sync_share` in the source server, after the hold.
pub async fn share_job(app: &App, src: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(need(p, "user")?);
    let cfg = app.cfg(src.get()).await?;
    let c = &cfg.ban_sync;
    if !(c.enabled && c.share)
        || is_copy(app, src, user).await?
        || !held(app, src, user, p, true).await?
    {
        return Ok(());
    }
    let at = p["at"].as_i64().unwrap_or_else(now_secs);
    let (origin, case) = origin(app, src, user, id_of(p, "actor"), at).await?;
    if !sends(c, origin) {
        return Ok(());
    }
    let mut targets = vec![];
    for to in group(app, src).await? {
        if grouped(app, src, to).await? {
            targets.push(to);
        }
    }
    if targets.is_empty() {
        return Ok(());
    }
    let copy = Copy {
        origin,
        case,
        reason: embed::clip(p["reason"].as_str().unwrap_or_default(), 400),
        expires: temp_end(app, src, user).await?,
    };
    let capped = shared_last_hour(app, src).await? >= BAN_SYNC_HOURLY;
    for to in targets {
        offer(app, (to, user, src), &copy, capped).await?;
    }
    if capped {
        paused(app, src).await;
    }
    Ok(())
}

/// Over the cap the rest waits for the owner: a burst like that is how a hacked account looks.
async fn paused(app: &App, src: Id<GuildMarker>) {
    static LAST: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());
    if !reply::alert_due(&mut LAST.lock().unwrap(), src.get(), now_secs(), 3600) {
        return;
    }
    let owner = match app.guild_info(src).await {
        Ok(i) => i.owner,
        Err(e) => return tracing::warn!(?e, "ban sync pause DM: guild lookup failed"),
    };
    let name = verify::guild_name(app, src).await;
    let text = format!(
        "{WARNING} **Ban Sync paused for {name}.** More than {BAN_SYNC_HOURLY} bans were made there in the last hour, so the rest were not sent to your other servers. If you did not expect that many bans, check that server's staff first. If the bans are real, press **Send the rest** to send the next {BAN_SYNC_HOURLY}."
    );
    let row = buttons(vec![button(
        "Send the rest",
        format!("{BUTTON}{REST}{src}"),
        ButtonStyle::Primary,
    )]);
    reply::dm_buttons(app, owner, &text, &[row]).await;
}

fn button(label: &str, id: String, style: ButtonStyle) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

fn buttons(components: Vec<Component>) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components,
    })
}

/// Moves a row on from one of `from`; false when another run already did.
async fn mark(
    app: &App,
    (to, user, src): (Id<GuildMarker>, Id<UserMarker>, Id<GuildMarker>),
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
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1)
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

async fn copy(app: &App, key: (Id<GuildMarker>, Id<UserMarker>, Id<GuildMarker>)) -> Result<()> {
    let (to, user, src) = key;
    let row: Option<(String, String, Option<i64>, Option<i32>)> = sqlx::query_as(
        "SELECT state, reason, extract(epoch FROM expires_at)::bigint, source_case
         FROM ban_sync_links WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3",
    )
    .bind(to.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    // A second run of the same job finds the row done and stops.
    let Some((_, reason, expires, source_case)) = row.filter(|r| r.0 == "pending") else {
        return Ok(());
    };
    let skip = |why: &'static str| mark(app, key, &["pending"], "skipped", Some(why));
    // Time has passed since the share: everything is checked again here.
    if !grouped(app, src, to).await? {
        return mark(
            app,
            key,
            &["pending"],
            "cancelled",
            Some("not in the same group"),
        )
        .await
        .map(drop);
    }
    if http_err::ban(&app.http, src, user).await?.is_none() {
        return mark(
            app,
            key,
            &["pending"],
            "cancelled",
            Some("source ban lifted"),
        )
        .await
        .map(drop);
    }
    let owner = app.guild_info(to).await?.owner;
    let level = app.level(to, owner, user).await?;
    if let Some(why) = exempt(&level, user, app.me, app.bot_owner) {
        return skip(why).await.map(drop);
    }
    // So a later unban there never lifts a ban this server made itself.
    if http_err::ban(&app.http, to, user).await?.is_some() {
        return skip("already banned").await.map(drop);
    }
    let duration = match expires {
        Some(t) if t <= now_secs() => return skip("temp ban already ended").await.map(drop),
        Some(t) => Some(t - now_secs()),
        None => None,
    };
    let name = verify::guild_name(app, src).await;
    match app.cfg(to.get()).await?.ban_sync.receive {
        BanSyncReceive::Off => Ok(()),
        BanSyncReceive::Alert => {
            if !mark(app, key, &["pending"], "alerted", None).await? {
                return Ok(());
            }
            let log = alert_log(&name, src.get(), user.get(), &reason, source_case, expires);
            let row = buttons(vec![button(
                "Ban here",
                format!("{BUTTON}{src}:{user}"),
                ButtonStyle::Danger,
            )]);
            let sent =
                crate::events::send_log_rows(app, to.get(), LogKind::ModCommand, log, &[row]).await;
            if let Err(e) = sent {
                tracing::warn!(?e, "ban sync alert log failed");
            }
            Ok(())
        }
        BanSyncReceive::Ban => {
            // Claimed before the ban, so a job that runs twice bans and writes a case once.
            // shortcut: a crash between this claim and the ban leaves a banned row without a ban; a later unban there finds none.
            let claimed: Option<i32> = sqlx::query_scalar(
                "UPDATE ban_sync_links SET state = 'banned', done_at = now()
                 WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3
                   AND state = 'pending' AND case_no IS NULL RETURNING 1",
            )
            .bind(to.get() as i64)
            .bind(user.get() as i64)
            .bind(src.get() as i64)
            .fetch_optional(&app.pool)
            .await?;
            if claimed.is_none() {
                return Ok(());
            }
            let why = copy_reason(&name, src.get(), &reason);
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
                Some(&why),
                duration,
                0,
                flags,
                None,
            )
            .await;
            let case = match &done {
                Ok(a) => a.case,
                Err(_) => None,
            };
            match case {
                Some(n) => {
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
                }
                None => {
                    let text = match &done {
                        Ok(a) => a.text.clone(),
                        Err(e) => http_err::why(e),
                    };
                    mark(app, key, &["banned"], "skipped", Some(&text)).await?;
                }
            }
            done.map(drop)
        }
    }
}

/// The case and audit reason of a copy; the audit header is cut to Discord's limit by `apply_dm`.
fn copy_reason(name: &str, src: u64, reason: &str) -> String {
    let reason = if reason.is_empty() {
        "No reason provided"
    } else {
        reason
    };
    format!("{PREFIX} {name} ({src}): {reason}")
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
        .line("Server", &format!("`{}` *`{src}`*", embed::clip(name, 100)))
        .line(
            "Reason",
            if reason.is_empty() {
                "No reason provided"
            } else {
                reason
            },
        );
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
    if !(c.enabled && c.sync_unbans) || !held(app, src, user, p, false).await? {
        return Ok(());
    }
    let copies: Vec<i64> = sqlx::query_scalar(
        "SELECT guild_id FROM ban_sync_links WHERE source_guild = $1 AND user_id = $2 AND state = 'banned'",
    )
    .bind(src.get() as i64)
    .bind(user.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    let name = verify::guild_name(app, src).await;
    for to in copies {
        lift(app, (Id::new(to as u64), user, src), &name).await?;
    }
    Ok(())
}

async fn lift(
    app: &App,
    key: (Id<GuildMarker>, Id<UserMarker>, Id<GuildMarker>),
    name: &str,
) -> Result<()> {
    let (to, user, src) = key;
    // A server that left the group keeps its copies as ordinary bans.
    if !same_owner(app, src, to).await? {
        return Ok(());
    }
    let ban = http_err::ban(&app.http, to, user).await?;
    let ours = ban
        .as_ref()
        .and_then(|b| b.reason.as_deref())
        .is_some_and(|r| r.starts_with(PREFIX));
    if ban.is_none() {
        return mark(app, key, &["banned"], "lifted", None).await.map(drop);
    }
    if !ours {
        return mark(
            app,
            key,
            &["banned"],
            "skipped",
            Some("banned again by hand"),
        )
        .await
        .map(drop);
    }
    let why = format!("Ban Sync: lifted in {name} ({src})");
    let audit = commands::audit_reason(Some(&why), app.me);
    match app.http.delete_ban(to, user).reason(&audit).await {
        Err(e) if !http_err::gone(&e) => return Err(e.into()),
        _ => {}
    }
    let case_no = cases::create(
        &app.pool,
        cases::NewCase {
            guild_id: to.get() as i64,
            kind: "unban",
            user_id: user.get() as i64,
            mod_id: app.me.get() as i64,
            reason: Some(&why),
            points: 0,
            duration_s: None,
            proof: None,
            inverse: None,
        },
    )
    .await?;
    mark(app, key, &["banned"], "lifted", None).await?;
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

/// `ban_sync_import` {source, target}: the source's existing bans, through the copy path and the cap.
pub async fn import_job(app: &App, p: &Value) -> Result<()> {
    let (src, to) = (Id::new(need(p, "source")?), Id::new(need(p, "target")?));
    if !grouped(app, src, to).await? {
        return Ok(());
    }
    let mut sent = shared_last_hour(app, src).await?;
    let mut after = None;
    loop {
        let req = app.http.bans(src).limit(1000);
        let req = match after {
            Some(a) => req.after(a),
            None => req,
        };
        let page = req.await?.models().await?;
        for b in &page {
            let user = b.user.id;
            // A copy in the source never chains on.
            if is_copy(app, src, user).await? {
                continue;
            }
            let copy = Copy {
                origin: Origin::Discord,
                case: None,
                reason: embed::clip(b.reason.as_deref().unwrap_or_default(), 400),
                expires: temp_end(app, src, user).await?,
            };
            if offer(app, (to, user, src), &copy, sent >= BAN_SYNC_HOURLY).await? {
                sent += 1;
            }
        }
        match page.last() {
            Some(b) if page.len() == 1000 => after = Some(b.user.id),
            _ => break,
        }
    }
    if sent >= BAN_SYNC_HOURLY {
        paused(app, src).await;
    }
    Ok(())
}

/// `ban_sync_send_rest` {source}: the next users held back by the cap, sent by the owner's choice.
pub async fn send_rest_job(app: &App, p: &Value) -> Result<()> {
    let src = need(p, "source")? as i64;
    let users: Vec<i64> = sqlx::query_scalar(
        "SELECT user_id FROM ban_sync_links
         WHERE source_guild = $1 AND state = 'skipped' AND skip_reason = 'rate cap'
         GROUP BY user_id ORDER BY min(created_at) LIMIT $2",
    )
    .bind(src)
    .bind(BAN_SYNC_HOURLY)
    .fetch_all(&app.pool)
    .await?;
    for u in users {
        let targets: Vec<i64> = sqlx::query_scalar(
            "UPDATE ban_sync_links SET state = 'pending', skip_reason = NULL
             WHERE source_guild = $1 AND user_id = $2 AND state = 'skipped' AND skip_reason = 'rate cap'
             RETURNING guild_id",
        )
        .bind(src)
        .bind(u)
        .fetch_all(&app.pool)
        .await?;
        for t in targets {
            let to = Id::new(t as u64);
            enqueue_copy(app, to, Id::new(u as u64), Id::new(src as u64)).await?;
        }
    }
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
        let name = verify::guild_name(app, gid).await;
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
         WHERE state = 'pending' AND (guild_id = $1 OR source_guild = $1)",
    )
    .bind(gid.get() as i64)
    .bind(why)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// Below the `bansync` panel: the group and the last 10 copies to or from this server.
pub async fn panel_text(app: &App, gid: Id<GuildMarker>) -> Result<String> {
    let mut names = BTreeMap::new();
    for g in group(app, gid).await? {
        names.insert(g.get(), verify::guild_name(app, g).await);
    }
    let mut out = match names.values().map(|n| format!("`{n}`")).collect::<Vec<_>>() {
        v if v.is_empty() => "**Group:** no other server of this owner has TommyBot".to_string(),
        v => format!("**Group:** {}", v.join(", ")),
    };
    let rows: Vec<(i64, i64, i64, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT guild_id, source_guild, user_id, state, skip_reason, extract(epoch FROM created_at)::bigint
         FROM ban_sync_links WHERE guild_id = $1 OR source_guild = $1 ORDER BY created_at DESC LIMIT 10",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    out.push_str("\n**Last copies:**");
    if rows.is_empty() {
        out.push_str(" none yet");
    }
    let name = |g: i64| {
        names
            .get(&(g as u64))
            .cloned()
            .unwrap_or_else(|| g.to_string())
    };
    for (to, src, user, state, why, at) in rows {
        let way = if to as u64 == gid.get() {
            format!("from `{}`", name(src))
        } else {
            format!("to `{}`", name(to))
        };
        let why = why.map_or(String::new(), |w| format!(" ({})", embed::clip(&w, 60)));
        out.push_str(&format!("\n`{state}`{why} <@{user}> {way} <t:{at}:R>"));
    }
    Ok(out)
}

/// `bansync:{src}:{user}` (Ban here, in the receiving server) and `bansync:rest:{src}` (Send the
/// rest, in the owner's DM). The id only points at a row; the clicker's rights and the DB decide.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(BUTTON) else {
        return Ok(false);
    };
    let client = app.http.interaction(Id::new(app.me.get()));
    let defer = verify::ephemeral(
        InteractionResponseType::DeferredChannelMessageWithSource,
        None,
    );
    client.create_response(i.id, &i.token, &defer).await?;
    let text = match rest.strip_prefix(REST) {
        Some(src) => send_rest_click(app, i, src).await?,
        None => ban_click(app, i, rest).await?,
    };
    client
        .create_followup(&i.token)
        .content(&text)
        .flags(MessageFlags::EPHEMERAL)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(true)
}

async fn ban_click(app: &App, i: &Interaction, rest: &str) -> Result<String> {
    let parsed = rest.split_once(':').and_then(|(s, u)| {
        Some((
            Id::new_checked(s.parse().ok()?)?,
            Id::new_checked(u.parse().ok()?)?,
        ))
    });
    let (Some(gid), Some(clicker), Some((src, user))) = (i.guild_id, i.author_id(), parsed) else {
        return Ok("This button does not work here.".into());
    };
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, clicker).await?;
    if !level.has("ban") {
        return Ok("You need the ban permit in this server to do this.".into());
    }
    let key = (gid, user, src);
    // Pending while the ban runs, so its audit event is not shared on as a new ban.
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "UPDATE ban_sync_links SET state = 'pending'
         WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3 AND state = 'alerted'
         RETURNING reason, extract(epoch FROM expires_at)::bigint",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((reason, expires)) = row else {
        return Ok("This alert was already handled.".into());
    };
    let duration = match expires {
        Some(t) if t <= now_secs() => {
            mark(
                app,
                key,
                &["pending"],
                "skipped",
                Some("temp ban already ended"),
            )
            .await?;
            return Ok("That temp ban has already ended, so nothing was done.".into());
        }
        Some(t) => Some(t - now_secs()),
        None => None,
    };
    let why = copy_reason(&verify::guild_name(app, src).await, src.get(), &reason);
    let done = commands::apply(
        app,
        gid,
        owner,
        (clicker, &level),
        ModKind::Ban,
        user,
        Some(&why),
        duration,
        0,
    )
    .await;
    let case = done.as_ref().ok().and_then(|a| a.case);
    let (state, no) = match case {
        Some(n) => ("banned", Some(n)),
        None => ("alerted", None),
    };
    sqlx::query(
        "UPDATE ban_sync_links SET state = $4, case_no = $5, done_at = now()
         WHERE guild_id = $1 AND user_id = $2 AND source_guild = $3 AND state = 'pending'",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(src.get() as i64)
    .bind(state)
    .bind(no)
    .execute(&app.pool)
    .await?;
    Ok(match done {
        Ok(a) => a.text,
        Err(e) => format!("Something went wrong: {}", http_err::why(&e)),
    })
}

async fn send_rest_click(app: &App, i: &Interaction, src: &str) -> Result<String> {
    let (Some(clicker), Some(src)) = (
        i.author_id(),
        src.parse().ok().and_then(Id::<GuildMarker>::new_checked),
    ) else {
        return Ok("This button does not work here.".into());
    };
    if app.guild_info(src).await?.owner != clicker {
        return Ok("Only the owner of that server can do this.".into());
    }
    let cfg = app.cfg(src.get()).await?;
    let c = &cfg.ban_sync;
    if !(c.enabled && c.share) {
        return Ok("Ban Sync no longer sends bans from that server.".into());
    }
    let job = json!({"source": src.get()});
    jobs::enqueue(
        &app.pool,
        src.get() as i64,
        "ban_sync_send_rest",
        job,
        now_secs(),
    )
    .await?;
    Ok(format!(
        "Sending the next {BAN_SYNC_HOURLY} held back bans."
    ))
}

#[cfg(test)]
#[path = "bansync_tests.rs"]
mod tests;
