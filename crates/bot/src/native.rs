//! Bot-side actions for Discord's native AutoMod rules, with mention scopes.
use anyhow::Result;
use std::collections::HashMap;
use tommybot_automod::{
    discord_api::{self, BotAction},
    join::Action,
    lockdown::kind_from,
};
use tommybot_core::cases::{self, NewCase};
use tommybot_core::permits::Level;
use tommybot_logging::{embed::ORANGE, Log, LogKind};
use twilight_model::{
    gateway::payload::incoming::AutoModerationActionExecution,
    guild::{
        audit_log::{AuditLogEntry, AuditLogEventType},
        auto_moderation::AutoModerationActionType,
    },
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::{now_secs, App, GuildCfg},
    automod::warn_actions,
    commands::timeout,
    events::send_log,
    joingate::punish,
    lockdown, reports,
};

/// Recent hits of rules with a mention scope, by (guild, rule, member or 0 for the server).
type ScopeHits = std::sync::Mutex<HashMap<(u64, u64, u64), Vec<i64>>>;
static MENTION_HITS: std::sync::LazyLock<ScopeHits> = std::sync::LazyLock::new(Default::default);

/// Whether the rule was last created or edited by someone without a permit (see [`on_rule_audit`]).
async fn untrusted(app: &App, gid: Id<GuildMarker>, rule: u64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM automod_rule_untrusted
                        WHERE guild_id = $1 AND rule_id = $2 AND NOT trusted)",
    )
    .bind(gid.get() as i64)
    .bind(rule as i64)
    .fetch_one(&app.pool)
    .await?)
}

/// Per (guild, rule): when Discord last sent a rule create or update, and whether its audit entry
/// was handled since. The gateway event and the audit entry come in their own tasks, so hits in
/// between are held: the rule may have just been changed by someone without a permit.
static CHANGED: std::sync::LazyLock<std::sync::Mutex<HashMap<(u64, u64), i64>>> =
    std::sync::LazyLock::new(Default::default);
/// How long a changed rule's hits wait for its audit entry.
const HOLD_MS: i64 = 30_000;

/// The gateway's rule create or update: hits wait for its audit entry (at most [`HOLD_MS`]).
pub(crate) fn rule_changed(guild: u64, rule: u64) {
    let mut m = CHANGED.lock().unwrap();
    let now = crate::automod::now_ms();
    m.retain(|_, t| now - *t < HOLD_MS);
    m.insert((guild, rule), now);
}

fn seen_audit(guild: u64, rule: u64) {
    CHANGED.lock().unwrap().remove(&(guild, rule));
}

fn held(guild: u64, rule: u64) -> bool {
    CHANGED
        .lock()
        .unwrap()
        .get(&(guild, rule))
        .is_some_and(|t| crate::automod::now_ms() - t < HOLD_MS)
}

/// Who may change what a native rule triggers: Owner, EO, TA or a dashboard-edit permit, the same
/// people who set its TommyBot actions.
fn trusted(level: &Level) -> bool {
    level.immune() || level.has("dashboard_edit")
}

