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
    permits::Level,
};
use tommybot_logging::{
    embed::{duration, ALERT, ARROW, DONE, NO, ORANGE, RED, YES},
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

use crate::automod::{now_ms, warn_actions};

pub async fn on_join(app: &App, m: &MemberAdd) -> Result<()> {
    let (gid, user) = (m.guild_id, &m.member.user);
    let cfg = app.cfg(gid.get()).await?;
    let now = now_ms();
    if let Some(lock) = lockdown::join_lock(app, gid).await? {
        let action = match lock {
            JoinLock::Kick => Action::Kick,
            JoinLock::Ban => Action::Ban,
        };
        return punish(
            app,
            &cfg,
            gid,
            user.id.get(),
            action,
            "join lock",
            Detail::default(),
        )
        .await;
    }
    // Before the other checks, so a failed join gate step cannot let a quarantined member back in.
    if let Err(e) = crate::antinuke::requarantine_rejoin(app, gid, user.id).await {
        tracing::warn!(?e, "quarantining a rejoined member failed");
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
    // Only asked when the exception is on: no audit entry, or a failed lookup, means no exception.
    let vouched = match adder {
        Some(a) if cfg.joingate.exclude_trusted_bots => match app.guild_info(gid).await {
            Ok(info) => app
                .level(gid, info.owner, a)
                .await
                .is_ok_and(|l| vouches(&l)),
            Err(_) => false,
        },
        _ => false,
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
        added_by_owner: vouched,
    };
    let suspicious = is_suspicious(&j, &cfg.joingate, cfg.joingate.min_age_ms(), now);
    let timeout = m.member.communication_disabled_until.map(|t| t.as_secs());
    verify::on_join(app, gid, user.id, suspicious, user.bot, timeout).await;
    let hits = gate(&j, &cfg.joingate, now);
    if let Some(action) = harshest(&hits) {
        let why = if hits.iter().any(|h| h.0 == Filter::AccountAge) {
            let min = duration(cfg.joingate.min_age_ms() / 1000);
            format!("join gate: accounts must be at least {min} old")
        } else {
            "join gate".into()
        };
        let filters: Vec<&str> = hits.iter().map(|h| h.0.name()).collect();
        let detail = Detail {
            filters: &filters,
            is_bot: user.bot,
            adder: adder.map(|a| a.get()),
        };
        punish(app, &cfg, gid, j.id, action, &why, detail).await?;
        // The bot filter also punishes whoever added it.
        if let (Some(adder), true) = (adder, hits.iter().any(|h| h.0 == Filter::UnauthorizedBot)) {
            punish(
                app,
                &cfg,
                gid,
                adder.get(),
                action,
                "adding a bot without permission",
                Detail::default(),
            )
            .await?;
        }
    }
    if let Some(raid) = cfg.joinraid.as_ref().filter(|r| r.enabled) {
        join_raid(app, &cfg, gid, raid, &j, suspicious, now).await?;
    }
    Ok(())
}

/// The alert when a join raid starts: what happens next and the first flagged accounts.
pub(crate) fn raid_trigger_log(
    raid: &RaidCfg,
    flagged: &[(u64, &str)],
    code: &str,
    url: Option<String>,
) -> Log {
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
            &(flagged
                .iter()
                .take(40)
                .map(|(u, n)| format!("`{} | {n}`", crate::invites::short_id(*u))))
            .chain((flagged.len() > 40).then(|| format!("+{} more", flagged.len() - 40)))
            .collect::<Vec<_>>()
            .join("\n"),
        );
    log = log.field("Code:", &format!("`{code}`"));
    if let Some(url) = url {
        log = log.text(&format!("{ARROW} [Every caught account]({url})"));
    }
    log
}

/// The report when a join raid ends; `code` names the raid when the full report has a link.
pub(crate) fn raid_report_log(caught: i64, code: Option<&str>) -> Log {
    let log = Log::new("🕵 Join Raid Report!", DONE).text(&format!(
        "{YES} `{caught}` users have been caught by the join raid."
    ));
    match code {
        Some(code) => log.text(&format!("{ARROW} `{code}`")),
        None => log,
    }
}

/// The link button under a raid report.
pub(crate) const RAID_REPORT: &str = "Full Raid Report ↗";

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
        let flagged: Vec<(u64, &str)> = out
            .flagged
            .iter()
            .copied()
            .zip(names.iter().map(String::as_str))
            .collect();
        let log = raid_trigger_log(raid, &flagged, &code, crate::invites::raid_url(&code));
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
        punish(
            app,
            cfg,
            gid,
            id,
            raid.action,
            "join raid",
            Detail::default(),
        )
        .await?;
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
    // Owner, Extra Owners and Trusted Admins are immune to every bot action, here too.
    if skip || app.is_immune(m.guild_id, m.user.id).await? {
        return Ok(());
    }
    punish(
        app,
        &cfg,
        m.guild_id,
        m.user.id.get(),
        action,
        "username filter",
        Detail::default(),
    )
    .await
}

