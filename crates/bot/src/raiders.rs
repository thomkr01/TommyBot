//! Raider List: a global list of raider accounts. Evidence comes from every server, the bot owner
//! reviews it on the dashboard (strong evidence from 3 different owners lists a user by itself),
//! and servers that turn the list on act on listed users when they join.
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Mutex};
use tommybot_automod::{join::Action, window::Window};
use tommybot_core::{
    cases, jobs,
    limits::{RAIDER_AUTO_OWNERS, RAIDER_MIN_DAYS, RAIDER_MIN_MEMBERS},
    permits::{self, Level},
    raiders,
    sections::{RaiderAction, RaiderCategory, RaiderListCfg},
};
use tommybot_logging::{
    embed::{self, DONE, ORANGE, RED},
    emoji::WARNING,
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::component::{ActionRow, ButtonStyle, Component},
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App, GuildCfg},
    bansync::{self, live_owner, md, need},
    commands::{self, ModKind},
    http_err, joingate,
    modcmds::{self, ModCmd},
    reply,
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

/// Writes the evidence and its entry, then applies the auto-listing rule, in one transaction with
/// the check jobs it queues. True when the evidence is new.
async fn record(app: &App, e: &raiders::Evidence<'_>) -> Result<bool> {
    let user = Id::new(e.user as u64);
    let mut tx = app.pool.begin().await?;
    let fresh = raiders::add(&mut tx, e).await?;
    let listed = fresh && STRONG.contains(&e.kind) && auto_list(app, &mut tx, user).await?;
    tx.commit().await?;
    if listed {
        listed_dm(app, user).await;
    }
    Ok(fresh)
}

/// Strong evidence that counts: only from servers with enough members that had the bot for a
/// while, so three throwaway servers are not three independent owners.
const QUALIFIED: &str = "user_id = $1 AND kind = ANY($2)
    AND COALESCE((detail->>'members')::bigint, 0) >= $3
    AND COALESCE((detail->>'bot_joined')::bigint, 9223372036854775807)
        <= extract(epoch FROM now())::bigint - $4 * 86400";

/// A pending entry backed by qualified strong evidence from enough different owners is confirmed
/// by itself, unverified, with the category most of those owners saw, and every enabled server
/// checks for it. A bot account always waits for the bot owner.
async fn auto_list(app: &App, tx: &mut sqlx::PgConnection, user: Id<UserMarker>) -> Result<bool> {
    let u = user.get() as i64;
    let owners: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(DISTINCT source_owner) FROM raider_evidence WHERE {QUALIFIED}"
    )))
    .bind(u)
    .bind(STRONG.as_slice())
    .bind(RAIDER_MIN_MEMBERS as i64)
    .bind(RAIDER_MIN_DAYS)
    .fetch_one(&mut *tx)
    .await?;
    if owners < RAIDER_AUTO_OWNERS {
        return Ok(false);
    }
    // A failed lookup leaves it pending; the next evidence tries again.
    match async { anyhow::Ok(app.http.user(user).await?.model().await?) }.await {
        Ok(who) if !who.bot => {}
        Ok(_) => return Ok(false),
        Err(e) => {
            tracing::warn!(?e, %user, "raider list: user lookup failed, not listing yet");
            return Ok(false);
        }
    }
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "WITH s AS (SELECT kind, count(DISTINCT source_owner) AS n FROM raider_evidence
                    WHERE {QUALIFIED} GROUP BY kind)
         UPDATE raiders SET status = 'confirmed', verified = false, expires_at = NULL, updated_at = now(),
             category = (SELECT CASE kind WHEN 'auto_nuke' THEN 'nuke' WHEN 'auto_raid' THEN 'raid' ELSE 'spam' END
                         FROM s ORDER BY n DESC, kind LIMIT 1)
         WHERE user_id = $1 AND status = 'pending'"
    )))
    .bind(u)
    .bind(STRONG.as_slice())
    .bind(RAIDER_MIN_MEMBERS as i64)
    .bind(RAIDER_MIN_DAYS)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !done {
        return Ok(false);
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
    Ok(true)
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

/// A confirmed, unexpired entry in a category this server acts on.
struct Entry {
    category: String,
    note: String,
    verified: bool,
}

async fn listed(app: &App, c: &RaiderListCfg, user: Id<UserMarker>) -> Result<Option<Entry>> {
    if !c.enabled {
        return Ok(None);
    }
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT category, public_note, verified FROM raiders WHERE user_id = $1 AND status = 'confirmed'
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(user.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let acts = |cat: &str| c.categories.iter().any(|k| cat_name(*k) == cat);
    Ok(row
        .filter(|(cat, ..)| acts(cat))
        .map(|(category, note, verified)| Entry {
            category,
            note,
            verified,
        }))
}

/// What the list does to a listed member who joins: an automatic listing the bot owner has not
/// verified yet only alerts.
fn join_action(c: &RaiderListCfg, e: &Entry) -> RaiderAction {
    if UNVERIFIED_ALERT_ONLY && !e.verified {
        RaiderAction::Alert
    } else {
        c.action
    }
}

/// Never acted on here: the bot, the bot owner, the owner, extra owners, trusted admins, the
/// anti-nuke whitelist, and whoever staff let back in. `roles` when the caller has them already.
async fn exempt(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    roles: Option<&[i64]>,
) -> Result<bool> {
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM raider_allow WHERE guild_id = $1 AND user_id = $2)",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    if allowed {
        return Ok(true);
    }
    let owner = live_owner(app, gid)
        .await?
        .context("the server is not visible")?;
    let level = match roles {
        Some(r) => {
            let (g, o, u) = (gid.get() as i64, owner.get() as i64, user.get() as i64);
            permits::resolve(&app.pool, g, o, u, r).await?
        }
        None => app.level(gid, owner, user).await?,
    };
    Ok(bansync::exempt(&level, user, app.me, app.bot_owner).is_some())
}

