//! Auto slowmode: a flooding channel gets a slowmode, and its old one comes back once it is calm.
//! Each slowed channel has an `auto_slowmode` row, written with its `slowmode_off` job in one
//! transaction, so a restart still reverts it.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tommybot_automod::{
    heat::{AutoSlowmodeCfg, Breakdown, HeatCfg},
    lockdown::LockKind,
};
use tommybot_core::{jobs, sections::AutomodCfg};
use tommybot_logging::{
    embed::{channel, DONE, ORANGE, RED},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    channel::Message,
    guild::Permissions,
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    automod::now_ms,
    events::send_log,
    http_err,
};

const KIND: &str = "slowmode_off";
const REASON: &str = "TommyBot auto slowmode";

/// Fills the in-memory list of slowed channels at startup.
pub async fn restore(app: &App) -> Result<()> {
    let rows: Vec<i64> = sqlx::query_scalar("SELECT channel_id FROM auto_slowmode")
        .fetch_all(&app.pool)
        .await?;
    app.slow_active
        .lock()
        .unwrap()
        .extend(rows.into_iter().map(|c| c as u64));
    Ok(())
}

/// Adds a message to its channel's pool. Some(cfg) when the pool tripped: the caller runs `start`
/// after the message's own punishment, so a slowmode never delays it. Only what a slowmode would
/// slow counts: no bots, webhooks or members Discord lets skip it.
pub async fn feed(
    app: &App,
    am: &AutomodCfg,
    gid: Id<GuildMarker>,
    m: &Message,
    b: &Breakdown,
    heat: &HeatCfg,
    now: i64,
) -> Option<AutoSlowmodeCfg> {
    let slow = &am.auto_slowmode;
    let roles = m.member.as_ref().map_or(&[][..], |mem| &mem.roles);
    if !slow.enabled
        || !am.anti_spam
        || m.author.bot
        || m.webhook_id.is_some()
        || exempt(app, gid, m.author.id, roles).await
    {
        return None;
    }
    let mut map = app.slow.lock().unwrap();
    if map.len() > 50_000 {
        map.retain(|_, c| !c.idle(now));
    }
    map.entry(m.channel_id.get())
        .or_default()
        .on_message(now, m.author.id.get(), b, slow, heat)
        .then(|| slow.clone())
}

async fn exempt(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    roles: &[Id<RoleMarker>],
) -> bool {
    const BITS: u64 = Permissions::ADMINISTRATOR
        .union(Permissions::MANAGE_MESSAGES)
        .union(Permissions::MANAGE_CHANNELS)
        .bits();
    // A failed lookup counts as exempt: no slowmode is better than a wrong one.
    app.guild_info(gid).await.map_or(true, |info| {
        info.owner == user
            || roles
                .iter()
                .copied()
                .chain([gid.cast()])
                .any(|r| info.role_perms.get(&r).is_some_and(|p| p & BITS != 0))
    })
}

/// A slowmode only goes up, never down.
fn raises(current: u16, set: u16) -> bool {
    current < set
}

/// The value to put back: only when the slowmode is still the one we set, else staff changed it.
fn revert_to(current: u16, set: i32, prior: i32) -> Option<u16> {
    (i32::from(current) == set)
        .then(|| u16::try_from(prior).ok())
        .flatten()
}

/// What `try_start` did.
#[derive(Debug, PartialEq)]
enum Started {
    On,
    /// Nothing set: locked, already as slow, or staff took over meanwhile.
    Nothing,
    /// The channel already has a live auto slowmode (another task, process, or before a restart):
    /// its off job ends the claim.
    Owned,
}

