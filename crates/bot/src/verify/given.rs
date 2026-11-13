//! A verification role given by another bot (D58): while TommyBot's verification is on, the role
//! is taken back and nobody is punished, as the quarantine hold does for a join quarantine (#170).
//! Switching verification off lets another bot verify instead.

use crate::{app::App, events::send_log};
use anyhow::Result;
use twilight_http::request::AuditLogReason;
use tommybot_logging::{
    embed::{NO, ORANGE},
    Log, LogKind,
};
use twilight_model::{
    guild::audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType as A},
    id::{marker::UserMarker, Id},
};

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (A::MemberRoleUpdate, Some(gid), Some(actor), Some(target)) =
        (e.action_type, e.guild_id, e.user_id, e.target_id)
    else {
        return Ok(());
    };
    if actor == app.me {
        return Ok(());
    }
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    let added: Vec<_> = e
        .changes
        .iter()
        .filter_map(|c| match c {
            AuditLogChange::RoleAdded { new, .. } => Some(new),
            _ => None,
        })
        .flatten()
        .map(|r| r.id)
        .filter(|r| v.roles.contains(r))
        .collect();
    if !v.enabled || added.is_empty() {
        return Ok(());
    }
    let target: Id<UserMarker> = target.cast();
    // The quarantine hold already takes back every role a held member is given.
    if cfg.antinuke.enabled && crate::antinuke::quarantined(app, gid, target).await? {
        return Ok(());
    }
    if !crate::antinuke::is_bot(app, actor).await {
        return Ok(());
    }
    let Some(member) = crate::http_err::member(&app.http, gid, target).await? else {
        return Ok(());
    };
    // Already verified (holds another verification role): nothing was skipped.
    if member
        .roles
        .iter()
        .any(|r| v.roles.contains(r) && !added.contains(r))
    {
        return Ok(());
    }
    for r in &added {
        app.http
            .remove_guild_member_role(gid, target, *r)
            .reason("Verification role given by another bot")
            .await?;
    }
    let log = Log::new("Verification", ORANGE)
        .user("Member", target.get())
        .user("Bot", actor.get())
        .text(&format!(
            "{NO} **A verification role given by another bot was taken back**"
        ))
        .line(
            "Why",
            "TommyBot's verification is on, so members verify through it. Turn verification off to let another bot verify.",
        );
    if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
        tracing::warn!(?e, "verification role log failed");
    }
    Ok(())
}