/// On join, right after the join lock: true when the list punished the member, so the rest of
/// the join path (and Join Raid) does not punish them a second time. Errors are logged.
pub async fn on_join(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> bool {
    match enforce(app, cfg, gid, user).await {
        Ok(done) => done,
        Err(e) => {
            tracing::warn!(?e, %gid, %user, "raider list: join check failed");
            false
        }
    }
}

async fn enforce(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> Result<bool> {
    let c = &cfg.raider_list;
    let Some(entry) = listed(app, c, user).await? else {
        return Ok(false);
    };
    if exempt(app, gid, user, None).await? {
        return Ok(false);
    }
    let action = join_action(c, &entry);
    // Written before the action, so a later lift finds every ban the list may have made; a
    // quarantine older than this row is not the list's.
    sqlx::query(
        "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, $3)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET action = excluded.action, case_no = NULL, created_at = now()",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(action_name(action))
    .execute(&app.pool)
    .await?;
    let why = format!("{PREFIX} {}", entry.category);
    let gate = joingate::Detail::default();
    let u = user.get();
    let (kind, inverse) = match action {
        RaiderAction::Alert => {
            alert(app, gid, user, &entry).await;
            return Ok(false);
        }
        RaiderAction::Kick => {
            joingate::punish(app, cfg, gid, u, Action::Kick, &why, gate).await?;
            ("kick", None)
        }
        RaiderAction::Ban => {
            joingate::punish(app, cfg, gid, u, Action::Ban, &why, gate).await?;
            ("ban", Some(json!({"op": "unban", "user": u})))
        }
        RaiderAction::Quarantine => {
            let done = crate::antinuke::quarantine_because(app, gid, user, Some(&why)).await;
            let failed = done.as_ref().err().map(http_err::why);
            let what = ("Quarantine", why.as_str(), gate);
            joingate::record(app, gid, u, what, failed.as_deref()).await;
            let log = action_log(u, &entry.category, "Quarantine", failed.as_deref());
            if let Err(e) = crate::events::send_log(app, gid.get(), LogKind::JoinGate, log).await {
                tracing::warn!(?e, "raider list action log failed");
            }
            done?;
            ("quarantine", Some(json!({"op": "unquarantine", "user": u})))
        }
    };
    // The action is done: its case, with the bot as moderator, goes on the row.
    let n = bot_case(app, (gid, user), kind, &why, inverse).await?;
    sqlx::query("UPDATE raider_actions SET case_no = $3 WHERE guild_id = $1 AND user_id = $2")
        .bind(gid.get() as i64)
        .bind(u as i64)
        .bind(n)
        .execute(&app.pool)
        .await?;
    commands::mod_log(app, gid, (n, kind), (app.me, user), None, Some(&why)).await;
    Ok(true)
}

/// A case with the bot as moderator.
async fn bot_case(
    app: &App,
    (gid, user): (Id<GuildMarker>, Id<UserMarker>),
    kind: &str,
    why: &str,
    inverse: Option<Value>,
) -> Result<i32> {
    let case = cases::NewCase {
        guild_id: gid.get() as i64,
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

/// The Join Gate log line of a quarantine by the list (kick and ban use the Join Gate's own log).
pub(crate) fn action_log(user: u64, category: &str, action: &str, failed: Option<&str>) -> Log {
    let log = Log::new(
        "Raider List action",
        if failed.is_some() { RED } else { ORANGE },
    )
    .user("Member", user)
    .line("Action", &format!("`{action}`"))
    .line("Category", category);
    match failed {
        Some(e) => log.text(&format!("**Failed:** {e}")),
        None => log.text("This account is on TommyBot's Raider List."),
    }
}

/// The mod log alert for a listed member, with what staff may do about it.
pub(crate) fn alert_log(user: u64, category: &str, note: &str) -> Log {
    let mut log = Log::new("Raider List match", ORANGE)
        .user("Member", user)
        .line("Category", category);
    if !note.is_empty() {
        log = log.line("Reason", &md(&embed::clip(note, 200)));
    }
    log.text("This account is on TommyBot's Raider List of raider accounts. Ban, kick or quarantine them, or press **Allow** so the list never acts on them here.")
}

pub(crate) fn alert_rows(user: u64) -> Vec<Component> {
    let b = |label: &str, action: &str, style| {
        embed::button(label, &format!("{BUTTON}{action}:{user}"), style)
    };
    vec![Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            b("Ban", "ban", ButtonStyle::Danger),
            b("Kick", "kick", ButtonStyle::Danger),
            b("Quarantine", "quarantine", ButtonStyle::Secondary),
            b("Allow", "allow", ButtonStyle::Success),
        ],
    })]
}

async fn alert(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, e: &Entry) {
    let log = alert_log(user.get(), &e.category, &e.note);
    let rows = alert_rows(user.get());
    let sent = crate::events::send_log_rows(app, gid.get(), LogKind::ModCommand, log, &rows);
    if let Err(e) = sent.await {
        tracing::warn!(?e, "raider list alert failed");
    }
}

/// Alerts once for a listed member already in the server.
async fn alert_member(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    roles: &[i64],
    entry: &Entry,
) -> Result<()> {
    if exempt(app, gid, user, Some(roles)).await? {
        return Ok(());
    }
    let fresh = sqlx::query(
        "INSERT INTO raider_actions (guild_id, user_id, action) VALUES ($1, $2, 'alert') ON CONFLICT DO NOTHING",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1;
    if fresh {
        alert(app, gid, user, entry).await;
    }
    Ok(())
}

/// The servers a fan-out job reaches: every server with the list on (check), or every server
/// where the list did something to the user (lift).
async fn targets(app: &App, kind: &str, user: i64) -> Result<Vec<i64>> {
    let q = match kind {
        "raider_check" => sqlx::query_scalar(
            "SELECT o.guild_id FROM guild_owners o
             JOIN LATERAL (SELECT config FROM guild_config WHERE guild_id = o.guild_id
                           ORDER BY version DESC LIMIT 1) c ON true
             WHERE c.config->'raider_list'->>'enabled' = 'true'",
        ),
        "raider_lift" => {
            sqlx::query_scalar("SELECT guild_id FROM raider_actions WHERE user_id = $1").bind(user)
        }
        other => anyhow::bail!("job {other} needs a server"),
    };
    Ok(q.fetch_all(&app.pool).await?)
}

/// `raider_check` / `raider_lift` {user} with server 0: one job per server, queued together.
pub async fn fan_out(app: &App, job: &jobs::Job) -> Result<()> {
    let user = need(&job.payload, "user")?;
    // `raider_notify` {user, text}: a DM the dashboard asked for (an appeal result, the bot
    // owner's decision log). A closed DM is not retried.
    if job.kind == "raider_notify" {
        let text = job.payload["text"]
            .as_str()
            .context("job payload has no text")?;
        reply::dm(app, Id::new(user), &embed::clip(text, 1900)).await;
        return Ok(());
    }
    let guilds = targets(app, &job.kind, user as i64).await?;
    let mut tx = app.pool.begin().await?;
    for g in guilds {
        let p = json!({"user": user.to_string()});
        jobs::enqueue(&mut *tx, g, &job.kind, p, now_secs()).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `raider_check` {user} in one server: a listed member already here gets an alert, never more.
pub async fn check_job(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(need(p, "user")?);
    let cfg = app.cfg(gid.get()).await?;
    if !cfg.raider_list.scan_members {
        return Ok(());
    }
    let Some(entry) = listed(app, &cfg.raider_list, user).await? else {
        return Ok(());
    };
    // Only a 404 means they are not here; any other error retries the job.
    let Some(m) = http_err::member(&app.http, gid, user).await? else {
        return Ok(());
    };
    let roles: Vec<i64> = m.roles.iter().map(|r| r.get() as i64).collect();
    alert_member(app, gid, user, &roles, &entry).await
}

/// `raider_scan`: once when a server turns the list on, every member is checked, alert only.
pub async fn scan_job(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let c = &cfg.raider_list;
    if !c.scan_members {
        return Ok(());
    }
    let mut after = None;
    loop {
        let req = app.http.guild_members(gid).limit(PAGE);
        let req = match after {
            Some(a) => req.after(a),
            None => req,
        };
        let page = req.await?.models().await?;
        let ids: Vec<i64> = page.iter().map(|m| m.user.id.get() as i64).collect();
        let hits: Vec<i64> = sqlx::query_scalar(
            "SELECT user_id FROM raiders WHERE user_id = ANY($1) AND status = 'confirmed'
               AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(&ids)
        .fetch_all(&app.pool)
        .await?;
        for m in page
            .iter()
            .filter(|m| hits.contains(&(m.user.id.get() as i64)))
        {
            if let Some(entry) = listed(app, c, m.user.id).await? {
                let roles: Vec<i64> = m.roles.iter().map(|r| r.get() as i64).collect();
                alert_member(app, gid, m.user.id, &roles, &entry).await?;
            }
        }
        match page.last() {
            Some(m) if page.len() == usize::from(PAGE) => after = Some(m.user.id),
            _ => break,
        }
    }
    Ok(())
}

/// A saved config that turns the list on (compared with the version saved before it) queues the
/// member scan, unless one is waiting.
pub async fn on_saved(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let on: Vec<bool> = sqlx::query_scalar(
        "SELECT COALESCE((config->'raider_list'->>'enabled')::boolean, false)
           AND COALESCE((config->'raider_list'->>'scan_members')::boolean, true)
         FROM guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT 2",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    if !turned_on(&on) {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO scheduled_jobs (guild_id, kind, payload, run_at)
         SELECT $1, 'raider_scan', '{}', now()
         WHERE NOT EXISTS (SELECT 1 FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'raider_scan')",
    )
    .bind(gid.get() as i64)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// The newest version scans and the one before it did not (or there was none).
fn turned_on(newest_first: &[bool]) -> bool {
    matches!(newest_first, [true] | [true, false])
}

/// `raider_lift` {user} in one server, once the entry is removed: undoes the list's ban (if it is
/// still the list's) or quarantine when the server wants that, and tells the mod log.
pub async fn lift_job(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(need(p, "user")?);
    let (g, u) = (gid.get() as i64, user.get() as i64);
    let action: Option<String> = sqlx::query_scalar(
        "SELECT action FROM raider_actions WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(g)
    .bind(u)
    .fetch_optional(&app.pool)
    .await?;
    let Some(action) = action else {
        return Ok(());
    };
    let removed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM raiders WHERE user_id = $1 AND status = 'removed')",
    )
    .bind(u)
    .fetch_one(&app.pool)
    .await?;
    if !removed {
        return Ok(());
    }
    // No row lock across the Discord calls: they are safe to repeat, and the row is claimed by
    // deleting it afterwards, so only one run writes the case and the notice.
    let lift = app.cfg(gid.get()).await?.raider_list.lift_on_removal;
    let (result, unbanned) = match action.as_str() {
        "ban" if lift => unban(app, gid, user).await?,
        "quarantine" if lift => {
            // Only a quarantine made since the list acted is the list's to release.
            let ours: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM quarantines q JOIN raider_actions a USING (guild_id, user_id)
                                WHERE q.guild_id = $1 AND q.user_id = $2 AND q.created_at >= a.created_at)",
            )
            .bind(g)
            .bind(u)
            .fetch_one(&app.pool)
            .await?;
            if ours {
                let inv = tommybot_antinuke::Inverse::Unquarantine { user: user.get() };
                crate::antinuke::run_inverse(app, gid, &inv).await?;
                ("Released from quarantine.", false)
            } else {
                ("Kept: not quarantined by the list.", false)
            }
        }
        "ban" | "quarantine" => (
            "This server keeps what the list did (Lift When Removed is off).",
            false,
        ),
        _ => ("Nothing to undo.", false),
    };
    let claimed = sqlx::query("DELETE FROM raider_actions WHERE guild_id = $1 AND user_id = $2")
        .bind(g)
        .bind(u)
        .execute(&app.pool)
        .await?
        .rows_affected()
        == 1;
    if !claimed {
        return Ok(());
    }
    if unbanned {
        let why = format!("{PREFIX} removed from the list");
        let n = bot_case(app, (gid, user), "unban", &why, None).await?;
        commands::mod_log(app, gid, (n, "unban"), (app.me, user), None, Some(&why)).await;
    }
    let log = lift_log(user.get(), result);
    if let Err(e) = crate::events::send_log(app, gid.get(), LogKind::ModCommand, log).await {
        tracing::warn!(?e, "raider list lift log failed");
    }
    Ok(())
}

/// The scheduler dropped a lift after its last try: staff hear what is left to undo by hand.
pub async fn lift_gave_up(app: &App, job: &jobs::Job, e: &anyhow::Error) {
    let (Some(gid), Ok(user)) = (
        Id::<GuildMarker>::new_checked(job.guild_id as u64),
        need(&job.payload, "user"),
    ) else {
        return;
    };
    tracing::warn!(?e, %gid, user, "raider list: lift gave up");
    let why = http_err::why(e);
    let log = Log::new("Raider List removal failed", RED)
        .user("Member", user)
        .line("Error", &why)
        .text("The bot owner took this account off the Raider List, but TommyBot could not undo what the list did here. Unban them or release them from quarantine by hand.");
    if let Err(e) = crate::events::send_log(app, gid.get(), LogKind::ModCommand, log).await {
        tracing::warn!(?e, "raider list: give-up log failed");
    }
}

pub(crate) fn lift_log(user: u64, result: &str) -> Log {
    Log::new("Raider List removal", DONE)
        .user("Member", user)
        .line("Result", result)
        .text("The bot owner took this account off the Raider List.")
}

/// Unbans when the ban is still the one the list made; true when it unbanned.
async fn unban(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> Result<(&'static str, bool)> {
    let Some(ban) = http_err::ban(&app.http, gid, user).await? else {
        return Ok(("Not banned any more, nothing to undo.", false));
    };
    if !ban.reason.as_deref().is_some_and(|r| r.starts_with(PREFIX)) {
        return Ok(("Kept: the ban was made again by hand.", false));
    }
    let why = format!("{PREFIX} removed from the list");
    let audit = commands::audit_reason(Some(&why), app.me);
    match app.http.delete_ban(gid, user).reason(&audit).await {
        Err(e) if !http_err::gone(&e) => return Err(e.into()),
        _ => {}
    }
    Ok(("Unbanned.", true))
}

/// Staff lifted what the list did here (unban, unquarantine): it never acts on them here again.
pub async fn lifted(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, by: Id<UserMarker>) {
    let (g, u, b) = (gid.get() as i64, user.get() as i64, by.get() as i64);
    if let Err(e) = raiders::allow(&app.pool, g, u, b).await {
        tracing::warn!(?e, %gid, %user, "raider list: saving the allow failed");
    }
}

/// How long after a bot-made unban its staff record (case or action log) counts as its cause.
const STAFF_WINDOW_S: i64 = 300;

/// Audit `MemberBanRemove`. By someone other than the bot (Discord's own unban), that person let
/// the user back in. By the bot, the newest unban a staff member recorded in the last minutes
/// (a dashboard unban or an appeal) did; the bot's own lifts are no staff choice.
pub async fn on_unban(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (A::MemberBanRemove, Some(gid), Some(user), Some(by)) =
        (e.action_type, e.guild_id, e.target_id, e.user_id)
    else {
        return Ok(());
    };
    let by = if by != app.me {
        Some(by)
    } else {
        let staff: Option<i64> = sqlx::query_scalar(
            "SELECT who FROM (
                 SELECT mod_id AS who, created_at FROM cases WHERE guild_id = $1 AND user_id = $2 AND kind = 'unban'
                 UNION ALL
                 SELECT actor_id, created_at FROM action_log WHERE guild_id = $1 AND target_id = $2 AND action = 'unban'
             ) x WHERE who <> $3 AND created_at > now() - make_interval(secs => $4)
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(gid.get() as i64)
        .bind(user.get() as i64)
        .bind(app.me.get() as i64)
        .bind(STAFF_WINDOW_S as f64)
        .fetch_optional(&app.pool)
        .await?;
        staff.map(|s| Id::new(s as u64))
    };
    if let Some(by) = by {
        lifted(app, gid, user.cast(), by).await;
    }
    Ok(())
}

/// `raider:{ban|kick|quarantine|allow}:{user}` under an alert. The id is data only: the server is
/// where the button was pressed, the clicker's permit there decides, and the DB is read again.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(BUTTON) else {
        return Ok(false);
    };
    let clicked = async {
        let (action, user) = rest.split_once(':')?;
        let user = user.parse().ok().and_then(Id::new_checked)?;
        let here = (i.guild_id?, i.channel.as_ref()?.id);
        Some(click(app, here, i.author_id()?, action, user).await)
    };
    bansync::answer_click(app, i, clicked).await?;
    Ok(true)
}

/// What a click needs: the permit for the action (any of the three for Allow).
fn click_permit(level: &Level, action: &str) -> Option<bool> {
    Some(match action {
        "ban" | "kick" | "quarantine" => level.has(action),
        "allow" => ["ban", "kick", "quarantine"].iter().any(|p| level.has(p)),
        _ => return None,
    })
}

async fn click(
    app: &App,
    (gid, channel): (Id<GuildMarker>, Id<ChannelMarker>),
    clicker: Id<UserMarker>,
    action: &str,
    user: Id<UserMarker>,
) -> Result<String> {
    let owner = live_owner(app, gid)
        .await?
        .context("the server is not visible")?;
    let level = app.level(gid, owner, clicker).await?;
    match click_permit(&level, action) {
        None => return Ok("This button does not work here.".into()),
        Some(false) if action == "allow" => {
            return Ok(
                "You need the ban, kick or quarantine permit in this server to do this.".into(),
            )
        }
        Some(false) => {
            return Ok(format!(
                "You need the {action} permit in this server to do this."
            ))
        }
        Some(true) => {}
    }
    let row: Option<String> = sqlx::query_scalar(
        "SELECT action FROM raider_actions WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    if row.as_deref() != Some("alert") {
        return Ok("This alert was already handled.".into());
    }
    if action == "allow" {
        let (g, u, c) = (gid.get() as i64, user.get() as i64, clicker.get() as i64);
        raiders::allow(&app.pool, g, u, c).await?;
        return Ok(format!(
            "<@{user}> is allowed here: the Raider List will not act on them in this server again."
        ));
    }
    let Some(entry) = listed(app, &app.cfg(gid.get()).await?.raider_list, user).await? else {
        return Ok(
            "This account is no longer on the Raider List here, so nothing was done.".into(),
        );
    };
    // Their level may have changed since the alert (made staff, whitelisted).
    if exempt(app, gid, user, None).await? {
        return Ok(
            "This member is exempt here now (staff, whitelisted or allowed), so nothing was done."
                .into(),
        );
    }
    let why = format!("{PREFIX} {}", entry.category);
    let me = (clicker, &level);
    let done = match action {
        "ban" => {
            commands::apply(app, gid, owner, me, ModKind::Ban, user, Some(&why), None, 0).await?
        }
        "kick" => {
            commands::apply(
                app,
                gid,
                owner,
                me,
                ModKind::Kick,
                user,
                Some(&why),
                None,
                0,
            )
            .await?
        }
        _ => {
            let cmd = ModCmd::Quarantine { user: user.get() };
            modcmds::run(app, gid, owner, me, channel, cmd, (Some(&why), None), false).await?
        }
    };
    if done.case.is_some() {
        sqlx::query(
            "UPDATE raider_actions SET action = $3, case_no = $4 WHERE guild_id = $1 AND user_id = $2 AND action = 'alert'",
        )
        .bind(gid.get() as i64)
        .bind(user.get() as i64)
        .bind(action)
        .bind(done.case)
        .execute(&app.pool)
        .await?;
    }
    Ok(done.text)
}

/// The dashboard review queue, optionally opened on one user and decision.
fn queue_url(user: Option<(u64, &str)>) -> Option<String> {
    let base = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|b| !b.is_empty())?;
    let base = base.trim_end_matches('/');
    Some(match user {
        Some((u, d)) => format!("{base}/#/raiders?user={u}&do={d}"),
        None => format!("{base}/#/raiders"),
    })
}

/// The unix time a Discord id was made.
fn id_secs(id: u64) -> i64 {
    (((id >> 22) + 1_420_070_400_000) / 1000) as i64
}

/// One evidence row of the DM: (server, owner-less) server, kind, ref, detail, time.
type EvRow = (i64, String, String, Value, i64);

/// Everything the bot owner needs to verify an automatic listing, as DM text.
pub(crate) fn listed_text(
    user: u64,
    who: Option<(&str, bool)>,
    owners: usize,
    rows: &[(String, Option<u64>, EvRow)],
) -> String {
    let (name, bot) = who.map_or(("unknown".into(), "unknown"), |(n, b)| {
        (md(n), if b { "yes" } else { "no" })
    });
    let mut t = format!(
        "{WARNING} **Raider List: <@{user}> was listed by itself.**\nUser: `{user}` {name}, account made <t:{}:R>, bot: {bot}\nStrong evidence from **{owners}** different server owners, so every server with the list on now acts on them. Check it and press **Verify** or **Remove** (both need 2FA on the dashboard).\n**Evidence:**",
        id_secs(user)
    );
    for (server, members, (g, kind, r, detail, at)) in rows.iter().take(10) {
        let members = members.map_or(String::new(), |m| format!(", {m} members"));
        let mut extra = vec![format!("ref `{}`", md(r))];
        for (k, label) in [
            ("case", "case"),
            ("case_no", "case"),
            ("count", "count"),
            ("filter", "by"),
        ] {
            if let Some(v) = detail.get(k).filter(|v| !v.is_null()) {
                let v = v.as_str().map_or(v.to_string(), str::to_string);
                extra.push(format!("{label} {}", md(&v)));
            }
        }
        t.push_str(&format!(
            "\n• **{}** (`{g}`{members}): `{kind}` <t:{at}:R>, {}",
            md(&embed::clip(server, 60)),
            extra.join(", ")
        ));
    }
    if rows.len() > 10 {
        t.push_str(&format!("\n• +{} more on the dashboard", rows.len() - 10));
    }
    embed::clip(&t, 1900)
}

/// DMs the bot owner the full evidence of an automatic listing, with Verify and Remove links.
async fn listed_dm(app: &App, user: Id<UserMarker>) {
    let Some(owner) = app.bot_owner else { return };
    let rows: Vec<EvRow> = match sqlx::query_as(
        "SELECT source_guild, kind, ref, detail, extract(epoch FROM created_at)::bigint
         FROM raider_evidence WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user.get() as i64)
    .fetch_all(&app.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return tracing::warn!(?e, "raider list DM: reading the evidence failed"),
    };
    let owners = sqlx::query_scalar::<_, i64>(
        "SELECT count(DISTINCT source_owner) FROM raider_evidence WHERE user_id = $1 AND kind = ANY($2)",
    )
    .bind(user.get() as i64)
    .bind(STRONG.as_slice())
    .fetch_one(&app.pool)
    .await
    .unwrap_or(0);
    let mut named = vec![];
    for r in rows.into_iter().take(11) {
        let g = Id::new(r.0 as u64);
        let info = async { anyhow::Ok(app.http.guild(g).with_counts(true).await?.model().await?) };
        let (name, members) = match info.await {
            Ok(g) => (g.name, g.approximate_member_count),
            Err(_) => ("a server".into(), None),
        };
        named.push((name, members, r));
    }
    let who = async { anyhow::Ok(app.http.user(user).await?.model().await?) }
        .await
        .ok();
    let who = who.as_ref().map(|u| (u.name.as_str(), u.bot));
    let text = listed_text(user.get(), who, owners as usize, &named);
    let links: Vec<Component> = [("Verify", "verify"), ("Remove", "remove")]
        .into_iter()
        .filter_map(|(l, d)| Some(crate::welcome::link(l, queue_url(Some((user.get(), d)))?)))
        .collect();
    let rows = if links.is_empty() {
        vec![]
    } else {
        vec![Component::ActionRow(ActionRow {
            id: None,
            components: links,
        })]
    };
    reply::dm_buttons(app, owner, &text, &rows).await;
}

/// Hourly: scam listings past their end are removed (and their lifts queued), rejected entries go
/// after 30 days and unreviewed pending ones after 90, with their evidence; reviews stay. Then the
/// daily digest for the bot owner.
pub async fn sweep(app: &App) {
    if let Err(e) = expire(app).await {
        tracing::warn!(?e, "raider list: expiry failed");
    }
    if let Err(e) = digest(app).await {
        tracing::warn!(?e, "raider list: digest failed");
    }
}

async fn expire(app: &App) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let ended: Vec<i64> = sqlx::query_scalar(
        "UPDATE raiders SET status = 'removed', updated_at = now()
         WHERE status = 'confirmed' AND expires_at <= now() RETURNING user_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for u in ended {
        sqlx::query(
            "INSERT INTO raider_reviews (actor, user_id, from_status, to_status, note)
             VALUES ($1, $2, 'confirmed', 'removed', 'scam listing expired')",
        )
        .bind(app.me.get() as i64)
        .bind(u)
        .execute(&mut *tx)
        .await?;
        fan(&mut *tx, "raider_lift", Id::new(u as u64)).await?;
    }
    tx.commit().await?;
    sqlx::query(
        "WITH old AS (DELETE FROM raiders
                      WHERE (status = 'rejected' AND COALESCE(reviewed_at, updated_at) < now() - interval '30 days')
                         OR (status = 'pending' AND reviewed_at IS NULL AND updated_at < now() - interval '90 days')
                      RETURNING user_id)
         DELETE FROM raider_evidence WHERE user_id IN (SELECT user_id FROM old)",
    )
    .execute(&app.pool)
    .await?;
    // One owner's raid or spam detection makes no entry, so its evidence goes by its own age.
    sqlx::query(
        "DELETE FROM raider_evidence e WHERE created_at < now() - interval '90 days'
           AND NOT EXISTS (SELECT 1 FROM raiders r WHERE r.user_id = e.user_id)",
    )
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// At most one DM a day while the queue holds entries backed by 2 or more owners.
async fn digest(app: &App) -> Result<()> {
    static LAST: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());
    let Some(owner) = app.bot_owner else {
        return Ok(());
    };
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM raiders r WHERE status = 'pending'
           AND (SELECT count(DISTINCT source_owner) FROM raider_evidence e WHERE e.user_id = r.user_id) >= 2",
    )
    .fetch_one(&app.pool)
    .await?;
    // shortcut: the day is kept in memory, so a restart can send a second digest that day.
    if n == 0 || !reply::alert_due(&mut LAST.lock().unwrap(), 0, now_secs(), 86_400) {
        return Ok(());
    }
    let text = format!("**Raider List:** {n} entries in the review queue have evidence from 2 or more server owners.");
    let rows: Vec<Component> = queue_url(None)
        .map(|u| crate::verify::row(crate::welcome::link("Open the queue", u)))
        .into_iter()
        .collect();
    reply::dm_buttons(app, owner, &text, &rows).await;
    Ok(())
}

