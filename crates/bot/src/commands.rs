//! Prefix mod commands: permit check -> action -> case (+ undo data). ban/kick/timeout/warn.
use anyhow::Result;
use serde_json::json;
use tommybot_core::{cases, permits, warn};
use twilight_http::Client;
use twilight_model::{
    gateway::payload::incoming::MessageCreate,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
    util::Timestamp,
};

use crate::{app::App, lockdown, prefix::Parsed};

pub const PREFIX: &str = "w!";

/// Replies are plain text; returns Err only for infrastructure failures.
pub async fn run(app: &App, msg: &MessageCreate, p: Parsed) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    let perm = match p.command.as_str() {
        "ban" | "kick" | "warn" | "lock" | "unlock" => p.command.as_str(),
        "timeout" | "mute" => "timeout",
        _ => return Ok(()),
    };
    let owner = app.http.guild(gid).await?.model().await?.owner_id;
    let actor = app.level(gid, owner, msg.author.id).await?;
    if matches!(perm, "lock" | "unlock") {
        return lockdown_cmd(app, msg, &actor, &p).await;
    }

    for t in &p.targets {
        let uid: Id<UserMarker> = Id::new(*t);
        let target = app.level(gid, owner, uid).await?;
        // shortcut: bot-role hierarchy not checked (needs role cache), Discord rejects it with an error shown below.
        if let Err(d) = permits::check(
            &actor,
            perm,
            msg.author.id.get() as i64,
            &target,
            *t as i64,
            false,
        ) {
            say(app, msg, &format!("Cannot {perm} <@{t}>: {d:?}")).await?;
            continue;
        }
        let reason = p.reason.as_deref();
        let (kind, inverse, points) = match perm {
            "ban" => {
                app.http.create_ban(gid, uid).await?;
                ("ban", Some(json!({"op":"unban","user":t})), 0)
            }
            "kick" => {
                app.http.remove_guild_member(gid, uid).await?;
                ("kick", None, 0)
            }
            "timeout" => {
                timeout(&app.http, gid, uid, p.duration_s.unwrap_or(600)).await?;
                ("timeout", Some(json!({"op":"untimeout","user":t})), 0)
            }
            _ => ("warn", None, 1),
        };
        let case_no = cases::create(
            &app.pool,
            cases::NewCase {
                guild_id: gid.get() as i64,
                kind,
                user_id: *t as i64,
                mod_id: msg.author.id.get() as i64,
                reason,
                points,
                duration_s: p.duration_s,
                inverse,
            },
        )
        .await?;
        say(app, msg, &format!("Case #{case_no}: {kind} <@{t}>")).await?;
        if kind == "warn" {
            apply_warn_thresholds(app, msg, gid.get() as i64, *t as i64, points).await?;
        }
    }
    Ok(())
}

async fn apply_warn_thresholds(
    app: &App,
    msg: &MessageCreate,
    gid: i64,
    uid: i64,
    added: i32,
) -> Result<()> {
    let rows: Vec<(i32, String, Option<i64>)> = sqlx::query_as(
        "SELECT points, action, duration_s FROM warn_thresholds WHERE guild_id = $1",
    )
    .bind(gid)
    .fetch_all(&app.pool)
    .await?;
    let ts: Vec<warn::Threshold> = rows
        .into_iter()
        .filter_map(|(points, a, d)| {
            let action = match a.as_str() {
                "timeout" => warn::Action::Timeout { secs: d? },
                "kick" => warn::Action::Kick,
                "ban" => warn::Action::Ban,
                _ => return None,
            };
            Some(warn::Threshold { points, action })
        })
        .collect();
    let after = cases::warn_points(&app.pool, gid, uid).await?;
    let Some(t) = warn::crossed(&ts, after - added, after) else {
        return Ok(());
    };
    let (g, u) = (Id::new(gid as u64), Id::<UserMarker>::new(uid as u64));
    match t.action {
        warn::Action::Ban => {
            app.http.create_ban(g, u).await?;
        }
        warn::Action::Kick => {
            app.http.remove_guild_member(g, u).await?;
        }
        warn::Action::Timeout { secs } => timeout(&app.http, g, u, secs).await?,
    }
    cases::reset_warns(&app.pool, gid, uid).await?;
    say(
        app,
        msg,
        &format!("<@{uid}> reached {} warn points: {:?}", t.points, t.action),
    )
    .await
}

pub async fn timeout(
    http: &Client,
    g: Id<GuildMarker>,
    u: Id<UserMarker>,
    secs: i64,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let until = Timestamp::from_secs(now + secs)?;
    http.update_guild_member(g, u)
        .communication_disabled_until(Some(until))
        .await?;
    Ok(())
}

async fn say(app: &App, msg: &MessageCreate, text: &str) -> Result<()> {
    app.http
        .create_message(msg.channel_id)
        .content(text)
        .await?;
    Ok(())
}

async fn lockdown_cmd(
    app: &App,
    msg: &MessageCreate,
    actor: &permits::Level,
    p: &Parsed,
) -> Result<()> {
    let gid = msg.guild_id.expect("checked by caller");
    if !actor.has("lockdown") {
        return say(app, msg, "Cannot lock: no permit").await;
    }
    if p.command == "unlock" {
        lockdown::unlock(app, gid).await?;
        return say(app, msg, "Server unlocked").await;
    }
    let done = lockdown::lock(app, gid, tommybot_automod::lockdown::LockKind::Server).await?;
    say(
        app,
        msg,
        if done {
            "Server locked"
        } else {
            "Already locked"
        },
    )
    .await
}
