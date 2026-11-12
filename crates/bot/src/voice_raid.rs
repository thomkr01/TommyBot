//! Voice join raid (Join Raid's Voice Raid card): many members joining voice at once. Everyone
//! who joins voice during the raid is disconnected and gets the card's action, except staff (any
//! permit), bots, the bot itself and members on the Auto Mod spam whitelist.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_automod::voice::VoiceRaidCfg;
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
    events::send_log,
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
    let Some(raid) = cfg
        .joinraid
        .as_ref()
        .map(|r| &r.voice)
        .filter(|r| r.enabled)
    else {
        return Ok(());
    };
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
    let (out, started, code) = {
        let mut all = app.voice_raids.lock().unwrap();
        all.retain(|_, d| !d.idle(now));
        let d = all.entry(gid.get()).or_default();
        let out = d.on_join(v.user_id.get(), &name, exempt, raid, now);
        // Claimed here, under the lock: one trigger per active period.
        if out.triggered {
            d.code = format!("voice-{}", crate::invites::raid_code(gid.get()));
        }
        (out, d.started, d.code.clone())
    };
    if out.flagged.is_empty() {
        return Ok(());
    }
    if out.triggered {
        triggered(app, gid, raid, &out.flagged, (started, &code), now).await;
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
    raid: &VoiceRaidCfg,
    flagged: &[(u64, String)],
    (started, code): (i64, &str),
    now: i64,
) {
    let until = now + raid.active();
    let job = json!({ "until_ms": until, "started_ms": started, "code": code });
    if let Err(e) = jobs::replace(&app.pool, gid.get() as i64, JOB, job, until / 1000).await {
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
    if let Err(e) = send_log(app, gid.get(), LogKind::JoinRaid, log).await {
        tracing::warn!(?e, "voice raid log failed");
    }
    crate::reply::alert_owner(app, gid, "A voice raid started").await;
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
        return jobs::replace(&app.pool, gid.get() as i64, JOB, p, until / 1000).await;
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

/// Re-arms voice raids that were running at restart, from their pending report jobs.
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
