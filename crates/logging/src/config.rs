use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use twilight_model::{
    guild::audit_log::AuditLogEventType as A,
    id::{
        marker::{ChannelMarker, UserMarker},
        Id,
    },
};

/// Log types. Discord event types and Wick's own log types share one routing table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    Message,
    Member,
    Role,
    Channel,
    Server,
    Invite,
    Webhook,
    Emoji,
    Sticker,
    ScheduledEvent,
    Thread,
    Report,
    AutoMod,
    AntiNuke,
    Verification,
    JoinGate,
    JoinRaid,
    Panic,
    ModCommand,
}

/// A Discord event type that can get its own log channel. Names are the dashboard's option keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogEvent {
    GuildUpdate,
    ChannelCreate,
    ChannelUpdate,
    ChannelDelete,
    ChannelOverwriteCreate,
    ChannelOverwriteUpdate,
    ChannelOverwriteDelete,
    MessagePin,
    MessageUnpin,
    MessageDelete,
    MessageDeleteBulk,
    MessageUpdate,
    MemberKick,
    MemberPrune,
    MemberBanAdd,
    MemberBanRemove,
    MemberUpdate,
    MemberRoleUpdate,
    MemberMove,
    MemberDisconnect,
    BotAdd,
    RoleCreate,
    RoleUpdate,
    RoleDelete,
    InviteCreate,
    InviteUpdate,
    InviteDelete,
    WebhookCreate,
    WebhookUpdate,
    WebhookDelete,
    EmojiCreate,
    EmojiUpdate,
    EmojiDelete,
    StickerCreate,
    StickerUpdate,
    StickerDelete,
    GuildScheduledEventCreate,
    GuildScheduledEventUpdate,
    GuildScheduledEventDelete,
    ThreadCreate,
    ThreadUpdate,
    ThreadDelete,
}

impl LogEvent {
    pub const ALL: [LogEvent; 42] = [
        LogEvent::GuildUpdate,
        LogEvent::ChannelCreate,
        LogEvent::ChannelUpdate,
        LogEvent::ChannelDelete,
        LogEvent::ChannelOverwriteCreate,
        LogEvent::ChannelOverwriteUpdate,
        LogEvent::ChannelOverwriteDelete,
        LogEvent::MessagePin,
        LogEvent::MessageUnpin,
        LogEvent::MessageDelete,
        LogEvent::MessageDeleteBulk,
        LogEvent::MessageUpdate,
        LogEvent::MemberKick,
        LogEvent::MemberPrune,
        LogEvent::MemberBanAdd,
        LogEvent::MemberBanRemove,
        LogEvent::MemberUpdate,
        LogEvent::MemberRoleUpdate,
        LogEvent::MemberMove,
        LogEvent::MemberDisconnect,
        LogEvent::BotAdd,
        LogEvent::RoleCreate,
        LogEvent::RoleUpdate,
        LogEvent::RoleDelete,
        LogEvent::InviteCreate,
        LogEvent::InviteUpdate,
        LogEvent::InviteDelete,
        LogEvent::WebhookCreate,
        LogEvent::WebhookUpdate,
        LogEvent::WebhookDelete,
        LogEvent::EmojiCreate,
        LogEvent::EmojiUpdate,
        LogEvent::EmojiDelete,
        LogEvent::StickerCreate,
        LogEvent::StickerUpdate,
        LogEvent::StickerDelete,
        LogEvent::GuildScheduledEventCreate,
        LogEvent::GuildScheduledEventUpdate,
        LogEvent::GuildScheduledEventDelete,
        LogEvent::ThreadCreate,
        LogEvent::ThreadUpdate,
        LogEvent::ThreadDelete,
    ];

