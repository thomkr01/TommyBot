//! Voice join raid (Join Raid's Voice Raid card): many members joining voice at once. Everyone
//! who joins voice during the raid is disconnected and gets the card's action, except staff (any
//! permit), bots, the bot itself and members on the Auto Mod spam whitelist.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_automod::join::RaidCfg;
use tommybot_core::{jobs, permits};
use tommybot_logging::{embed::duration, LogKind};
use twilight_http::request::AuditLogReason;
use twilight_model::{
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker},
        Id,
    },
    voice::VoiceState,
};

use crate::{
    app::{App, GuildCfg},
    automod::now_ms,
    events::send_log_pinging,
    joingate::{self, Detail},
};

const JOB: &str = "voice_raid_end";

/// A voice update; `old` is the member's channel before it (from the logger). Only a join from no
/// channel counts: moves and mute changes keep a channel.
pub async fn on_voice(app: &App, v: &VoiceState, old: Option<Id<ChannelMarker>>) -> Result<()> {
    let Some(gid) = v
        .guild_id
        .filter(|_| old.is_none() && v.channel_id.is_some())
    else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    let Some(join) = cfg.joinraid.as_ref().filter(|r| r.voice.enabled) else {
        return Ok(());
    };
    let raid = &join.voice;
    // A member whose level could not be read is never flagged.
    let (exempt, name) = match who(app, &cfg, gid, v).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                ?e,
                user = v.user_id.get(),
                "voice raid: reading the member failed"
            );
            return Ok(());
        }
    };
    let now = now_ms();
    let (out, started, code, extended) = {
        let mut all = app.voice_raids.lock().unwrap();
        all.retain(|_, d| !d.idle(now));
        let d = all.entry(gid.get()).or_default();
        let before = d.active_until();
        let out = d.on_join(v.user_id.get(), &name, exempt, raid, now);
        // Claimed here, under the lock: one trigger per active period.
        if out.triggered {
            d.code = format!("voice-{}", crate::invites::raid_code(gid.get()));
        }
        let extended = (!out.triggered && d.active_until() > before).then(|| d.active_until());
        (out, d.started, d.code.clone(), extended)
    };
    if out.flagged.is_empty() {
        return Ok(());
    }
    if out.triggered {
        triggered(app, gid, join, &out.flagged, (started, &code), now).await;
    }
    if let Some(until) = extended {
        if let Err(e) = extend(&app.pool, gid.get() as i64, &code, until).await {
            tracing::warn!(?e, "voice raid: saving the new end failed");
        }
    }
    let (ids, names): (Vec<i64>, Vec<String>) = out
        .flagged
        .iter()
        .map(|(u, n)| (*u as i64, n.clone()))
        .unzip();
    let logged = sqlx::query(
        "insert into raid_log (guild_id, raid_ms, user_id, name, code)
         select $1, $2, u, n, $5 from unnest($3::bigint[], $4::text[]) as t(u, n)
         on conflict do nothing",
    )
    .bind(gid.get() as i64)
    .bind(started)
    .bind(&ids)
    .bind(&names)
    .bind(&code)
    .execute(&app.pool)
    .await;
    if let Err(e) = logged {
        tracing::warn!(?e, "saving the voice raid log failed");
    }
    // One member who already left or a refused step must not let the rest go.
    for (id, _) in out.flagged {
        let uid = Id::new(id);
        let left = app
            .http
            .update_guild_member(gid, uid)
            .channel_id(None)
            .reason("Voice raid")
            .await;
        if let Err(e) = left {
            tracing::warn!(?e, user = id, "voice raid: disconnecting failed");
        }
        let r = joingate::punish(
            app,
            &cfg,
            gid,
            id,
            raid.action(),
            "voice raid",
            Detail::default(),
        )
        .await;
        if let Err(e) = r {
            tracing::warn!(?e, user = id, "voice raid: punishing failed");
        }
    }
    Ok(())
}

/// Exempt (staff, bot, the bot itself, spam whitelist) and the member's name.
async fn who(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    v: &VoiceState,
) -> Result<(bool, String)> {
    let user = v.user_id;
    let (bot, name, roles): (bool, String, Vec<Id<RoleMarker>>) = match &v.member {
        Some(m) => (m.user.bot, m.user.name.clone(), m.roles.clone()),
        None => match crate::http_err::member(&app.http, gid, user).await? {
            Some(m) => (m.user.bot, m.user.name, m.roles),
            None => return Ok((true, String::new())),
        },
    };
    let w = &cfg.whitelist.spam;
    if bot || user == app.me || w.users.contains(&user) || roles.iter().any(|r| w.roles.contains(r))
    {
        return Ok((true, name));
    }
    let owner = app.guild_info(gid).await?.owner;
    let ids: Vec<i64> = roles.iter().map(|r| r.get() as i64).collect();
    let level = permits::resolve(
        &app.pool,
        gid.get() as i64,
        owner.get() as i64,
        user.get() as i64,
        &ids,
    )
    .await?;
    Ok((!matches!(level, permits::Level::None), name))
}

