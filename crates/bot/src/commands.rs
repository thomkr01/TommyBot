//! Prefix mod commands: permit check -> action -> case (+ undo data). ban/kick/timeout/warn.
use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use tommybot_core::{cases, permits, warn};
use twilight_http::Client;
use twilight_model::{
    gateway::payload::incoming::MessageCreate,
    id::{marker::UserMarker, Id},
};

use crate::prefix::Parsed;

pub const PREFIX: &str = "w!";

pub struct Ctx<'a> {
    pub http: &'a Client,
    pub pool: &'a PgPool,
}

/// Replies are plain text; returns Err only for infrastructure failures.
pub async fn run(ctx: &Ctx<'_>, msg: &MessageCreate, p: Parsed) -> Result<()> {
    let (Some(gid), Some(member)) = (msg.guild_id, msg.member.as_ref()) else { return Ok(()) };
    let perm = match p.command.as_str() {
        "ban" | "kick" | "warn" => p.command.as_str(),
        "timeout" | "mute" => "timeout",
        _ => return Ok(()),
    };
    let guild = ctx.http.guild(gid).await?.model().await?;
    let roles: Vec<i64> = member.roles.iter().map(|r| r.get() as i64).collect();
    let actor = permits::resolve(ctx.pool, gid.get() as i64, guild.owner_id.get() as i64, msg.author.id.get() as i64, &roles).await?;

    for t in &p.targets {
        let uid: Id<UserMarker> = Id::new(*t);
        let target = match ctx.http.guild_member(gid, uid).await {
            Ok(r) => {
                let m = r.model().await?;
                let r: Vec<i64> = m.roles.iter().map(|r| r.get() as i64).collect();
                permits::resolve(ctx.pool, gid.get() as i64, guild.owner_id.get() as i64, *t as i64, &r).await?
            }
            Err(_) => permits::Level::None, // not in the server (hackban)
        };
        // shortcut: bot-role hierarchy not checked (needs role cache), Discord rejects it with an error shown below.
        if let Err(d) = permits::check(&actor, perm, msg.author.id.get() as i64, &target, *t as i64, false) {
            say(ctx, msg, &format!("Cannot {perm} <@{t}>: {d:?}")).await?;
            continue;
        }
        let reason = p.reason.as_deref();
        let (kind, inverse, points) = match perm {
            "ban" => {
                ctx.http.create_ban(gid, uid).await?;
                ("ban", Some(json!({"op":"unban","user":t})), 0)
            }
            "kick" => {
                ctx.http.remove_guild_member(gid, uid).await?;
                ("kick", None, 0)
            }
            "timeout" => {
                let secs = p.duration_s.unwrap_or(600);
                let until = twilight_model::util::Timestamp::from_secs(chrono_now() + secs)?;
                ctx.http.update_guild_member(gid, uid).communication_disabled_until(Some(until)).await?;
                ("timeout", Some(json!({"op":"untimeout","user":t})), 0)
            }
            _ => ("warn", None, 1),
        };
        let case_no = cases::create(ctx.pool, cases::NewCase {
            guild_id: gid.get() as i64, kind, user_id: *t as i64, mod_id: msg.author.id.get() as i64,
            reason, points, duration_s: p.duration_s, inverse,
        }).await?;
        say(ctx, msg, &format!("Case #{case_no}: {kind} <@{t}>")).await?;
        if kind == "warn" {
            apply_warn_thresholds(ctx, msg, gid.get() as i64, *t as i64, points).await?;
        }
    }
    Ok(())
}

async fn apply_warn_thresholds(ctx: &Ctx<'_>, msg: &MessageCreate, gid: i64, uid: i64, added: i32) -> Result<()> {
    let rows: Vec<(i32, String, Option<i64>)> =
        sqlx::query_as("SELECT points, action, duration_s FROM warn_thresholds WHERE guild_id = $1")
            .bind(gid).fetch_all(ctx.pool).await?;
    let ts: Vec<warn::Threshold> = rows.into_iter().filter_map(|(points, a, d)| {
        let action = match a.as_str() {
            "timeout" => warn::Action::Timeout { secs: d? },
            "kick" => warn::Action::Kick,
            "ban" => warn::Action::Ban,
            _ => return None,
        };
        Some(warn::Threshold { points, action })
    }).collect();
    let after = cases::warn_points(ctx.pool, gid, uid).await?;
    let Some(t) = warn::crossed(&ts, after - added, after) else { return Ok(()) };
    let (g, u) = (Id::new(gid as u64), Id::<UserMarker>::new(uid as u64));
    match t.action {
        warn::Action::Ban => { ctx.http.create_ban(g, u).await?; }
        warn::Action::Kick => { ctx.http.remove_guild_member(g, u).await?; }
        warn::Action::Timeout { secs } => {
            let until = twilight_model::util::Timestamp::from_secs(chrono_now() + secs)?;
            ctx.http.update_guild_member(g, u).communication_disabled_until(Some(until)).await?;
        }
    }
    cases::reset_warns(ctx.pool, gid, uid).await?;
    say(ctx, msg, &format!("<@{uid}> reached {} warn points: {:?}", t.points, t.action)).await
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

async fn say(ctx: &Ctx<'_>, msg: &MessageCreate, text: &str) -> Result<()> {
    ctx.http.create_message(msg.channel_id).content(text).await?;
    Ok(())
}
