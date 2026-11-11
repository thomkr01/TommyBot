//! Sticky roles: a member who leaves and rejoins gets their safe roles back. Each row belongs to one
//! membership (`joined_at`), because member events run in their own tasks and arrive in any order:
//! an update from another membership never overwrites the roles a rejoin is about to read.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::{
    types::chrono::{DateTime, Utc},
    PgPool,
};
use std::{collections::HashMap, time::Instant};
use tommybot_core::{jobs, permits};
use tommybot_logging::{
    embed::{DONE, ORANGE},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    gateway::payload::incoming::{MemberAdd, MemberUpdate},
    id::{
        marker::{GuildMarker, RoleMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

use crate::{
    app::{now_secs, App},
    events::send_log,
    hierarchy::GuildInfo,
    http_err,
};

/// Job that gives back the roles past the per-guild cap.
pub(crate) const KIND: &str = "sticky_restore";
const REASON: &str = "Sticky roles: rejoined";
/// Role adds per guild per minute. Role adds share the member rate-limit bucket with timeouts and
/// bans, so a mass rejoin must not hold back punishments.
const PER_MIN: u32 = 20;
const KEEP_DAYS: i32 = 30;
const SYNC_SECS: u64 = 86_400;

fn at(t: Timestamp) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros(t.as_micros())
}

/// Sorted, so the no-op check does not depend on the order Discord lists roles in.
fn ids(roles: &[Id<RoleMarker>]) -> Vec<i64> {
    let mut v: Vec<i64> = roles.iter().map(|r| r.get() as i64).collect();
    v.sort_unstable();
    v
}

/// Saves the member's roles for this membership only; a held (pending) list is left alone.
pub async fn on_member_update(app: &App, m: &MemberUpdate) -> Result<()> {
    let Some(joined) = m.joined_at.and_then(at) else {
        return Ok(());
    };
    if m.user.bot || !app.cfg(m.guild_id.get()).await?.misc.sticky_roles {
        return Ok(());
    }
    save(
        &app.pool,
        m.guild_id.get() as i64,
        m.user.id.get() as i64,
        &ids(&m.roles),
        joined,
    )
    .await
}

async fn save(pool: &PgPool, g: i64, u: i64, roles: &[i64], joined: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "INSERT INTO member_roles (guild_id, user_id, roles, joined_at) VALUES ($1, $2, $3, $4)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET roles = EXCLUDED.roles, updated_at = now()
         WHERE member_roles.joined_at = EXCLUDED.joined_at AND NOT member_roles.pending
           AND member_roles.roles IS DISTINCT FROM EXCLUDED.roles",
    )
    .bind(g)
    .bind(u)
    .bind(roles)
    .bind(joined)
    .execute(pool)
    .await?;
    Ok(())
}

/// The roles stay; retention counts from now.
pub async fn on_leave(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<()> {
    left(&app.pool, gid.get() as i64, user.get() as i64).await
}

async fn left(pool: &PgPool, g: i64, u: i64) -> Result<()> {
    sqlx::query(
        "UPDATE member_roles SET left_at = now() WHERE guild_id = $1 AND user_id = $2 AND left_at IS NULL",
    )
    .bind(g)
    .bind(u)
    .execute(pool)
    .await?;
    Ok(())
}

/// A ban forgets the roles, so an unban and rejoin starts clean. A kick keeps them. The row stays
/// as an empty tombstone of a membership newer than the old one, so a late update or sync page of
/// the old membership cannot write the old roles back.
pub async fn on_ban(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<()> {
    let (g, u) = (gid.get() as i64, user.get() as i64);
    sqlx::query(
        "DELETE FROM scheduled_jobs WHERE guild_id = $1 AND kind = $2 AND payload->>'user' = $3",
    )
    .bind(g)
    .bind(KIND)
    .bind(u.to_string())
    .execute(&app.pool)
    .await?;
    if !app.cfg(gid.get()).await?.misc.sticky_roles {
        return Ok(());
    }
    forget(&app.pool, g, u).await
}

async fn forget(pool: &PgPool, g: i64, u: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO member_roles (guild_id, user_id, roles, joined_at, left_at)
         VALUES ($1, $2, '{}', now(), now())
         ON CONFLICT (guild_id, user_id) DO UPDATE SET roles = '{}', joined_at = now(),
           left_at = now(), pending = false, updated_at = now()",
    )
    .bind(g)
    .bind(u)
    .execute(pool)
    .await?;
    Ok(())
}

/// Moves the row to this membership in one statement and returns the previous membership's roles,
/// so a replayed `MemberAdd` gets nothing. `pending` keeps the list in the row for verification.
async fn claim(
    pool: &PgPool,
    g: i64,
    u: i64,
    joined: DateTime<Utc>,
    pending: bool,
) -> Result<Option<Vec<i64>>> {
    // `FROM old` reads the roles before the update; a subquery in RETURNING would run after it
    // and see nothing. A second claim waits on the lock, then finds the new joined_at.
    let old: Option<Vec<i64>> = sqlx::query_scalar(
        "WITH old AS (SELECT roles FROM member_roles WHERE guild_id = $1 AND user_id = $2
                      AND joined_at < $3 FOR UPDATE)
         UPDATE member_roles m SET roles = CASE WHEN $4 THEN m.roles ELSE '{}' END, joined_at = $3,
                left_at = NULL, pending = $4, updated_at = now()
         FROM old WHERE m.guild_id = $1 AND m.user_id = $2 AND m.joined_at < $3
         RETURNING old.roles",
    )
    .bind(g)
    .bind(u)
    .bind(joined)
    .bind(pending)
    .fetch_optional(pool)
    .await?;
    if old.is_none() {
        sqlx::query(
            "INSERT INTO member_roles (guild_id, user_id, roles, joined_at) VALUES ($1, $2, '{}', $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(g)
        .bind(u)
        .bind(joined)
        .execute(pool)
        .await?;
    }
    Ok(old)
}

/// Last step of `joingate::on_join` for a joiner still in the server. `held`: an earlier step
/// (quarantine, join raid) holds them back, so the row moves to this membership and the old list
/// is dropped (a quarantine keeps its own saved roles). `verifying`: the roles are held until
/// [`restore_pending`].
pub async fn on_join(app: &App, m: &MemberAdd, held: bool, verifying: bool) -> Result<()> {
    let (gid, user) = (m.guild_id, m.member.user.id);
    let Some(joined) = m.member.joined_at.and_then(at) else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    if m.member.user.bot || !cfg.misc.sticky_roles {
        return Ok(());
    }
    let (g, u) = (gid.get() as i64, user.get() as i64);
    if held {
        return claim(&app.pool, g, u, joined, false).await.map(drop);
    }
    // Read before the claim: a failure after it would lose the claimed roles.
    let info = app.guild_info(gid).await?;
    let permit = permits::permit_roles(&app.pool, g).await?;
    let saved = claim(&app.pool, g, u, joined, verifying).await?;
    let Some(saved) = saved.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let t = Target {
        user,
        joined: joined.timestamp_micros(),
        tries: 0,
    };
    if verifying {
        // Verified before the claim landed: `verify::grant` found nothing pending, so restore now.
        let now = http_err::member(&app.http, gid, user).await?;
        let verified =
            now.is_some_and(|n| n.roles.iter().any(|r| cfg.verification.roles.contains(r)));
        return if verified {
            restore_pending(app, gid, user).await
        } else {
            Ok(())
        };
    }
    let timeout = m.member.communication_disabled_until.map(|t| t.as_secs());
    restore_or_queue(app, gid, &t, &saved, timeout, (&info, &permit)).await
}

/// Called by `verify::grant` after it set the verification roles, so they cannot wipe these.
pub async fn restore_pending(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<()> {
    if !app.cfg(gid.get()).await?.misc.sticky_roles {
        return Ok(());
    }
    let info = app.guild_info(gid).await?;
    let permit = permits::permit_roles(&app.pool, gid.get() as i64).await?;
    let held: Option<(Vec<i64>, DateTime<Utc>)> = sqlx::query_as(
        "UPDATE member_roles SET pending = false, updated_at = now()
         WHERE guild_id = $1 AND user_id = $2 AND pending RETURNING roles, joined_at",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    match held {
        Some((roles, joined)) if !roles.is_empty() => {
            let t = Target {
                user,
                joined: joined.timestamp_micros(),
                tries: 0,
            };
            // A member cannot press Verify while timed out.
            restore_or_queue(app, gid, &t, &roles, None, (&info, &permit)).await
        }
        _ => Ok(()),
    }
}

/// Whose roles go back: the member, the membership (`joined_at` in µs) the roles were claimed
/// for, and how many job runs already had a failed add.
#[derive(Clone, Copy)]
struct Target {
    user: Id<UserMarker>,
    joined: i64,
    tries: u64,
}

/// A failed role add is retried this many times (one job a minute), then dropped.
const MAX_TRIES: u64 = 3;

fn job(t: &Target, roles: &[u64]) -> Value {
    json!({"user": t.user.get(), "roles": roles, "joined_at": t.joined, "tries": t.tries})
}

/// The roles are already claimed: a failed restore goes into a job instead of being lost.
async fn restore_or_queue(
    app: &App,
    gid: Id<GuildMarker>,
    t: &Target,
    saved: &[i64],
    timeout_until: Option<i64>,
    ctx: (&GuildInfo, &[i64]),
) -> Result<()> {
    let Err(e) = restore(app, gid, t, saved, timeout_until, ctx).await else {
        return Ok(());
    };
    tracing::warn!(?e, "giving back sticky roles failed, retrying in a minute");
    let roles: Vec<u64> = saved.iter().map(|&r| r as u64).collect();
    jobs::enqueue(
        &app.pool,
        gid.get() as i64,
        KIND,
        job(t, &roles),
        now_secs() + 60,
    )
    .await
    .map(drop)
}

/// The `sticky_restore` job: roles past the cap or a failed add. Membership is checked again
/// here, at add time: a member who left and came back, was quarantined, was banned (the row is
/// a tombstone then), or must verify again gets nothing from this job.
pub async fn run_job(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(p["user"].as_u64().context("job payload has no user")?);
    let joined = p["joined_at"]
        .as_i64()
        .context("job payload has no joined_at")?;
    if !app.cfg(gid.get()).await?.misc.sticky_roles {
        return Ok(());
    }
    let Some(member) = http_err::member(&app.http, gid, user).await? else {
        return Ok(());
    };
    let (g, u) = (gid.get() as i64, user.get() as i64);
    let same = member.joined_at.map(|t| t.as_micros()) == Some(joined);
    if !same || !due(&app.pool, g, u, joined).await? {
        return Ok(());
    }
    let roles: Vec<i64> = p["roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect();
    let t = Target {
        user,
        joined,
        tries: p["tries"].as_u64().unwrap_or(0),
    };
    let info = app.guild_info(gid).await?;
    let permit = permits::permit_roles(&app.pool, g).await?;
    let timeout = member.communication_disabled_until.map(|t| t.as_secs());
    // An error retries this job through the scheduler's attempt limit.
    restore(app, gid, &t, &roles, timeout, (&info, &permit)).await
}

/// The row still belongs to the membership `joined` (µs), is not held for verification, and the
/// member is not quarantined.
async fn due(pool: &PgPool, g: i64, u: i64, joined: i64) -> Result<bool> {
    let joined = DateTime::from_timestamp_micros(joined).context("bad joined_at")?;
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM member_roles WHERE guild_id = $1 AND user_id = $2
                          AND joined_at = $3 AND left_at IS NULL AND NOT pending)
            AND NOT EXISTS (SELECT 1 FROM quarantines WHERE guild_id = $1 AND user_id = $2)",
    )
    .bind(g)
    .bind(u)
    .bind(joined)
    .fetch_one(pool)
    .await?)
}

/// Gives back the safe part of `saved`, checked against the roles as they are now.
async fn restore(
    app: &App,
    gid: Id<GuildMarker>,
    t: &Target,
    saved: &[i64],
    timeout_until: Option<i64>,
    (info, permit): (&GuildInfo, &[i64]),
) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let saved: Vec<Id<RoleMarker>> = saved
        .iter()
        .filter_map(|&r| Id::new_checked(r as u64))
        .collect();
    let (mut give, skipped) = restorable(
        gid,
        &saved,
        info,
        permit,
        cfg.antinuke.quarantine_role,
        &cfg.misc.sticky_never,
    );
    // Added now, `timeout_roles` would take them away again a moment later.
    let main = &cfg.lockdown.main_roles;
    if let Some(until) = timeout_until
        .filter(|&t| t > now_secs())
        .filter(|_| cfg.misc.remove_main_roles_on_timeout)
    {
        let held: Vec<u64> = give
            .iter()
            .filter(|r| main.contains(r))
            .map(|r| r.get())
            .collect();
        if !held.is_empty() {
            // joined_at: `timeout_roles::restore` skips it once this membership is over.
            let job = json!({"user": t.user.get(), "roles": held, "joined_at": t.joined});
            jobs::enqueue(
                &app.pool,
                gid.get() as i64,
                crate::timeout_roles::KIND,
                job,
                until,
            )
            .await?;
            give.retain(|r| !main.contains(r));
        }
    }
    let given = add(app, gid, t, &give).await?;
    if let Some(log) = rejoin_log(t.user.get(), &given, &skipped) {
        if let Err(e) = send_log(app, gid.get(), LogKind::Member, log).await {
            tracing::warn!(?e, "sticky roles log failed");
        }
    }
    if given.is_empty() {
        return Ok(());
    }
    // Quarantined while the roles went on (anti-nuke or staff): put the quarantine back.
    crate::antinuke::requarantine_rejoin(app, gid, t.user)
        .await
        .map(drop)
}