/// Starts a slowmode after `ChannelHeat::on_message` claimed one. Failures are logged here.
pub async fn start(app: &App, gid: Id<GuildMarker>, ch: Id<ChannelMarker>, cfg: &AutoSlowmodeCfg) {
    let r = try_start(app, gid, ch, cfg).await;
    if matches!(r, Ok(Started::Nothing) | Err(_)) {
        release(app, ch.get());
    }
    match r {
        Ok(Started::On) => {
            let log = Log::new("Auto Slowmode On", ORANGE)
                .line("Channel", &channel(ch.get()))
                .line("Slowmode", &format!("`{}s`", cfg.slowmode_secs))
                .line("Factor", &format!("*{}% of max heat*", cfg.channel_pct));
            if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
                tracing::warn!(?e, %ch, "could not log the auto slowmode");
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(?e, %ch, "auto slowmode failed");
            // Every flooding message would log it again (missing Manage Channels): once an hour.
            let now = now_ms();
            let due = app
                .slow
                .lock()
                .unwrap()
                .get_mut(&ch.get())
                .is_some_and(|c| {
                    let due = now - c.warned_ms >= 3_600_000;
                    if due {
                        c.warned_ms = now;
                    }
                    due
                });
            if due {
                let log = Log::new("Auto Slowmode Failed", RED)
                    .line("Channel", &channel(ch.get()))
                    .line("Error", &http_err::why(&e));
                if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
                    tracing::warn!(?e, %ch, "could not log the failed auto slowmode");
                }
            }
        }
    }
}

async fn try_start(
    app: &App,
    gid: Id<GuildMarker>,
    ch: Id<ChannelMarker>,
    cfg: &AutoSlowmodeCfg,
) -> Result<Started> {
    let locked = crate::lockdown::active(app, gid)
        .await?
        .iter()
        .any(|k| k.covers().0 || *k == LockKind::Channel(ch.get()));
    if locked {
        return Ok(Started::Nothing);
    }
    let c = app.http.channel(ch).await?.model().await?;
    anyhow::ensure!(
        c.guild_id == Some(gid),
        "<#{ch}> is not a channel of this server"
    );
    let (prior, set) = (c.rate_limit_per_user.unwrap_or(0), cfg.slowmode_secs as u16);
    let off_at = now_secs() + i64::from(cfg.min_on_secs);
    if !raises(prior, set) {
        requeue_stuck(app, gid.get(), ch.get(), off_at).await?;
        return Ok(Started::Nothing);
    }
    if !claim(app, gid.get(), ch.get(), prior, set, off_at).await? {
        return Ok(Started::Owned);
    }
    // A failed PATCH keeps the row and its job: the job finds the old value and only drops the row.
    app.http
        .update_channel(ch)
        .rate_limit_per_user(set)
        .reason(REASON)
        .await?;
    let marked = match mark_on(app, ch.get()).await {
        Ok(m) => m,
        Err(e) => {
            // The slowmode is on; the pending row and its job still turn it off.
            tracing::warn!(?e, %ch, "auto slowmode is on, marking its row failed");
            return Ok(Started::On);
        }
    };
    match marked {
        Some(None) => Ok(Started::On),
        // Staff changed it between our read and our write: theirs wins. If this PATCH fails, the
        // row keeps staff's value and the off job puts it back.
        Some(Some(staff)) => {
            app.http
                .update_channel(ch)
                .rate_limit_per_user(u16::try_from(staff)?)
                .reason(REASON)
                .await?;
            drop_rows(app, &[ch.get()]).await?;
            Ok(Started::Nothing)
        }
        // The off job already ran and put the old value back.
        None => Ok(Started::Nothing),
    }
}

