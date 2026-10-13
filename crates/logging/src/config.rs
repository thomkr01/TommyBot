use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use twilight_model::id::{
    marker::{ChannelMarker, UserMarker},
    Id,
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

/// Per-guild logging settings, stored as part of the guild config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Receives every log type without its own route (Wick's `#wick-logs`).
    pub fallback: Option<Id<ChannelMarker>>,
    pub routes: HashMap<LogKind, Id<ChannelMarker>>,
    /// Message logs from these channels are dropped.
    pub ignored_channels: HashSet<Id<ChannelMarker>>,
    /// Message and member logs for these users are dropped.
    pub ignored_users: HashSet<Id<UserMarker>>,
}

impl LogConfig {
    /// The channel a log of this kind goes to, if any.
    pub fn target(&self, kind: LogKind) -> Option<Id<ChannelMarker>> {
        self.routes.get(&kind).copied().or(self.fallback)
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
        assert_eq!(cfg.target(LogKind::Role), Some(fallback));
        cfg.routes.insert(LogKind::Role, Id::new(2));
        assert_eq!(cfg.target(LogKind::Role), Some(Id::new(2)));
        assert_eq!(cfg.target(LogKind::Member), Some(fallback));
    }

    #[test]
    fn no_route_and_no_fallback_is_none() {
        assert_eq!(LogConfig::default().target(LogKind::Message), None);
    }
}