/// `raider check @user` (any permit) and `raider report @user <category> <reason>` (owner, extra owner).
pub async fn command(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: Id<UserMarker>,
    level: &Level,
    words: &[&str],
) -> Result<String> {
    let user = words
        .get(1)
        .and_then(|w| w.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok())
        .and_then(Id::new_checked);
    let usage = "Use `raider check @user` or `raider report @user <nuke|raid|scam|spam> <reason>`.";
    match (words.first().copied(), user) {
        (Some("check"), Some(u)) => check_text(app, u).await,
        (Some("report"), Some(u)) => {
            if !matches!(level, Level::Owner | Level::ExtraOwner) {
                return Ok(
                    "Only the owner or an Extra Owner can report to the Raider List.".into(),
                );
            }
            let (Some(cat), reason) = (
                words.get(2).and_then(|c| parse_cat(c)),
                words[3.min(words.len())..].join(" "),
            ) else {
                return Ok(usage.into());
            };
            if reason.trim().is_empty() {
                return Ok(usage.into());
            }
            report(app, gid, invoker, u, cat, &reason).await
        }
        _ => Ok(usage.into()),
    }
}

/// Listed or not, the category and the public note; never the evidence or where it came from.
async fn check_text(app: &App, user: Id<UserMarker>) -> Result<String> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT category, public_note FROM raiders WHERE user_id = $1 AND status = 'confirmed'
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(user.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    Ok(match row {
        Some((cat, note)) if note.is_empty() => {
            format!("<@{user}> is on the Raider List: **{cat}**.")
        }
        Some((cat, note)) => format!(
            "<@{user}> is on the Raider List: **{cat}**. Note: {}",
            md(&note)
        ),
        None => format!("<@{user}> is not on the Raider List."),
    })
}