/// One Member log per restore: the roles given back and the ones skipped, with the reason.
fn rejoin_log(user: u64, given: &[Id<RoleMarker>], skipped: &Skipped) -> Option<Log> {
    if given.is_empty() && skipped.is_empty() {
        return None;
    }
    let mut log =
        Log::new("Sticky Roles", if given.is_empty() { ORANGE } else { DONE }).user("Member", user);
    if !given.is_empty() {
        let list: Vec<String> = given.iter().map(|r| format!("<@&{r}>")).collect();
        let s = if given.len() == 1 { "" } else { "s" };
        log = log.line(
            &format!("Gave back {} role{s}", given.len()),
            &list.join(", "),
        );
    }
    if !skipped.is_empty() {
        let list: Vec<String> = skipped
            .iter()
            .map(|(r, why)| format!("<@&{r}> ({why})"))
            .collect();
        log = log.line("Not given back", &list.join(", "));
    }
    Some(log)
}

/// Adds the roles one by one (a full role list would drop what another bot gave meanwhile), as
/// far as the guild's cap allows. The rest, and adds that failed for another reason than 404,
/// wait a minute as a job, since the row was already claimed. Returns the roles given.
async fn add(
    app: &App,
    gid: Id<GuildMarker>,
    t: &Target,
    roles: &[Id<RoleMarker>],
) -> Result<Vec<Id<RoleMarker>>> {
    let n = take(
        &mut app.sticky_budget.lock().unwrap(),
        gid.get(),
        roles.len(),
        now_secs(),
    );
    let (now, later) = roles.split_at(n);
    let mut retry: Vec<u64> = later.iter().map(|r| r.get()).collect();
    let (mut given, mut failed) = (vec![], false);
    for &r in now {
        match app
            .http
            .add_guild_member_role(gid, t.user, r)
            .reason(REASON)
            .await
        {
            Ok(_) => given.push(r),
            // The member left again or the role is gone.
            Err(e) if http_err::gone(&e) => {}
            Err(e) => {
                tracing::warn!(?e, role = r.get(), "giving back a sticky role failed");
                if t.tries < MAX_TRIES {
                    failed = true;
                    retry.push(r.get());
                }
            }
        }
    }
    if !retry.is_empty() {
        let next = Target {
            tries: t.tries + u64::from(failed),
            ..*t
        };
        let job = job(&next, &retry);
        jobs::enqueue(&app.pool, gid.get() as i64, KIND, job, now_secs() + 60).await?;
    }
    Ok(given)
}

