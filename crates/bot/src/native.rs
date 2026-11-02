//! Bot-side actions for Discord's native AutoMod rules, with Wick-style mention scopes.
use anyhow::Result;
use std::collections::HashMap;
use tommybot_automod::{
    discord_api::{self, BotAction},
    join::Action,
    lockdown::kind_from,
};
use tommybot_core::cases::{self, NewCase};
use tommybot_logging::{embed::ORANGE, Log, LogKind};
use twilight_model::{
    gateway::payload::incoming::AutoModerationActionExecution,
    guild::auto_moderation::AutoModerationActionType,
};

use crate::{
    app::{now_secs, App, GuildCfg},
    automod::warn_actions,
    commands::timeout,
    events::send_log,
    joingate::punish,
    lockdown, reports,
};

/// Recent hits of rules with a Wick-style mention scope, by (guild, rule, member or 0 for the server).
type ScopeHits = std::sync::Mutex<HashMap<(u64, u64, u64), Vec<i64>>>;
static MENTION_HITS: std::sync::LazyLock<ScopeHits> = std::sync::LazyLock::new(Default::default);

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
            tracing::warn!(?err, ?a, "native rule action failed");
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
    if app.whitelisted(gid, uid, "automod").await? {
        return Ok(());
    }
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
        BotAction::Kick => punish(app, cfg, gid, uid.get(), Action::Kick, WHY).await?,
        BotAction::Ban => punish(app, cfg, gid, uid.get(), Action::Ban, WHY).await?,
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
