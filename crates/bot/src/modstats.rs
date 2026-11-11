//! Moderator stats: bans, kicks and timeouts staff do in the Discord app, from the audit log.
//! TommyBot's own actions are recorded as cases (`cases::create`), so they are skipped here.
use anyhow::Result;
use tommybot_core::cases::{self, ModAction};
use twilight_model::guild::audit_log::{
    AuditLogChange, AuditLogEntry, AuditLogEventType as A,
};

use crate::app::App;

/// The stats kind of an audit log entry, or None when it is not a counted action.
fn kind(e: &AuditLogEntry) -> Option<&'static str> {
    Some(match e.action_type {
        A::MemberBanAdd => "ban",
        A::MemberBanRemove => "unban",
        A::MemberKick => "kick",
        A::MemberUpdate => e.changes.iter().find_map(|c| match c {
            AuditLogChange::CommunicationDisabledUntil { new: Some(_), .. } => Some("timeout"),
            AuditLogChange::CommunicationDisabledUntil { new: None, old: Some(_) } => {
                Some("untimeout")
            }
            _ => None,
        })?,
        _ => return None,
    })
}

pub async fn on_audit(app: &App, e: &AuditLogEntry) -> Result<()> {
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
        assert_eq!(kind(&entry(A::MemberUpdate, vec![to(Some(t), None)])), Some("timeout"));
        assert_eq!(kind(&entry(A::MemberUpdate, vec![to(None, Some(t))])), Some("untimeout"));
        // A nickname change, or a timeout field that did not change, is not an action.
        assert_eq!(kind(&entry(A::MemberUpdate, vec![to(None, None)])), None);
        assert_eq!(
            kind(&entry(A::MemberUpdate, vec![AuditLogChange::Nick { new: None, old: None }])),
            None
        );
        assert_eq!(kind(&entry(A::ChannelDelete, vec![])), None);
    }
}