/// How many of `want` role adds the guild may make this minute; takes them from its budget.
fn take(budget: &mut HashMap<u64, (i64, u32)>, guild: u64, want: usize, now: i64) -> usize {
    let (minute, used) = budget.entry(guild).or_insert((now / 60, 0));
    if *minute != now / 60 {
        (*minute, *used) = (now / 60, 0);
    }
    let n = (PER_MIN - *used).min(want as u32);
    *used += n;
    n as usize
}

type Skipped = Vec<(Id<RoleMarker>, &'static str)>;

/// Splits saved roles into the ones it is safe to give back and the skipped ones with the reason.
/// Deleted roles and @everyone are dropped without a word.
pub(crate) fn restorable(
    gid: Id<GuildMarker>,
    saved: &[Id<RoleMarker>],
    info: &GuildInfo,
    permit: &[i64],
    quarantine: Option<Id<RoleMarker>>,
    never: &[Id<RoleMarker>],
) -> (Vec<Id<RoleMarker>>, Skipped) {
    let (mut give, mut skipped) = (vec![], vec![]);
    for &r in saved {
        let Some(&pos) = info.roles.get(&r).filter(|_| r.get() != gid.get()) else {
            continue;
        };
        let why = if info.managed.contains(&r) {
            "managed"
        } else if pos >= info.bot_top {
            "not below the bot's role"
        } else if info
            .role_perms
            .get(&r)
            .is_some_and(|p| p & tommybot_antinuke::DANGEROUS_PERMS != 0)
        {
            "dangerous permissions"
        } else if permit.contains(&(r.get() as i64)) {
            "permit role"
        } else if Some(r) == quarantine {
            "quarantine role"
        } else if never.contains(&r) {
            "never given back"
        } else {
            give.push(r);
            continue;
        };
        skipped.push((r, why));
    }
    (give, skipped)
}

/// Hourly from `welcome::purge_left`: syncs each guild with sticky roles on once a day (`synced`
/// is in memory, so a restart syncs once more), then deletes rows past retention and rows of
/// guilds with the setting off.
pub async fn sweep(app: &App, synced: &mut HashMap<u64, Instant>) -> Result<()> {
    let rows: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT guild_id, guild_id IN (SELECT guild_id FROM guild_left) FROM
           (SELECT DISTINCT ON (guild_id) guild_id, config FROM guild_config
            ORDER BY guild_id, version DESC) c
         WHERE c.config->'misc'->>'sticky_roles' = 'true'",
    )
    .fetch_all(&app.pool)
    .await?;
    let mut on = vec![];
    for (g, gone) in rows {
        // The bot reads the section with serde, so a broken misc section counts as off.
        if !app.cfg(g as u64).await?.misc.sticky_roles {
            continue;
        }
        on.push(g);
        let due = synced
            .get(&(g as u64))
            .is_none_or(|t| t.elapsed().as_secs() >= SYNC_SECS);
        if gone || !due {
            continue;
        }
        synced.insert(g as u64, Instant::now());
        if let Err(e) = sync(app, Id::new(g as u64)).await {
            tracing::warn!(?e, guild = g, "sticky roles sync failed");
        }
    }
    prune(&app.pool, &on).await
}

