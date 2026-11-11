//! Moderator stats: bans, kicks and timeouts staff do in the Discord app, from the audit log.
//! TommyBot's own actions are recorded as cases (`cases::create`), so they are skipped here.
use anyhow::Result;
use tommybot_core::cases::{self, ModAction};
use twilight_model::guild::audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType as A};

use tommybot_core::permits::Level;
use tommybot_logging::{
    embed::{ALERT, YES},
    Log, LogKind,
};
use twilight_http::request::AuditLogReason;
use twilight_model::id::Id;

use crate::{app::App, events::send_log};

/// The stats kind of an audit log entry, or None when it is not a counted action.
fn kind(e: &AuditLogEntry) -> Option<&'static str> {
    Some(match e.action_type {
        A::MemberBanAdd => "ban",
        A::MemberBanRemove => "unban",
        A::MemberKick => "kick",
        A::MemberUpdate => e.changes.iter().find_map(|c| match c {
            AuditLogChange::CommunicationDisabledUntil { new: Some(_), .. } => Some("timeout"),
            AuditLogChange::CommunicationDisabledUntil {
                new: None,
                old: Some(_),
            } => Some("untimeout"),
            _ => None,
        })?,
        _ => return None,
    })
}

/// Roles added in a member role update that hold View Mod Stats.
fn stats_roles_added(e: &AuditLogEntry, holders: &[i64]) -> Vec<u64> {
    e.changes
        .iter()
        .filter_map(|c| match c {
            AuditLogChange::RoleAdded { new, .. } => Some(new),
            _ => None,
        })
        .flatten()
        .map(|r| r.id.get())
        .filter(|r| holders.contains(&(*r as i64)))
        .collect()
}

/// Only owners choose who sees moderator stats (Thomas 2026-11-11): a View Mod Stats role given in
/// the Discord app by anyone else is taken back and logged.
async fn guard_stats_role(app: &App, e: &AuditLogEntry) -> Result<()> {
    let (Some(gid), Some(actor), Some(target)) = (e.guild_id, e.user_id, e.target_id) else {
        return Ok(());
    };
    if e.action_type != A::MemberRoleUpdate || actor == app.me {
        return Ok(());
    }
    let holders: Vec<i64> = sqlx::query_scalar(
        "SELECT r.role_id FROM permit_roles r JOIN permits p ON p.id = r.permit_id
         WHERE p.guild_id = $1 AND 'mod_stats' = ANY(p.perms)",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    let added = stats_roles_added(e, &holders);
    if added.is_empty() {
        return Ok(());
    }
    let owner = app.guild_info(gid).await?.owner;
    if matches!(
        app.level(gid, owner, actor).await?,
        Level::Owner | Level::ExtraOwner
    ) {
        return Ok(());
    }
    for &r in &added {
        app.http
            .remove_guild_member_role(gid, target.cast(), Id::new(r))
            .reason("Only owners can give View Mod Stats")
            .await?;
    }
    let names: Vec<String> = added.iter().map(|r| format!("<@&{r}>")).collect();
    let log = Log::new("Anti Nuke", ALERT)
        .user("Member", target.get())
        .user("Given by", actor.get())
        .text(&format!("{YES} **View Mod Stats role removed**"))
        .line("Roles", &names.join(", "))
        .line(
            "Reason",
            "only owners can give roles that see moderator stats",
        );
    send_log(app, gid.get(), LogKind::AntiNuke, log).await
}

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
    if let Err(err) = guard_stats_role(app, e).await {
        tracing::warn!(?err, "View Mod Stats role check failed");
    }
    let (Some(gid), Some(actor), Some(kind)) = (e.guild_id, e.user_id, kind(e)) else {
        return Ok(());
    };
    if actor == app.me {
        return Ok(());
    }
    cases::record(
        &mut *app.pool.acquire().await?,
        ModAction {
            guild_id: gid.get() as i64,
            mod_id: actor.get() as i64,
            kind,
            source: "discord",
            target_id: e.target_id.map(|t| t.get() as i64),
            case_no: None,
            audit_id: Some(e.id.get() as i64),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::{id::Id, util::Timestamp};

    fn entry(action_type: A, changes: Vec<AuditLogChange>) -> AuditLogEntry {
        AuditLogEntry {
            action_type,
            changes,
            guild_id: Some(Id::new(1)),
            id: Id::new(2),
            options: None,
            reason: None,
            target_id: Some(Id::new(3)),
            user_id: Some(Id::new(4)),
        }
    }

    #[test]
    fn counted_actions() {
        let t = Timestamp::from_secs(1_800_000_000).unwrap();
        let to = |new, old| AuditLogChange::CommunicationDisabledUntil { new, old };
        assert_eq!(kind(&entry(A::MemberBanAdd, vec![])), Some("ban"));
        assert_eq!(kind(&entry(A::MemberBanRemove, vec![])), Some("unban"));
        assert_eq!(kind(&entry(A::MemberKick, vec![])), Some("kick"));
        assert_eq!(
            kind(&entry(A::MemberUpdate, vec![to(Some(t), None)])),
            Some("timeout")
        );
        assert_eq!(
            kind(&entry(A::MemberUpdate, vec![to(None, Some(t))])),
            Some("untimeout")
        );
        // A nickname change, or a timeout field that did not change, is not an action.
        assert_eq!(kind(&entry(A::MemberUpdate, vec![to(None, None)])), None);
        assert_eq!(
            kind(&entry(
                A::MemberUpdate,
                vec![AuditLogChange::Nick {
                    new: None,
                    old: None
                }]
            )),
            None
        );
        assert_eq!(kind(&entry(A::ChannelDelete, vec![])), None);
    }

    #[test]
    fn only_stats_roles_count_as_added() {
        let role = |id| twilight_model::guild::audit_log::AffectedRole {
            id: Id::new(id),
            name: String::new(),
        };
        let e = entry(
            A::MemberRoleUpdate,
            vec![
                AuditLogChange::RoleAdded {
                    new: vec![role(5), role(6)],
                    old: vec![],
                },
                AuditLogChange::RoleRemoved {
                    new: vec![role(7)],
                    old: vec![],
                },
            ],
        );
        assert_eq!(
            stats_roles_added(&e, &[6, 7]),
            vec![6],
            "a removed role is not a grant"
        );
        assert!(stats_roles_added(&e, &[]).is_empty());
    }
}