/// Report job first, then the log and the owner alert; a failure of one never stops the rest.
async fn triggered(
    app: &App,
    gid: Id<GuildMarker>,
    join: &RaidCfg,
    flagged: &[(u64, String)],
    (started, code): (i64, &str),
    now: i64,
) {
    let raid = &join.voice;
    let until = now + raid.active();
    let job = json!({ "until_ms": until, "started_ms": started, "code": code });
    // Its own job, not a replace: the last raid's report may be due and not yet claimed.
    if let Err(e) = jobs::enqueue(&app.pool, gid.get() as i64, JOB, job, until / 1000).await {
        tracing::warn!(?e, "queueing the voice raid report failed");
    }
    let next = format!(
        "Members joining voice for the next `{}` will be disconnected and **{}**.",
        duration(raid.active() / 1000),
        joingate::punished(raid.action())
    );
    let flagged: Vec<(u64, &str)> = flagged.iter().map(|(u, n)| (*u, n.as_str())).collect();
    let url = crate::invites::raid_url(code);
    let log = joingate::trigger_log(
        ("🎙 Voice Raid Trigger!", "voice join"),
        &next,
        &flagged,
        code,
        url,
    );
    let warned = joingate::warned_roles(join);
    if let Err(e) = send_log_pinging(app, gid.get(), LogKind::JoinRaid, log, &warned).await {
        tracing::warn!(?e, "voice raid log failed");
    }
    crate::reply::alert_owner(app, gid, "A voice raid started").await;
}

/// Writes an extended raid's end into its report job, so `restore` after a restart keeps it.
/// A job the scheduler holds right now re-queues itself from memory (`end`).
async fn extend(pool: &sqlx::PgPool, gid: i64, code: &str, until: i64) -> Result<()> {
    sqlx::query(
        "UPDATE scheduled_jobs
         SET payload = jsonb_set(payload, '{until_ms}', to_jsonb(GREATEST((payload->>'until_ms')::bigint, $3))),
             run_at = GREATEST(run_at, to_timestamp($3 / 1000))
         WHERE guild_id = $1 AND kind = $4 AND payload->>'code' = $2
           AND (locked_until IS NULL OR locked_until < now())",
    )
    .bind(gid)
    .bind(code)
    .bind(until)
    .bind(JOB)
    .execute(pool)
    .await?;
    Ok(())
}

/// The `voice_raid_end` job: an extended raid waits for its new end, otherwise the report.
pub async fn end(app: &App, gid: Id<GuildMarker>, p: &Value) -> Result<()> {
    let code = p["code"].as_str().unwrap_or_default();
    let until = app
        .voice_raids
        .lock()
        .unwrap()
        .get(&gid.get())
        .filter(|d| d.code == code)
        .map(|d| d.active_until());
    if let Some(until) = until.filter(|u| *u > now_ms() + 1000) {
        let mut p = p.clone();
        p["until_ms"] = json!(until);
        // This job is finished after it returns; a replace would also delete a newer raid's job.
        return jobs::enqueue(&app.pool, gid.get() as i64, JOB, p, until / 1000)
            .await
            .map(drop);
    }
    let caught: i64 = sqlx::query_scalar("select count(*) from raid_log where code = $1")
        .bind(code)
        .fetch_one(&app.pool)
        .await?;
    let url = crate::invites::raid_url(code).filter(|_| caught > 0);
    let log = joingate::report_log(
        ("🎙 Voice Raid Report!", "voice raid"),
        caught,
        url.is_some().then_some(code),
    );
    crate::scheduler::raid_report(app, gid, log, url).await
}

/// Re-arms voice raids that were running at restart, from their pending report jobs (oldest end
/// first, so the newest raid of a guild is the one kept).
pub async fn restore(app: &App) -> Result<()> {
    for (guild, p) in jobs::pending(&app.pool, JOB).await? {
        if let Some(until) = p["until_ms"].as_i64() {
            let d = tommybot_automod::voice::VoiceRaidDetector::restored(
                p["started_ms"].as_i64().unwrap_or(until),
                until,
                p["code"].as_str().unwrap_or_default().into(),
            );
            app.voice_raids.lock().unwrap().insert(guild as u64, d);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only when DATABASE_URL points at a scratch Postgres. A back-to-back raid keeps the
    /// last raid's report job, and an extension reaches the job `restore` reads.
    #[tokio::test]
    async fn back_to_back_raids_keep_both_reports_and_extensions_are_saved() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let gid = 9_720_000_000 + std::process::id() as i64;
        let job =
            |code: &str, until: i64| json!({ "until_ms": until, "started_ms": 1, "code": code });
        jobs::enqueue(&pool, gid, JOB, job("voice-a", 1_000_000), 1_000)
            .await
            .unwrap();
        jobs::enqueue(&pool, gid, JOB, job("voice-b", 2_000_000), 2_000)
            .await
            .unwrap();
        extend(&pool, gid, "voice-b", 3_000_000).await.unwrap();
        // An older value never moves an end back.
        extend(&pool, gid, "voice-b", 2_500_000).await.unwrap();
        let mine: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT payload->>'code', (payload->>'until_ms')::bigint, extract(epoch from run_at)::bigint
             FROM scheduled_jobs WHERE guild_id = $1 AND kind = $2 ORDER BY run_at",
        )
        .bind(gid)
        .bind(JOB)
        .fetch_all(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM scheduled_jobs WHERE guild_id = $1")
            .bind(gid)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            mine,
            [
                ("voice-a".into(), 1_000_000, 1_000),
                ("voice-b".into(), 3_000_000, 3_000)
            ]
        );
    }
}