async fn prune(pool: &PgPool, on: &[i64]) -> Result<()> {
    sqlx::query(
        "DELETE FROM member_roles WHERE guild_id <> ALL($1)
            OR left_at < now() - make_interval(days => $2)",
    )
    .bind(on)
    .bind(KEEP_DAYS)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stores every member's roles, then marks the rows of everyone not listed as left. Marking runs
/// only after a complete paging: a cut-short list would let retention delete roles of members.
async fn sync(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let g = gid.get() as i64;
    let start: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&app.pool)
        .await?;
    let mut seen = vec![];
    let mut after = None;
    loop {
        let req = app.http.guild_members(gid).limit(1000);
        let req = match after {
            Some(a) => req.after(a),
            None => req,
        };
        let page = req.await?.models().await?;
        let (mut users, mut roles, mut joined) = (vec![], vec![], vec![]);
        for m in page.iter().filter(|m| !m.user.bot) {
            if let Some(j) = m.joined_at.and_then(at) {
                users.push(m.user.id.get() as i64);
                let list: Vec<String> = ids(&m.roles).iter().map(i64::to_string).collect();
                roles.push(format!("{{{}}}", list.join(",")));
                joined.push(j);
            }
        }
        upsert(&app.pool, g, &users, &roles, &joined).await?;
        seen.extend(page.iter().map(|m| m.user.id.get() as i64));
        match page.last() {
            Some(m) if page.len() == 1000 => after = Some(m.user.id),
            _ => break,
        }
    }
    mark_left(&app.pool, g, &seen, start).await
}