/// Bots added by the owner or an Extra Owner may skip the account filters; Trusted Admins and
/// other admins do not count, so one taken-over admin account cannot let a bot in.
fn vouches(level: &Level) -> bool {
    matches!(level, Level::Owner | Level::ExtraOwner)
}

/// What the Join Gate log and records add to the reason: the filters that matched and, for a
/// bot, who added it.
#[derive(Default, Clone, Copy)]
pub(crate) struct Detail<'a> {
    pub filters: &'a [&'a str],
    pub is_bot: bool,
    pub adder: Option<u64>,
}

/// The Join Gate log of one punishment; `failed` is Discord's refusal.
pub(crate) fn gate_log(
    user: u64,
    action: Action,
    why: &str,
    d: Detail,
    failed: Option<&str>,
) -> Log {
    let mut log = Log::new("Join Gate", if failed.is_some() { RED } else { ORANGE })
        .user("Member", user)
        .line("Action", &format!("`{action:?}`"))
        .line("Reason", why);
    if !d.filters.is_empty() {
        log = log.line("Filters", &d.filters.join(", "));
    }
    if d.is_bot {
        let by = d
            .adder
            .map_or("an unknown member".into(), |a| format!("<@{a}>"));
        log = log.line("Bot", &format!("Yes, added by {by}"));
    }
    match failed {
        Some(e) => log.text(&format!("{NO} **Failed:** {e}")),
        None => log,
    }
}

/// Saves the punishment for the dashboard's Join Gate records.
async fn record(
    app: &App,
    gid: Id<GuildMarker>,
    user: u64,
    (action, why, d): (Action, &str, Detail<'_>),
    failed: Option<&str>,
) {
    let saved = sqlx::query(
        "INSERT INTO joingate_actions (guild_id, user_id, is_bot, adder_id, filters, reason, action, ok, error)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(gid.get() as i64)
    .bind(user as i64)
    .bind(d.is_bot)
    .bind(d.adder.map(|a| a as i64))
    .bind(serde_json::json!(d.filters))
    .bind(why)
    .bind(format!("{action:?}"))
    .bind(failed.is_none())
    .bind(failed)
    .execute(&app.pool)
    .await;
    if let Err(e) = saved {
        tracing::warn!(?e, "saving the join gate record failed");
    }
}

/// With no log channel at all, the owner hears once that Join Gate acts without a log.
async fn no_log_channel(app: &App, gid: Id<GuildMarker>) {
    static TOLD: std::sync::Mutex<std::collections::BTreeSet<u64>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    if TOLD.lock().unwrap().insert(gid.get()) {
        reply::dm_owner(app, gid, "TommyBot's Join Gate punished a member, but your server has no log channel, so only the Join Gate Log on the dashboard shows it. Pick a log channel on the dashboard.").await;
    }
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
    detail: Detail<'_>,
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
                // Logged like a success, so a refused kick still shows up in the Join Gate log.
                let reason = crate::http_err::why(&e);
                finish(app, cfg, gid, user, (action, why, detail), Some(&reason)).await?;
                return Err(e);
            }
        }
    }
    finish(app, cfg, gid, user, (action, why, detail), None).await
}

