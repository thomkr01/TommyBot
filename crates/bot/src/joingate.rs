//! Join gate, join raid detection and the punishments they share with native AutoMod.
use anyhow::Result;
use tommybot_automod::{
    filters,
    join::{gate, harshest, is_suspicious, Action, Filter, Joiner, RaidCfg},
    lockdown::JoinLock,
};
use tommybot_core::{
    cases::{self, NewCase},
    jobs,
};
use tommybot_logging::{
    embed::{duration, ALERT, ARROW, DONE, ORANGE, YES},
    Log, LogKind,
};
use twilight_model::{
    gateway::payload::incoming::{MemberAdd, MemberUpdate},
    id::{marker::GuildMarker, Id},
    user::UserFlags,
};

use crate::{
    app::{now_secs, App, GuildCfg},
    commands::timeout,
    events::{send_log, send_log_pinging},
    lockdown, reply, verify,
};

use crate::automod::{action_failed, now_ms, warn_actions};

pub async fn on_join(app: &App, m: &MemberAdd) -> Result<()> {
    let (gid, user) = (m.guild_id, &m.member.user);
    let cfg = app.cfg(gid.get()).await?;
    let now = now_ms();
    if let Some(lock) = lockdown::join_lock(app, gid).await? {
        let action = match lock {
            JoinLock::Kick => Action::Kick,
            JoinLock::Ban => Action::Ban,
        };
        return punish(app, &cfg, gid, user.id.get(), action, "join lock").await;
    }
    // One invites call per join: only when something shows the result.
    let invite = if cfg.joinraid.as_ref().is_some_and(|r| r.enabled)
        || cfg.logging.target(LogKind::Member, None).is_some()
    {
        crate::invites::used_invite(app, gid).await
    } else {
        None
    };
    if let Some((code, inviter)) = &invite {
        let log = Log::new("Invite Used", DONE)
            .user("Member", user.id.get())
            .line("Invite", &format!("`{code}`"))
            .line("Inviter", inviter);
        if let Err(e) = send_log(app, gid.get(), LogKind::Member, log).await {
            tracing::warn!(?e, "invite log failed");
        }
    }
    let name = names(&[Some(&user.name), user.global_name.as_ref()]);
    // Without an audit entry the bot counts as authorised.
    let adder = if user.bot {
        crate::welcome::bot_adder(app, gid, user.id).await
    } else {
        None
    };
    let authorized = match adder {
        Some(a) => app.is_immune(gid, a).await.unwrap_or(true),
        None => true,
    };
    let j = Joiner {
        invite: invite.as_ref().map(|i| i.0.as_str()),
        id: user.id.get(),
        name: &name,
        has_avatar: user.avatar.is_some(),
        is_bot: user.bot,
        bot_verified: user
            .public_flags
            .is_some_and(|f| f.contains(UserFlags::VERIFIED_BOT)),
        added_by_authorized: authorized,
    };
    let suspicious = is_suspicious(&j, &cfg.joingate, cfg.joingate.min_age_ms(), now);
    verify::on_join(app, gid, user.id, suspicious, user.bot).await;
    let hits = gate(&j, &cfg.joingate, now);
    if let Some(action) = harshest(&hits) {
        let why = if hits.iter().any(|h| h.0 == Filter::AccountAge) {
            let min = duration(cfg.joingate.min_age_ms() / 1000);
            format!("join gate: accounts must be at least {min} old")
        } else {
            "join gate".into()
        };
        punish(app, &cfg, gid, j.id, action, &why).await?;
        // The bot filter also punishes whoever added it.
        if let (Some(adder), true) = (adder, hits.iter().any(|h| h.0 == Filter::UnauthorizedBot)) {
            punish(
                app,
                &cfg,
                gid,
                adder.get(),
                action,
                "adding a bot without permission",
            )
            .await?;
        }
    }
    if let Some(raid) = cfg.joinraid.as_ref().filter(|r| r.enabled) {
        join_raid(app, &cfg, gid, raid, &j, suspicious, now).await?;
    }
    Ok(())
}