    /// The event of an audit log entry. Message deletes come from the gateway instead.
    pub fn from_action(action: A) -> Option<Self> {
        Some(match action {
            A::GuildUpdate => Self::GuildUpdate,
            A::ChannelCreate => Self::ChannelCreate,
            A::ChannelUpdate => Self::ChannelUpdate,
            A::ChannelDelete => Self::ChannelDelete,
            A::ChannelOverwriteCreate => Self::ChannelOverwriteCreate,
            A::ChannelOverwriteUpdate => Self::ChannelOverwriteUpdate,
            A::ChannelOverwriteDelete => Self::ChannelOverwriteDelete,
            A::MessagePin => Self::MessagePin,
            A::MessageUnpin => Self::MessageUnpin,
            A::MemberKick => Self::MemberKick,
            A::MemberPrune => Self::MemberPrune,
            A::MemberBanAdd => Self::MemberBanAdd,
            A::MemberBanRemove => Self::MemberBanRemove,
            A::MemberUpdate => Self::MemberUpdate,
            A::MemberRoleUpdate => Self::MemberRoleUpdate,
            A::MemberMove => Self::MemberMove,
            A::MemberDisconnect => Self::MemberDisconnect,
            A::BotAdd => Self::BotAdd,
            A::RoleCreate => Self::RoleCreate,
            A::RoleUpdate => Self::RoleUpdate,
            A::RoleDelete => Self::RoleDelete,
            A::InviteCreate => Self::InviteCreate,
            A::InviteUpdate => Self::InviteUpdate,
            A::InviteDelete => Self::InviteDelete,
            A::WebhookCreate => Self::WebhookCreate,
            A::WebhookUpdate => Self::WebhookUpdate,
            A::WebhookDelete => Self::WebhookDelete,
            A::EmojiCreate => Self::EmojiCreate,
            A::EmojiUpdate => Self::EmojiUpdate,
            A::EmojiDelete => Self::EmojiDelete,
            A::StickerCreate => Self::StickerCreate,
            A::StickerUpdate => Self::StickerUpdate,
            A::StickerDelete => Self::StickerDelete,
            A::GuildScheduledEventCreate => Self::GuildScheduledEventCreate,
            A::GuildScheduledEventUpdate => Self::GuildScheduledEventUpdate,
            A::GuildScheduledEventDelete => Self::GuildScheduledEventDelete,
            A::ThreadCreate => Self::ThreadCreate,
            A::ThreadUpdate => Self::ThreadUpdate,
            A::ThreadDelete => Self::ThreadDelete,
            _ => return None,
        })
    }
}

/// A custom logging type: every event in `events` goes to `channel`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomLog {
    pub events: Vec<LogEvent>,
    pub channel: Id<ChannelMarker>,
}

/// Per-guild logging settings, stored as part of the guild config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Receives every log type without its own route (Wick's `#wick-logs`).
    pub fallback: Option<Id<ChannelMarker>>,
    pub routes: HashMap<LogKind, Id<ChannelMarker>>,
    /// One channel per Discord event type.
    pub events: HashMap<LogEvent, Id<ChannelMarker>>,
    /// Custom logging types. The first one that lists an event wins.
    pub custom: Vec<CustomLog>,
    /// Message logs from these channels are dropped.
    pub ignored_channels: HashSet<Id<ChannelMarker>>,
    /// Message and member logs for these users are dropped.
    pub ignored_users: HashSet<Id<UserMarker>>,
}

impl LogConfig {
    /// The channel a log goes to, if any. A custom type beats an event route, which beats a kind route.
    pub fn target(&self, kind: LogKind, event: Option<LogEvent>) -> Option<Id<ChannelMarker>> {
        event
            .and_then(|e| {
                self.custom
                    .iter()
                    .find(|c| c.events.contains(&e))
                    .map(|c| c.channel)
                    .or_else(|| self.events.get(&e).copied())
            })
            .or_else(|| self.routes.get(&kind).copied())
            .or(self.fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_beats_fallback() {
        let fallback = Id::new(1);
        let mut cfg = LogConfig {
            fallback: Some(fallback),
            ..Default::default()
        };
        assert_eq!(cfg.target(LogKind::Role, None), Some(fallback));
        cfg.routes.insert(LogKind::Role, Id::new(2));
        assert_eq!(cfg.target(LogKind::Role, None), Some(Id::new(2)));
        assert_eq!(cfg.target(LogKind::Member, None), Some(fallback));
    }

    #[test]
    fn event_and_custom_beat_kind_route() {
        let mut cfg = LogConfig::default();
        cfg.routes.insert(LogKind::Member, Id::new(2));
        cfg.events.insert(LogEvent::MemberKick, Id::new(3));
        cfg.custom.push(CustomLog {
            events: vec![LogEvent::MemberKick, LogEvent::MemberBanAdd],
            channel: Id::new(4),
        });
        let kick = Some(LogEvent::MemberKick);
        assert_eq!(cfg.target(LogKind::Member, kick), Some(Id::new(4)));
        assert_eq!(
            cfg.target(LogKind::Member, Some(LogEvent::MemberMove)),
            Some(Id::new(2))
        );
        cfg.custom.clear();
        assert_eq!(cfg.target(LogKind::Member, kick), Some(Id::new(3)));
    }

    #[test]
    fn no_route_and_no_fallback_is_none() {
        assert_eq!(LogConfig::default().target(LogKind::Message, None), None);
    }
}