/// Records and logs a punishment, done or `failed`.
async fn finish(
    app: &App,
    cfg: &GuildCfg,
    gid: Id<GuildMarker>,
    user: u64,
    what: (Action, &str, Detail<'_>),
    failed: Option<&str>,
) -> Result<()> {
    record(app, gid, user, what, failed).await;
    if cfg.logging.target(LogKind::JoinGate, None).is_none() {
        no_log_channel(app, gid).await;
    }
    let (action, why, detail) = what;
    send_log(
        app,
        gid.get(),
        LogKind::JoinGate,
        gate_log(user, action, why, detail, failed),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_and_extra_owners_vouch_for_bots() {
        assert!(vouches(&Level::Owner));
        assert!(vouches(&Level::ExtraOwner));
        assert!(!vouches(&Level::TrustedAdmin));
        assert!(!vouches(&Level::None));
    }

    #[test]
    fn gate_log_names_filters_bot_and_failure() {
        let d = Detail {
            filters: &["No Avatar", "Suspicious Account"],
            is_bot: true,
            adder: Some(5),
        };
        let e = gate_log(9, Action::Kick, "join gate", d, Some("Missing Permissions")).build();
        let text = serde_json::to_string(&e).unwrap();
        for part in [
            "No Avatar, Suspicious Account",
            "added by <@5>",
            "Missing Permissions",
            "Kick",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert_eq!(e.color, Some(RED));
        let human = gate_log(9, Action::Ban, "join raid", Detail::default(), None).build();
        let text = serde_json::to_string(&human).unwrap();
        assert!(!text.contains("Filters") && !text.contains("Bot") && !text.contains("Failed"));
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn records_keep_the_newest_thousand() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let app = App::new(
            twilight_http::Client::new(String::new()),
            twilight_http::Client::new(String::new()),
            pool,
            tommybot_automod::discord_api::Api::new(""),
            Id::new(1),
        );
        let gid = Id::new(6_100_000_000 + std::process::id() as u64);
        let g = gid.get() as i64;
        // 1000 old-enough rows in one go, then a row older than 30 days.
        sqlx::query("INSERT INTO joingate_actions (guild_id, user_id, reason, action, ok) SELECT $1, n, 'x', 'Kick', true FROM generate_series(1, 1000) n")
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE joingate_actions SET created_at = now() - interval '31 days' WHERE guild_id = $1 AND user_id = 500")
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
        let d = Detail {
            filters: &["No Avatar"],
            is_bot: true,
            adder: Some(7),
        };
        record(
            &app,
            gid,
            2000,
            (Action::Kick, "join gate", d),
            Some("Missing Permissions"),
        )
        .await;
        let users: Vec<i64> = sqlx::query_scalar(
            "SELECT user_id FROM joingate_actions WHERE guild_id = $1 ORDER BY id",
        )
        .bind(g)
        .fetch_all(&app.pool)
        .await
        .unwrap();
        // The oldest row and the 31-day-old one are gone, the new one is in.
        assert_eq!(users.len(), 999);
        assert!(!users.contains(&1) && !users.contains(&500) && users.contains(&2000));
        let (filters, ok, error): (serde_json::Value, bool, Option<String>) = sqlx::query_as(
            "SELECT filters, ok, error FROM joingate_actions WHERE guild_id = $1 AND user_id = 2000",
        )
        .bind(g)
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(
            (filters, ok, error.as_deref()),
            (
                serde_json::json!(["No Avatar"]),
                false,
                Some("Missing Permissions")
            )
        );
        sqlx::query("DELETE FROM joingate_actions WHERE guild_id = $1")
            .bind(g)
            .execute(&app.pool)
            .await
            .unwrap();
    }
}