/// Writes the pending row and its undo job together; false when the channel already has a row.
/// A row whose off job is gone (it gave up after its retries) is stuck and is taken over, so it
/// cannot block the channel for good.
async fn claim(app: &App, guild: u64, ch: u64, prior: u16, set: u16, off_at: i64) -> Result<bool> {
    let mut tx = app.pool.begin().await?;
    // A parallel claim that also saw the stuck row finds it deleted here, then conflicts with ours.
    sqlx::query(
        "DELETE FROM auto_slowmode WHERE channel_id = $1 AND NOT EXISTS (
             SELECT 1 FROM scheduled_jobs WHERE kind = $2 AND payload->>'channel' = $3)",
    )
    .bind(ch as i64)
    .bind(KIND)
    .bind(ch.to_string())
    .execute(&mut *tx)
    .await?;
    let new = sqlx::query(
        "INSERT INTO auto_slowmode (channel_id, guild_id, prior_secs, set_secs)
         VALUES ($1, $2, $3, $4) ON CONFLICT (channel_id) DO NOTHING",
    )
    .bind(ch as i64)
    .bind(guild as i64)
    .bind(i32::from(prior))
    .bind(i32::from(set))
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if !new {
        return Ok(false);
    }
    let payload = json!({"channel": ch, "set_secs": set});
    jobs::enqueue(&mut *tx, guild as i64, KIND, payload, off_at).await?;
    tx.commit().await?;
    app.slow_active.lock().unwrap().insert(ch);
    Ok(true)
}

