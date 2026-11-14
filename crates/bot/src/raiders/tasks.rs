//! The bot's jobs for the list: fan-out, member checks and scans, and lifting what it did.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tommybot_core::{jobs, raiders};
use tommybot_logging::{
    embed::{self, DONE, RED},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use super::{
    auto_list,
    join::{alert_member, bot_case, listed},
    PAGE, PREFIX,
};
use crate::{
    app::{now_secs, App},
    commands, http_err,
    scheduler::need,
};

/// The servers a fan-out job reaches: every server with the list on (check), or every server
/// where the list did something to the user (lift).
async fn targets(app: &App, kind: &str, user: i64) -> Result<Vec<i64>> {
    let q = match kind {
        "raider_check" => sqlx::query_scalar(
            "SELECT o.guild_id FROM guild_owners o
             JOIN LATERAL (SELECT config FROM guild_config WHERE guild_id = o.guild_id
                           ORDER BY version DESC LIMIT 1) c ON true
             WHERE c.config->'raider_list'->>'enabled' = 'true'
               AND o.guild_id NOT IN (SELECT guild_id FROM guild_left)",
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
        let sent = async {
            let dm = app.http.create_private_channel(Id::new(user)).await?;
            let ch = dm.model().await?.id;
            app.http
                .create_message(ch)
                .content(&embed::clip(text, 1900))
                .await?;
            anyhow::Ok(())
        };
        return match sent.await {
            Err(e) if http_err::refused(&e) => {
                tracing::info!(?e, user, "raider list: notify DM refused (closed DMs)");
                Ok(())
            }
            r => r,
        };
    }
    // `raider_autolist` {user}: an automatic listing whose first try failed.
    if job.kind == "raider_autolist" {
        return auto_list(app, Id::new(user)).await;
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
    alert_member(app, gid, user, (&roles, m.user.bot), &entry).await
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
                alert_member(app, gid, m.user.id, (&roles, m.user.bot), &entry).await?;
            }
        }
        match page.last() {
            Some(m) if page.len() == usize::from(PAGE) => after = Some(m.user.id),
            _ => break,
        }
    }
    Ok(())
}

/// A saved config that turns the list on (`version`, compared with the version before it; the
/// newest when unknown) queues the member scan, unless one is waiting.
pub async fn on_saved(app: &App, gid: Id<GuildMarker>, version: Option<i32>) -> Result<()> {
    // The saved version and the one before it, not the newest two: a second quick save must not
    // hide that this one turned the list on.
    let on: Vec<bool> = sqlx::query_scalar(
        "SELECT COALESCE((config->'raider_list'->>'enabled')::boolean, false)
           AND COALESCE((config->'raider_list'->>'scan_members')::boolean, true)
         FROM guild_config WHERE guild_id = $1 AND ($2::int IS NULL OR version <= $2)
         ORDER BY version DESC LIMIT 2",
    )
    .bind(gid.get() as i64)
    .bind(version)
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
pub(super) fn turned_on(newest_first: &[bool]) -> bool {
    matches!(newest_first, [true] | [true, false])
}

/// `raider_lift` {user} in one server, once the entry is removed: undoes the list's ban (if it is
/// still the list's) or quarantine when the server wants that, and tells the mod log.
pub async fn lift_job(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let user = Id::new(need(p, "user")?);
    let (g, u) = (gid.get() as i64, user.get() as i64);
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT action, unbanning FROM raider_actions WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(g)
    .bind(u)
    .fetch_optional(&app.pool)
    .await?;
    let Some((action, unbanning)) = row else {
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
        "ban" if lift => unban(app, gid, user, unbanning).await?,
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

/// Unbans when the ban is still the one the list made; true when it unbanned. `unbanning`: an
/// earlier try of this lift asked Discord to unban, so a missing ban is that try's work and this
/// try writes the case it did not get to.
async fn unban(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    unbanning: bool,
) -> Result<(&'static str, bool)> {
    let Some(ban) = http_err::ban(&app.http, gid, user).await? else {
        return Ok(if unbanning {
            ("Unbanned (by an earlier try of this removal).", true)
        } else {
            ("Not banned any more, nothing to undo.", false)
        });
    };
    if !ban.reason.as_deref().is_some_and(|r| r.starts_with(PREFIX)) {
        return Ok(("Kept: the ban was made again by hand.", false));
    }
    sqlx::query("UPDATE raider_actions SET unbanning = true WHERE guild_id = $1 AND user_id = $2")
        .bind(gid.get() as i64)
        .bind(user.get() as i64)
        .execute(&app.pool)
        .await?;
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