/// `roles` are Postgres array literals, one per member, since UNNEST cannot take an array of arrays.
/// Roles change only for the same membership and when not held for verification. A row of an
/// older membership is taken over when the listed join is over 10 minutes old (a `MemberAdd`
/// missed while the bot was offline; a younger join may still have its claim on the way). A
/// listing older than the row (a ban tombstone written during the paging) changes nothing.
async fn upsert(
    pool: &PgPool,
    g: i64,
    users: &[i64],
    roles: &[String],
    joined: &[DateTime<Utc>],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO member_roles AS m (guild_id, user_id, roles, joined_at)
         SELECT $1, u, r::bigint[], j FROM unnest($2::bigint[], $3::text[], $4::timestamptz[]) AS t(u, r, j)
         ON CONFLICT (guild_id, user_id) DO UPDATE SET
           roles = CASE WHEN (m.joined_at = EXCLUDED.joined_at AND NOT m.pending)
                          OR (m.joined_at < EXCLUDED.joined_at
                              AND EXCLUDED.joined_at < now() - interval '10 minutes')
                        THEN EXCLUDED.roles ELSE m.roles END,
           pending = m.pending AND NOT (m.joined_at < EXCLUDED.joined_at
                                        AND EXCLUDED.joined_at < now() - interval '10 minutes'),
           joined_at = CASE WHEN EXCLUDED.joined_at < now() - interval '10 minutes'
                            THEN GREATEST(m.joined_at, EXCLUDED.joined_at) ELSE m.joined_at END,
           left_at = CASE WHEN m.joined_at <= EXCLUDED.joined_at THEN NULL ELSE m.left_at END,
           updated_at = now()",
    )
    .bind(g)
    .bind(users)
    .bind(roles)
    .bind(joined)
    .execute(pool)
    .await?;
    Ok(())
}