/// A row whose off job gave up while its slowmode is still on would keep that slowmode for good:
/// [`claim`] never runs for it, because the channel already looks slow enough. It gets a new off job.
async fn requeue_stuck(app: &App, guild: u64, ch: u64, off_at: i64) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let stuck: Option<i32> = sqlx::query_scalar(
        "SELECT set_secs FROM auto_slowmode WHERE channel_id = $1 AND guild_id = $2
           AND NOT EXISTS (SELECT 1 FROM scheduled_jobs WHERE kind = $3 AND payload->>'channel' = $4)
         FOR UPDATE",
    )
    .bind(ch as i64)
    .bind(guild as i64)
    .bind(KIND)
    .bind(ch.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(set) = stuck {
        let payload = json!({"channel": ch, "set_secs": set});
        jobs::enqueue(&mut *tx, guild as i64, KIND, payload, off_at).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Pending -> on, unless staff changed the slowmode meanwhile. None: the row is gone;
/// Some(Some(secs)): staff's value, to put back.
async fn mark_on(app: &App, ch: u64) -> Result<Option<Option<i32>>> {
    Ok(sqlx::query_scalar(
        "UPDATE auto_slowmode SET state = CASE WHEN staff_secs IS NULL THEN 'on' ELSE state END
         WHERE channel_id = $1 RETURNING staff_secs",
    )
    .bind(ch as i64)
    .fetch_optional(&app.pool)
    .await?)
}

/// Removes the channels' rows and off jobs: staff took over, or starting failed.
async fn drop_rows(app: &App, channels: &[u64]) -> Result<()> {
    let ids: Vec<i64> = channels.iter().map(|&c| c as i64).collect();
    let keys: Vec<String> = channels.iter().map(u64::to_string).collect();
    let mut tx = app.pool.begin().await?;
    sqlx::query("DELETE FROM auto_slowmode WHERE channel_id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM scheduled_jobs WHERE kind = $1 AND payload->>'channel' = ANY($2)")
        .bind(KIND)
        .bind(&keys)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    for &c in channels {
        app.slow_active.lock().unwrap().remove(&c);
        release(app, c);
    }
    Ok(())
}

/// Staff set the slowmode of `ch` to `secs` with `/slowmode`. Theirs wins: a running auto slowmode
/// ends without a revert, and a starting one puts `secs` back after its write.
pub async fn staff_set(app: &App, ch: u64, secs: u16) -> Result<()> {
    if !app.slow_active.lock().unwrap().contains(&ch) {
        return Ok(());
    }
    let state: Option<String> = sqlx::query_scalar(
        "UPDATE auto_slowmode SET staff_secs = CASE WHEN state = 'pending' THEN $2 ELSE staff_secs END
         WHERE channel_id = $1 RETURNING state",
    )
    .bind(ch as i64)
    .bind(i32::from(secs))
    .fetch_optional(&app.pool)
    .await?;
    if state.as_deref() == Some("on") {
        drop_rows(app, &[ch]).await?;
    }
    Ok(())
}

/// A channel update while an auto slowmode starts. Its value can be older than our own write, so
/// the live value is read: one that is neither the old nor our value is staff's, and wins. While
/// the slowmode is on nothing is needed: the off job reverts only its own value.
pub async fn channel_updated(app: &App, ch: u64) -> Result<()> {
    if !app.slow_active.lock().unwrap().contains(&ch) {
        return Ok(());
    }
    let pending: Option<(i32, i32)> = sqlx::query_as(
        "SELECT prior_secs, set_secs FROM auto_slowmode WHERE channel_id = $1 AND state = 'pending'",
    )
    .bind(ch as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((prior, set)) = pending else {
        return Ok(());
    };
    let live = app.http.channel(Id::new(ch)).await?.model().await?;
    if let Some(secs) = staff_value(live.rate_limit_per_user.unwrap_or(0), prior, set) {
        sqlx::query(
            "UPDATE auto_slowmode SET staff_secs = $2 WHERE channel_id = $1 AND state = 'pending'",
        )
        .bind(ch as i64)
        .bind(secs)
        .execute(&app.pool)
        .await?;
    }
    Ok(())
}

/// Staff's slowmode, if the live value is neither the one before nor the one we set.
fn staff_value(live: u16, prior: i32, set: i32) -> Option<i32> {
    let live = i32::from(live);
    (live != prior && live != set).then_some(live)
}

fn release(app: &App, ch: u64) {
    if let Some(c) = app.slow.lock().unwrap().get_mut(&ch) {
        c.release(now_ms());
    }
}

/// The `slowmode_off` job: waits while the channel is still hot, then puts the old value back if
/// the slowmode is still ours.
pub async fn off(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let ch = p["channel"]
        .as_u64()
        .context("job payload has no channel")?;
    let cfg = app.cfg(gid.get()).await?;
    let (slow, heat) = (&cfg.automod.auto_slowmode, &cfg.automod.heat);
    // After a restart nothing is known, which counts as calm: a flood still going trips it again.
    let calm = app
        .slow
        .lock()
        .unwrap()
        .get(&ch)
        .is_none_or(|c| c.is_calm(now_ms(), slow, heat));
    let mut tx = app.pool.begin().await?;
    if !calm {
        // Only while the row is there (locked, so staff ending it waits): no job without a row.
        let held = sqlx::query(
            "SELECT 1 FROM auto_slowmode WHERE channel_id = $1 AND guild_id = $2 FOR UPDATE",
        )
        .bind(ch as i64)
        .bind(gid.get() as i64)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if held {
            let at = now_secs() + i64::from(slow.calm_secs);
            jobs::enqueue(&mut *tx, gid.get() as i64, KIND, p.clone(), at).await?;
        }
        tx.commit().await?;
        return Ok(());
    }
    // The claim: a second run waits on this row and then finds nothing. Staff's value, when they
    // changed it while it started and putting it back failed, wins over the old one.
    let row: Option<(i32, i32)> = sqlx::query_as(
        "DELETE FROM auto_slowmode WHERE channel_id = $1 AND guild_id = $2
         RETURNING COALESCE(staff_secs, prior_secs), set_secs",
    )
    .bind(ch as i64)
    .bind(gid.get() as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((prior, set)) = row else {
        return Ok(());
    };
    let r = async {
        let now = app.http.channel(Id::new(ch)).await?.model().await?;
        anyhow::ensure!(
            now.guild_id == Some(gid),
            "<#{ch}> is not a channel of this server"
        );
        let back = revert_to(now.rate_limit_per_user.unwrap_or(0), set, prior);
        if let Some(v) = back {
            app.http
                .update_channel(Id::new(ch))
                .rate_limit_per_user(v)
                .reason(REASON)
                .await?;
        }
        anyhow::Ok(back)
    }
    .await;
    // A deleted channel ends the job (ONE_TARGET), and so does a missing permission, which a retry
    // cannot fix; any other failure keeps the row for the retry.
    let forbidden = r.as_ref().is_err_and(http_err::forbidden_any);
    if r.as_ref()
        .is_err_and(|e| !http_err::gone_any(e) && !forbidden)
    {
        return r.map(drop);
    }
    tx.commit().await?;
    app.slow_active.lock().unwrap().remove(&ch);
    release(app, ch);
    let log = match r {
        Err(e) if forbidden => {
            tracing::warn!(?e, ch, "auto slowmode could not be turned off");
            Log::new("Auto Slowmode Off Failed", RED)
                .line("Channel", &channel(ch))
                .line("Error", &http_err::why(&e))
        }
        r => {
            let value = r?.map_or("left as staff set it".into(), |v| format!("`{v}s` again"));
            Log::new("Auto Slowmode Off", DONE)
                .line("Channel", &channel(ch))
                .line("Slowmode", &value)
        }
    };
    if let Err(e) = send_log(app, gid.get(), LogKind::AutoMod, log).await {
        tracing::warn!(?e, ch, "could not log the auto slowmode end");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_lowers_and_reverts_only_its_own_value() {
        assert!(raises(0, 5));
        assert!(!raises(5, 5) && !raises(30, 5));
        assert_eq!(revert_to(5, 5, 0), Some(0));
        assert_eq!(revert_to(5, 5, 2), Some(2));
        assert_eq!(revert_to(30, 5, 0), None, "staff changed it");
        assert_eq!(revert_to(0, 5, 0), None);
    }

    #[test]
    fn a_channel_update_counts_as_staff_only_with_a_new_value() {
        assert_eq!(staff_value(0, 0, 5), None, "not slowed yet, or a rename");
        assert_eq!(staff_value(5, 0, 5), None, "our own value");
        assert_eq!(staff_value(30, 0, 5), Some(30));
        assert_eq!(staff_value(0, 10, 5), Some(0), "staff turned it off");
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn claims_overrides_and_off_runs_once() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = std::sync::Arc::new(App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        ));
        let g = 6_200_000_000 + std::process::id() as u64 * 10;
        let (a, b) = (g + 1, g + 2);
        let jobs_of = |ch: u64| {
            let app = app.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM scheduled_jobs WHERE kind = $1 AND payload->>'channel' = $2",
                )
                .bind(KIND)
                .bind(ch.to_string())
                .fetch_one(&app.pool)
                .await
                .unwrap()
            }
        };
        let rows_of = |ch: u64| {
            let app = app.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM auto_slowmode WHERE channel_id = $1",
                )
                .bind(ch as i64)
                .fetch_one(&app.pool)
                .await
                .unwrap()
            }
        };
        // Two parallel claims: one row, one job.
        let at = now_secs() + 3600;
        let (x, y) = tokio::join!(claim(&app, g, a, 0, 5, at), claim(&app, g, a, 0, 5, at));
        assert_eq!([x.unwrap(), y.unwrap()].iter().filter(|w| **w).count(), 1);
        assert_eq!((rows_of(a).await, jobs_of(a).await), (1, 1));
        assert!(app.slow_active.lock().unwrap().contains(&a));
        assert_eq!(mark_on(&app, a).await.unwrap(), Some(None));
        // A stuck row (its off job gave up) gets one new off job, and only one.
        sqlx::query("DELETE FROM scheduled_jobs WHERE kind = $1 AND payload->>'channel' = $2")
            .bind(KIND)
            .bind(a.to_string())
            .execute(&app.pool)
            .await
            .unwrap();
        requeue_stuck(&app, g, a, at).await.unwrap();
        requeue_stuck(&app, g, a, at).await.unwrap();
        assert_eq!(jobs_of(a).await, 1);
        // Rolled back after a failed start, or ended by staff: row and job are gone.
        drop_rows(&app, &[a]).await.unwrap();
        assert_eq!((rows_of(a).await, jobs_of(a).await), (0, 0));
        assert!(!app.slow_active.lock().unwrap().contains(&a));

        // /slowmode while pending: mark_on hands back staff's value.
        assert!(claim(&app, g, b, 0, 5, at).await.unwrap());
        staff_set(&app, b, 30).await.unwrap();
        assert_eq!(mark_on(&app, b).await.unwrap(), Some(Some(30)));
        drop_rows(&app, &[b]).await.unwrap();

        // A running one: /slowmode ends it.
        assert!(claim(&app, g, b, 0, 5, at).await.unwrap());
        assert_eq!(mark_on(&app, b).await.unwrap(), Some(None));
        staff_set(&app, b, 5).await.unwrap();
        assert_eq!((rows_of(b).await, jobs_of(b).await), (0, 0));

        // A row whose job gave up is taken over; one with a live job is not.
        assert!(claim(&app, g, b, 0, 5, at).await.unwrap());
        assert!(!claim(&app, g, b, 0, 5, at).await.unwrap());
        sqlx::query("DELETE FROM scheduled_jobs WHERE kind = $1 AND payload->>'channel' = $2")
            .bind(KIND)
            .bind(b.to_string())
            .execute(&app.pool)
            .await
            .unwrap();
        let (x, y) = tokio::join!(claim(&app, g, b, 2, 5, at), claim(&app, g, b, 2, 5, at));
        assert_eq!([x.unwrap(), y.unwrap()].iter().filter(|w| **w).count(), 1);
        assert_eq!((rows_of(b).await, jobs_of(b).await), (1, 1));

        // A hot channel's off job waits again, but only while its row is there, and only for
        // its own server.
        let hot = Breakdown {
            parts: vec![("message", 1e6)],
        };
        let (cfg, heat) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        // Three members: each adds at most max heat.
        for (ch, user) in [a, b]
            .into_iter()
            .flat_map(|ch| (1..=3).map(move |u| (ch, u)))
        {
            app.slow.lock().unwrap().entry(ch).or_default().on_message(
                now_ms(),
                user,
                &hot,
                &cfg,
                &heat,
            );
        }
        let job = |ch: u64| json!({"channel": ch, "set_secs": 5});
        off(&app, Id::new(g), &job(a)).await.unwrap();
        assert_eq!(jobs_of(a).await, 0, "no row, no new job");
        off(&app, Id::new(g + 9), &job(b)).await.unwrap();
        assert_eq!(
            jobs_of(b).await,
            1,
            "another server's job does not see the row"
        );
        off(&app, Id::new(g), &job(b)).await.unwrap();
        assert_eq!(jobs_of(b).await, 2, "queued again while hot");
        drop_rows(&app, &[b]).await.unwrap();
        assert_eq!((rows_of(b).await, jobs_of(b).await), (0, 0));
        app.slow.lock().unwrap().clear();

        // The off job's claim: two runs, one gets the row.
        assert!(claim(&app, g, a, 0, 5, at).await.unwrap());
        let take = || {
            let app = app.clone();
            async move {
                sqlx::query_as::<_, (i32, i32)>(
                    "DELETE FROM auto_slowmode WHERE channel_id = $1 RETURNING prior_secs, set_secs",
                )
                .bind(a as i64)
                .fetch_optional(&app.pool)
                .await
                .unwrap()
            }
        };
        let (x, y) = tokio::join!(take(), take());
        assert_eq!([x, y].iter().flatten().count(), 1);
        // A missing row (claimed already) makes the job a no-op without Discord.
        off(&app, Id::new(g), &json!({"channel": a, "set_secs": 5}))
            .await
            .unwrap();
        sqlx::query("DELETE FROM scheduled_jobs WHERE guild_id = $1")
            .bind(g as i64)
            .execute(&app.pool)
            .await
            .unwrap();
    }
}