async fn join_raid(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    raid: &RaidCfg,
    j: &Joiner<'_>,
    suspicious: bool,
    now: i64,
) -> Result<()> {
    let (out, started, names, invites, code) = {
        let mut raids = app.raids.lock().unwrap();
        let d = raids.entry(gid.get()).or_default();
        let out = d.on_join(j, suspicious, raid, now);
        if out.triggered || d.code.is_empty() {
            d.code = crate::invites::raid_code(gid.get());
        }
        let names: Vec<String> = out
            .flagged
            .iter()
            .map(|id| d.name_of(*id).unwrap_or_default().to_string())
            .collect();
        let invites: Vec<String> = out
            .flagged
            .iter()
            .map(|id| d.invite_of(*id).unwrap_or_default().to_string())
            .collect();
        (out, d.started, names, invites, d.code.clone())
    };
    if !out.flagged.is_empty() {
        let ids: Vec<i64> = out.flagged.iter().map(|&id| id as i64).collect();
        let logged = sqlx::query(
            "insert into raid_log (guild_id, raid_ms, user_id, name, invite_code, code)
             select $1, $2, u, n, nullif(i, ''), $6
             from unnest($3::bigint[], $4::text[], $5::text[]) as t(u, n, i) on conflict do nothing",
        )
        .bind(gid.get() as i64)
        .bind(started)
        .bind(&ids)
        .bind(&names)
        .bind(&invites)
        .bind(&code)
        .execute(&app.pool)
        .await;
        if let Err(e) = logged {
            tracing::warn!(?e, "saving the raid log failed");
        }
    }
    if out.triggered {
        jobs::replace(
            &app.pool,
            gid.get() as i64,
            "raid_end",
            serde_json::json!({ "until_ms": now + raid.active_ms, "started_ms": started, "code": code }),
            (now / 1000) + raid.active_ms / 1000,
        )
        .await?;
        let warned: Vec<_> = raid
            .warned_roles
            .iter()
            .filter_map(|r| r.parse().ok().map(Id::new))
            .collect();
        let mut log = Log::new("🕵 Join Raid Trigger!", ALERT)
            .text(&format!(
                "{YES} TommyBot has identified a weird join pattern."
            ))
            .text(&format!(
                "{ARROW} Flagged accounts joining for the next `{}` will be **{}**.",
                duration(raid.active_ms / 1000),
                match raid.action {
                    Action::Log => "logged",
                    Action::Warn => "warned",
                    Action::Timeout(_) => "timed out",
                    Action::Kick => "kicked",
                    Action::Ban => "banned",
                }
            ))
            .text(&format!(
                "{ARROW} A full report will be posted once the duration ends."
            ))
            .field(
                "Initial Flags:",
                // 40 IDs stay under Discord's 1024-character field limit; the page lists all
                &(out
                    .flagged
                    .iter()
                    .zip(&names)
                    .take(40)
                    .map(|(u, n)| format!("`{} | {n}`", crate::invites::short_id(*u))))
                .chain(
                    (out.flagged.len() > 40).then(|| format!("+{} more", out.flagged.len() - 40)),
                )
                .collect::<Vec<_>>()
                .join("\n"),
            );
        log = log.field("Code:", &format!("`{code}`"));
        if let Some(url) = crate::invites::raid_url(&code) {
            log = log.text(&format!("{ARROW} [Every caught account]({url})"));
        }
        send_log_pinging(app, gid.get(), LogKind::JoinRaid, log, &warned).await?;
        crate::reply::alert_owner(app, gid, "A join raid started").await;
    }
    let auto_lock = cfg.automod.auto_lockdown.enabled;
    if let Some(kind) = (out.triggered && auto_lock)
        .then(|| raid.lock.clone())
        .flatten()
    {
        lockdown::lock(app, gid, kind).await?;
    }
    for id in out.flagged {
        punish(app, cfg, gid, id, raid.action, "join raid").await?;
    }
    Ok(())
}