/// Rows touched since `start` (a join during the paging) are left alone.
async fn mark_left(pool: &PgPool, g: i64, seen: &[i64], start: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "UPDATE member_roles SET left_at = now() WHERE guild_id = $1 AND left_at IS NULL
           AND user_id <> ALL($2) AND updated_at < $3",
    )
    .bind(g)
    .bind(seen)
    .bind(start)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn id<T>(n: u64) -> Id<T> {
        Id::new(n)
    }

    /// Guild 1 (@everyone). Every other role sits at position 1, as on a server never reordered:
    /// the bot's role 10 ranks above the newer ids and below the older one (5).
    fn info() -> GuildInfo {
        let raw = [
            (1, 0),
            (5, 1),
            (10, 1),
            (20, 1),
            (21, 1),
            (22, 1),
            (23, 1),
            (24, 1),
            (25, 1),
        ];
        let rank = tommybot_core::roles::ranks(raw, 1);
        let roles: HashMap<Id<RoleMarker>, i64> = rank.iter().map(|(&r, &p)| (id(r), p)).collect();
        GuildInfo {
            owner: id(1),
            bot_top: roles[&id(10)],
            role_perms: HashMap::from([(id(21), tommybot_antinuke::DANGEROUS_PERMS & (1 << 2))]),
            managed: HashSet::from([id(22)]),
            parents: HashMap::new(),
            channels: HashSet::new(),
            roles,
            fetched: Instant::now(),
        }
    }

    #[test]
    fn restorable_applies_every_rule() {
        let saved: Vec<Id<RoleMarker>> = [1, 99, 5, 10, 20, 21, 22, 23, 24, 25].map(id).to_vec();
        let (give, skipped) = restorable(id(1), &saved, &info(), &[23], Some(id(24)), &[id(25)]);
        assert_eq!(give, vec![id(20)]);
        assert_eq!(
            skipped,
            vec![
                (id(5), "not below the bot's role"), // older id at the same position ranks higher
                (id(10), "not below the bot's role"), // the bot's own top role
                (id(21), "dangerous permissions"),
                (id(22), "managed"),
                (id(23), "permit role"),
                (id(24), "quarantine role"),
                (id(25), "never given back"),
            ]
        );
    }

    #[test]
    fn budget_caps_each_minute() {
        let mut b = HashMap::new();
        assert_eq!(take(&mut b, 1, 15, 60), 15);
        assert_eq!(take(&mut b, 1, 15, 61), 5);
        assert_eq!(take(&mut b, 1, 1, 62), 0);
        assert_eq!(take(&mut b, 2, 3, 62), 3); // other guild unaffected
        assert_eq!(take(&mut b, 1, 3, 120), 3); // next minute
    }

    #[test]
    fn rejoin_posts_one_log_with_both_lines() {
        let skipped = vec![(id(21), "dangerous permissions")];
        let text = |l: Log| serde_json::to_string(&l.build()).unwrap();
        let both = text(rejoin_log(9, &[id(20)], &skipped).unwrap());
        for part in [
            "Gave back 1 role",
            "<@&20>",
            "Not given back",
            "<@&21> (dangerous",
        ] {
            assert!(both.contains(part), "{part}: {both}");
        }
        assert!(!text(rejoin_log(9, &[], &skipped).unwrap()).contains("Gave back"));
        assert!(rejoin_log(9, &[], &vec![]).is_none());
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    async fn pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        Some(tommybot_core::db::connect(&url).await.unwrap())
    }

    fn guild(offset: i64) -> i64 {
        3_500_000_000 + std::process::id() as i64 * 10 + offset
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 123_456_000).unwrap()
    }

    async fn row(p: &PgPool, g: i64, u: i64) -> Option<(Vec<i64>, bool, bool)> {
        sqlx::query_as(
            "SELECT roles, left_at IS NOT NULL, pending FROM member_roles WHERE guild_id = $1 AND user_id = $2",
        )
        .bind(g)
        .bind(u)
        .fetch_optional(p)
        .await
        .unwrap()
    }

    async fn clean(p: &PgPool, g: i64) {
        sqlx::query("DELETE FROM member_roles WHERE guild_id = $1")
            .bind(g)
            .execute(p)
            .await
            .unwrap();
    }

    /// One test, so `prune` (which works on every guild) never races another test's rows.
    #[tokio::test]
    async fn member_roles_sql() {
        let Some(p) = pool().await else { return };
        claim_runs_once_and_old_updates_do_not_overwrite(&p).await;
        ban_empties_and_retention_keeps_members(&p).await;
        sync_upserts_and_marks_only_unlisted(&p).await;
        held_join_drops_the_old_list(&p).await;
        ban_tombstone_blocks_old_membership(&p).await;
        sync_takes_over_a_missed_join(&p).await;
        jobs_run_only_for_the_claimed_membership(&p).await;
    }

    async fn joined_at(p: &PgPool, g: i64, u: i64) -> DateTime<Utc> {
        sqlx::query_scalar(
            "SELECT joined_at FROM member_roles WHERE guild_id = $1 AND user_id = $2",
        )
        .bind(g)
        .bind(u)
        .fetch_one(p)
        .await
        .unwrap()
    }

    /// A quarantined or raid-flagged rejoin moves the row on; a later rejoin gets nothing from two
    /// memberships ago.
    async fn held_join_drops_the_old_list(p: &PgPool) {
        let g = guild(4);
        clean(p, g).await;
        save(p, g, 1, &[5], t(100)).await.unwrap();
        claim(p, g, 1, t(200), false).await.unwrap(); // held: list dropped
        save(p, g, 1, &[6], t(200)).await.unwrap(); // the new membership saves again
        assert_eq!(row(p, g, 1).await, Some((vec![6], false, false)));
        assert_eq!(claim(p, g, 1, t(300), false).await.unwrap(), Some(vec![6]));
        clean(p, g).await;
    }

    async fn ban_tombstone_blocks_old_membership(p: &PgPool) {
        let g = guild(5);
        clean(p, g).await;
        save(p, g, 1, &[5], t(100)).await.unwrap();
        forget(p, g, 1).await.unwrap();
        assert_eq!(row(p, g, 1).await, Some((vec![], true, false)));
        // A late update and a sync page of the banned membership write nothing back.
        save(p, g, 1, &[5], t(100)).await.unwrap();
        upsert(p, g, &[1], &["{5}".into()], &[t(100)])
            .await
            .unwrap();
        assert_eq!(row(p, g, 1).await, Some((vec![], true, false)));
        // Banned with no row yet: the tombstone still blocks the old membership.
        forget(p, g, 2).await.unwrap();
        save(p, g, 2, &[5], t(100)).await.unwrap();
        assert_eq!(row(p, g, 2).await, Some((vec![], true, false)));
        // Unbanned and rejoined: nothing to give back.
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT now() + interval '1 second'")
            .fetch_one(p)
            .await
            .unwrap();
        assert_eq!(claim(p, g, 1, now, false).await.unwrap(), Some(vec![]));
        clean(p, g).await;
    }

    async fn sync_takes_over_a_missed_join(p: &PgPool) {
        let g = guild(6);
        clean(p, g).await;
        save(p, g, 1, &[5], t(100)).await.unwrap(); // Muted, then left and rejoined offline
        save(p, g, 2, &[5], t(100)).await.unwrap(); // rejoined a minute ago, claim on the way
        claim(p, g, 3, t(100), true).await.unwrap();
        sqlx::query("UPDATE member_roles SET pending = true WHERE guild_id = $1 AND user_id = 3")
            .bind(g)
            .execute(p)
            .await
            .unwrap();
        let recent: DateTime<Utc> = sqlx::query_scalar("SELECT now() - interval '1 minute'")
            .fetch_one(p)
            .await
            .unwrap();
        let lists = ["{7}".to_string(), "{8}".into(), "{9}".into()];
        upsert(p, g, &[1, 2, 3], &lists, &[t(500), recent, t(500)])
            .await
            .unwrap();
        assert_eq!(row(p, g, 1).await, Some((vec![7], false, false)));
        assert_eq!(joined_at(p, g, 1).await, t(500));
        assert_eq!(row(p, g, 2).await, Some((vec![5], false, false)));
        assert_eq!(joined_at(p, g, 2).await, t(100));
        assert_eq!(row(p, g, 3).await, Some((vec![9], false, false)));
        clean(p, g).await;
    }

    async fn jobs_run_only_for_the_claimed_membership(p: &PgPool) {
        let g = guild(7);
        clean(p, g).await;
        let us = |s: i64| t(s).timestamp_micros();
        claim(p, g, 1, t(100), false).await.unwrap();
        assert!(due(p, g, 1, us(100)).await.unwrap());
        assert!(!due(p, g, 1, us(99)).await.unwrap()); // another membership
        assert!(!due(p, g, 2, us(100)).await.unwrap()); // no row
        sqlx::query("UPDATE member_roles SET pending = true WHERE guild_id = $1")
            .bind(g)
            .execute(p)
            .await
            .unwrap();
        assert!(!due(p, g, 1, us(100)).await.unwrap()); // must verify again
        sqlx::query("UPDATE member_roles SET pending = false WHERE guild_id = $1")
            .bind(g)
            .execute(p)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, 1, '{}')",
        )
        .bind(g)
        .execute(p)
        .await
        .unwrap();
        assert!(!due(p, g, 1, us(100)).await.unwrap()); // quarantined
        sqlx::query("DELETE FROM quarantines WHERE guild_id = $1")
            .bind(g)
            .execute(p)
            .await
            .unwrap();
        forget(p, g, 1).await.unwrap();
        assert!(!due(p, g, 1, us(100)).await.unwrap()); // banned
        clean(p, g).await;
    }

    async fn claim_runs_once_and_old_updates_do_not_overwrite(p: &PgPool) {
        let g = guild(0);
        clean(p, g).await;
        save(p, g, 7, &[3, 4], t(100)).await.unwrap();
        save(p, g, 7, &[3, 4], t(100)).await.unwrap(); // no-op
        let (a, b) = tokio::join!(claim(p, g, 7, t(200), false), claim(p, g, 7, t(200), false));
        let got: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
        assert_eq!(got, vec![vec![3, 4]]);
        // A late update from the old membership leaves the new one alone; the new one writes.
        save(p, g, 7, &[3, 4, 5], t(100)).await.unwrap();
        assert_eq!(row(p, g, 7).await, Some((vec![], false, false)));
        save(p, g, 7, &[9], t(200)).await.unwrap();
        assert_eq!(row(p, g, 7).await, Some((vec![9], false, false)));
        // No row yet: the claim inserts one and gives nothing back.
        assert_eq!(claim(p, g, 8, t(300), false).await.unwrap(), None);
        assert_eq!(row(p, g, 8).await, Some((vec![], false, false)));
        // Held for verification: the list stays and updates do not replace it.
        assert_eq!(claim(p, g, 7, t(400), true).await.unwrap(), Some(vec![9]));
        save(p, g, 7, &[1], t(400)).await.unwrap();
        assert_eq!(row(p, g, 7).await, Some((vec![9], false, true)));
        clean(p, g).await;
    }

    async fn ban_empties_and_retention_keeps_members(p: &PgPool) {
        let (g, off) = (guild(1), guild(2));
        clean(p, g).await;
        clean(p, off).await;
        for u in [1, 2, 3] {
            save(p, g, u, &[5], t(100)).await.unwrap();
        }
        save(p, off, 1, &[5], t(100)).await.unwrap();
        forget(p, g, 1).await.unwrap();
        assert_eq!(row(p, g, 1).await, Some((vec![], true, false)));
        // 2 left 31 days ago, 3 is still a member.
        sqlx::query("UPDATE member_roles SET left_at = now() - interval '31 days' WHERE guild_id = $1 AND user_id = 2")
            .bind(g)
            .execute(p)
            .await
            .unwrap();
        // Only this test's guild counts as on, so other tests' rows are not deleted.
        let others: Vec<i64> =
            sqlx::query_scalar("SELECT DISTINCT guild_id FROM member_roles WHERE guild_id <> $1")
                .bind(off)
                .fetch_all(p)
                .await
                .unwrap();
        prune(p, &others).await.unwrap();
        assert_eq!(row(p, g, 2).await, None);
        assert_eq!(row(p, g, 3).await, Some((vec![5], false, false)));
        assert_eq!(row(p, off, 1).await, None); // setting off
        clean(p, g).await;
    }

    async fn sync_upserts_and_marks_only_unlisted(p: &PgPool) {
        let g = guild(3);
        clean(p, g).await;
        save(p, g, 1, &[5], t(100)).await.unwrap();
        save(p, g, 2, &[5], t(100)).await.unwrap();
        left(p, g, 1).await.unwrap();
        let start: DateTime<Utc> = sqlx::query_scalar("SELECT now() + interval '1 second'")
            .fetch_one(p)
            .await
            .unwrap();
        // 1 rejoined (marked left by a late leave), 3 is new; 2 is not listed.
        upsert(
            p,
            g,
            &[1, 3],
            &["{6,7}".into(), "{8}".into()],
            &[t(100), t(300)],
        )
        .await
        .unwrap();
        mark_left(p, g, &[1, 3], start).await.unwrap();
        assert_eq!(row(p, g, 1).await, Some((vec![6, 7], false, false)));
        assert_eq!(row(p, g, 2).await, Some((vec![5], true, false)));
        assert_eq!(row(p, g, 3).await, Some((vec![8], false, false)));
        clean(p, g).await;
    }
}