/// A rule created or edited by someone who is not [`trusted`] triggers no TommyBot action until a
/// trusted edit (Thomas 2026-11-13 22:49: Discord's Manage Server gives no more power than a
/// member has). Runs once per audit entry, live or replayed, with anti-nuke on or off; the newest
/// entry per rule wins, so a late older one cannot clear a newer verdict.
pub(crate) async fn on_rule_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    use AuditLogEventType as A;
    let (Some(gid), Some(actor), Some(rule)) = (e.guild_id, e.user_id, e.target_id) else {
        return Ok(());
    };
    let (g, r) = (gid.get() as i64, rule.get() as i64);
    match e.action_type {
        A::AutoModerationRuleDelete => {
            sqlx::query("DELETE FROM automod_rule_untrusted WHERE guild_id = $1 AND rule_id = $2")
                .bind(g)
                .bind(r)
                .execute(&app.pool)
                .await?;
        }
        A::AutoModerationRuleCreate | A::AutoModerationRuleUpdate => {
            if actor == app.me {
                // The bot's keyword sync rewrites only part of a rule, and a dashboard edit is
                // trusted by the dashboard itself, so the bot's own entry keeps the verdict.
                sqlx::query(
                    "INSERT INTO automod_rule_untrusted (guild_id, rule_id, audit_id, actor, trusted)
                     VALUES ($1, $2, $3, $4, true)
                     ON CONFLICT (guild_id, rule_id) DO UPDATE
                       SET audit_id = greatest(automod_rule_untrusted.audit_id, $3)",
                )
                .bind(g)
                .bind(r)
                .bind(e.id.get() as i64)
                .bind(actor.get() as i64)
                .execute(&app.pool)
                .await?;
                seen_audit(gid.get(), rule.get());
                return Ok(());
            }
            // A failed lookup counts as untrusted: a trusted edit clears it later.
            let ok = match app.guild_info(gid).await {
                Ok(info) => match app.level(gid, info.owner, actor).await {
                    Ok(level) => trusted(&level),
                    Err(err) => {
                        tracing::warn!(?err, "permit lookup for an AutoMod rule edit failed");
                        false
                    }
                },
                Err(err) => {
                    tracing::warn!(?err, "owner lookup for an AutoMod rule edit failed");
                    false
                }
            };
            let won = sqlx::query(
                "INSERT INTO automod_rule_untrusted (guild_id, rule_id, audit_id, actor, trusted)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (guild_id, rule_id) DO UPDATE
                   SET audit_id = $3, actor = $4, trusted = $5, at = now()
                   WHERE automod_rule_untrusted.audit_id < $3",
            )
            .bind(g)
            .bind(r)
            .bind(e.id.get() as i64)
            .bind(actor.get() as i64)
            .bind(ok)
            .execute(&app.pool)
            .await?
            .rows_affected()
                == 1;
            seen_audit(gid.get(), rule.get());
            if won && !ok {
                let log = Log::new("Discord AutoMod Rule Changed Without A Permit", ORANGE)
                    .user("Responsible", actor.get())
                    .line("Rule", &format!("`{rule}`"))
                    .line(
                        "Action",
                        "TommyBot takes no action on this rule's hits until an Owner, Extra Owner, Trusted Admin or a permit with dashboard edit changes it",
                    );
                send_log(app, gid.get(), LogKind::AutoMod, log).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Bot-side actions for a native rule; the rule's block-message event fires once per hit.
pub async fn on_native_hit(app: &App, e: &AutoModerationActionExecution) -> Result<()> {
    if e.action.kind != AutoModerationActionType::BlockMessage {
        return Ok(());
    }
    let gid = e.guild_id;
    let cfg = app.cfg(gid.get()).await?;
    let Some(actions) = cfg.discord_automod.actions.get(&e.rule_id.to_string()) else {
        return Ok(());
    };
    if held(gid.get(), e.rule_id.get()) || untrusted(app, gid, e.rule_id.get()).await? {
        return Ok(());
    }
    // Before counting: a whitelisted member's hits must not fill a scope for the next member.
    if app.whitelisted(gid, e.user_id, "automod").await? {
        return Ok(());
    }
    if let Some(sc) = cfg.discord_automod.scopes.get(&e.rule_id.to_string()) {
        // Server scope counts everyone together, user scope each member apart.
        let who = if sc.scope == "server" {
            0
        } else {
            e.user_id.get()
        };
        let key = (gid.get(), e.rule_id.get(), who);
        let tripped = {
            let mut hits = MENTION_HITS.lock().unwrap();
            let tripped = discord_api::scope_tripped(hits.entry(key).or_default(), now_secs(), sc);
            if tripped {
                hits.remove(&key);
            }
            tripped
        };
        if !tripped {
            return Ok(());
        }
    }
    for &a in actions {
        if let Err(err) = native_action(app, &cfg, e, a).await {
            crate::automod::action_failed(app, gid, e.user_id.get(), &format!("{a:?}"), &err).await;
        }
    }
    Ok(())
}

async fn native_action(
    app: &App,
    cfg: &GuildCfg,
    e: &AutoModerationActionExecution,
    action: BotAction,
) -> Result<()> {
    const WHY: &str = "discord automod rule";
    let (gid, uid) = (e.guild_id, e.user_id);
    match action {
        BotAction::Report => {
            if reports::file_native(app, gid, e.channel_id, uid, &e.content, e.rule_id).await? {
                return Ok(());
            }
            let log = Log::new("Discord AutoMod Report", ORANGE)
                .user("Member", uid.get())
                .line("Rule", &format!("`{}`", e.rule_id))
                .text(&tommybot_logging::embed::label_head("Content"))
                .diff(Some(&e.content), None);
            send_log(app, gid.get(), LogKind::AutoMod, log).await?;
        }
        BotAction::Warn => {
            cases::create(
                &app.pool,
                NewCase {
                    guild_id: gid.get() as i64,
                    kind: "warn",
                    user_id: uid.get() as i64,
                    mod_id: app.me.get() as i64,
                    reason: Some(WHY),
                    points: cfg.misc.warn_base_points,
                    duration_s: None,
                    proof: None,
                    inverse: None,
                },
            )
            .await?;
            warn_actions(app, cfg, gid, uid).await?;
        }
        BotAction::Timeout => {
            timeout(
                &app.http,
                gid,
                uid,
                cfg.misc.timeout_secs() as i64,
                "Automod",
            )
            .await?
        }
        BotAction::Kick => {
            punish(
                app,
                cfg,
                gid,
                uid.get(),
                Action::Kick,
                WHY,
                Default::default(),
            )
            .await?
        }
        BotAction::Ban => {
            punish(
                app,
                cfg,
                gid,
                uid.get(),
                Action::Ban,
                WHY,
                Default::default(),
            )
            .await?
        }
        BotAction::Lock => {
            let sub = cfg
                .discord_automod
                .locks
                .get(&e.rule_id.to_string())
                .map_or("", String::as_str);
            if sub == "invites" {
                let now = now_secs() * 1000;
                let until = now + discord_api::MAX_INVITE_PAUSE_MS;
                app.api.pause_invites(gid.get(), Some((now, until))).await?;
            } else if sub.is_empty() {
                lockdown::lock(app, gid, cfg.lockdown_prefs.kind()).await?;
            } else if let Some(kind) = kind_from(sub, e.channel_id.map(|c| c.get())) {
                lockdown::lock(app, gid, kind).await?;
            }
        }
    }
    Ok(())
}