/// The cured names, one per line, as `GateCfg::name_matches` reads them.
fn names(raw: &[Option<&String>]) -> String {
    raw.iter()
        .flatten()
        .map(|n| filters::cure(n))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Re-runs the username filter when a member renames, if the gate asks for it.
pub async fn on_member_update(app: &App, m: &MemberUpdate) -> Result<()> {
    let cfg = app.cfg(m.guild_id.get()).await?;
    let g = &cfg.joingate;
    let Some(action) = g
        .username_filter
        .get(Filter::UsernameFilter.default_action())
        .filter(|_| g.enabled && g.username_post_join)
    else {
        return Ok(());
    };
    let timed_out = m
        .communication_disabled_until
        .is_some_and(|t| t.as_secs() > now_secs());
    let key = (m.guild_id.get(), m.user.id.get());
    let bad = g.name_matches(&names(&[
        Some(&m.user.name),
        m.user.global_name.as_ref(),
        m.nick.as_ref(),
    ]));
    let skip = {
        let mut seen = app.punished_names.lock().unwrap();
        if !bad {
            seen.remove(&key);
        }
        let names = format!(
            "{}\0{}\0{}",
            m.user.name,
            m.user.global_name.as_deref().unwrap_or_default(),
            m.nick.as_deref().unwrap_or_default()
        );
        // A timeout fires another update and a role change keeps the name: act once per bad name pair.
        timed_out || !bad || seen.insert(key, names.clone()) == Some(names)
    };
    if skip {
        return Ok(());
    }
    punish(
        app,
        &cfg,
        m.guild_id,
        m.user.id.get(),
        action,
        "username filter",
    )
    .await
}

/// A punishment Discord refused (role too low, permission missing) is shown to staff, not
/// only written to the container log.
pub(crate) async fn punish(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: u64,
    action: Action,
    why: &str,
) -> Result<()> {
    let uid = Id::new(user);
    let punishing = !matches!(action, Action::Log | Action::Timeout(..=-1));
    if punishing && cfg.joingate.dm_punished {
        let mut text = if cfg.misc.show_reason_on_auto {
            format!("Automod action: {action:?} ({why})")
        } else {
            format!("Automod action: {action:?}")
        };
        if cfg.joingate.expose_min_age && why.starts_with("join gate") {
            let min = duration(cfg.joingate.min_age_ms() / 1000);
            text.push_str(&format!("\nMinimum account age: `{min}`"));
        }
        let note =
            crate::appeals::note(&cfg.appeals, &cfg.misc.ban_appeal_message, gid).filter(|_| {
                match action {
                    Action::Ban => true,
                    // Short automatic timeouts (5 minutes or less) get no appeal link.
                    Action::Timeout(0) => cfg.misc.timeout_secs() > 300,
                    Action::Timeout(ms) => ms > 300_000,
                    _ => false,
                }
            });
        reply::dm(
            app,
            uid,
            &note.map_or(text.clone(), |n| format!("{text}\n{n}")),
        )
        .await;
    }
    match action {
        Action::Log => {}
        Action::Warn => {
            cases::create(
                &app.pool,
                NewCase {
                    guild_id: gid.get() as i64,
                    kind: "warn",
                    user_id: user as i64,
                    mod_id: app.me.get() as i64,
                    reason: Some(why),
                    points: cfg.misc.warn_base_points,
                    duration_s: None,
                    proof: None,
                    inverse: None,
                },
            )
            .await?;
            warn_actions(app, cfg, gid, uid).await?;
        }
        Action::Timeout(..=-1) => return Ok(()),
        Action::Timeout(_) | Action::Kick | Action::Ban => {
            let done = match action {
                Action::Timeout(ms) => {
                    let secs = match ms {
                        0 => cfg.misc.timeout_secs() as i64,
                        ms => ms / 1000,
                    };
                    timeout(&app.http, gid, uid, secs, "Automod").await
                }
                Action::Kick => app
                    .http
                    .remove_guild_member(gid, uid)
                    .await
                    .map(drop)
                    .map_err(Into::into),
                _ => app
                    .http
                    .create_ban(gid, uid)
                    .await
                    .map(drop)
                    .map_err(Into::into),
            };
            if let Err(e) = done {
                action_failed(app, gid, user, &format!("{action:?}"), &e).await;
                return Err(e);
            }
        }
    }
    send_log(
        app,
        gid.get(),
        LogKind::JoinGate,
        Log::new("Join Gate", ORANGE)
            .user("Member", user)
            .line("Action", &format!("`{action:?}`"))
            .line("Reason", why),
    )
    .await
}