/// The prefix command's side of a report: what only Discord knows, then the shared report.
async fn report(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: Id<UserMarker>,
    user: Id<UserMarker>,
    cat: RaiderCategory,
    reason: &str,
) -> Result<String> {
    let case: Option<(i32, String)> = sqlx::query_as(
        "SELECT case_no, kind FROM cases WHERE guild_id = $1 AND user_id = $2 AND kind = ANY($3)
         ORDER BY case_no DESC LIMIT 1",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(raiders::REPORTABLE.as_slice())
    .fetch_optional(&app.pool)
    .await?;
    let guild = app.http.guild(gid).with_counts(true).await?.model().await?;
    let me = app.http.guild_member(gid, app.me).await?.model().await?;
    let days = me
        .joined_at
        .map_or(0, |t| (now_secs() - t.as_secs()) / 86_400);
    let roles = member_roles(app, gid, user).await?;
    let r = raiders::Report {
        guild: gid.get() as i64,
        owner: guild.owner_id.get() as i64,
        user: user.get() as i64,
        reporter: invoker.get() as i64,
        case: case.as_ref().map(|(n, k)| (*n, k.as_str())),
        category: cat_name(cat),
        reason: Some(reason),
        members: guild.approximate_member_count.unwrap_or(0),
        days_with_bot: days,
        roles: &roles,
        protected: [
            app.me.get() as i64,
            app.bot_owner.map_or(0, |b| b.get() as i64),
        ],
    };
    Ok(match raiders::report(&app.pool, &r).await? {
        Ok(()) => format!("Reported <@{user}> to the Raider List as **{}**. The bot owner reviews every report; a report never lists anyone by itself.", cat_name(cat)),
        Err(no) => no,
    })
}

#[cfg(test)]
#[path = "raiders_tests.rs"]
mod tests;
