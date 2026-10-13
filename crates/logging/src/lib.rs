//! Event logging. The bot passes every gateway event of a guild to [`Logger::handle`] with that
//! guild's [`LogConfig`] and sends the returned [`LogMessage`]s. Send them with allowed mentions
//! set to none, since logged user text must not ping anyone.

mod cache;
mod config;
mod render;

use cache::{CachedMessage, MessageCache};
pub use config::{LogConfig, LogKind};

use std::time::Instant;
use twilight_model::{
    gateway::event::Event,
    id::{
        marker::{ChannelMarker, UserMarker},
        Id,
    },
};

/// Discord rejects message content longer than this.
const MAX_CHARS: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMessage {
    pub channel_id: Id<ChannelMarker>,
    pub kind: LogKind,
    pub content: String,
}

pub(crate) struct Entry {
    kind: LogKind,
    channel: Option<Id<ChannelMarker>>,
    user: Option<Id<UserMarker>>,
    text: String,
}

#[derive(Default)]
pub struct Logger {
    cache: MessageCache,
}

impl Logger {
    /// Returns the logs to send for one event. Call it for every event of a guild, even when
    /// logging is off, so the message cache can serve edits and deletes.
    pub fn handle(&mut self, cfg: &LogConfig, event: &Event, now: Instant) -> Vec<LogMessage> {
        let Some(entry) = self.entry(event, now) else {
            return Vec::new();
        };
        let ignored = entry
            .channel
            .is_some_and(|c| cfg.ignored_channels.contains(&c))
            || entry.user.is_some_and(|u| cfg.ignored_users.contains(&u));
        match cfg.target(entry.kind) {
            Some(channel_id) if !ignored => vec![LogMessage {
                channel_id,
                kind: entry.kind,
                content: clamp(entry.text),
            }],
            _ => Vec::new(),
        }
    }

    fn entry(&mut self, event: &Event, now: Instant) -> Option<Entry> {
        match event {
            Event::MessageCreate(m) => {
                if let Some(guild) = m.guild_id {
                    let cached = CachedMessage::new(m.author.id, m.content.clone(), now);
                    self.cache.insert(guild, m.id, cached, now);
                }
                None
            }
            Event::MessageUpdate(m) => {
                let msg = &m.0;
                let guild = msg.guild_id?;
                let old = self
                    .cache
                    .get(guild, msg.id, now)
                    .map(|c| c.content.clone());
                let cached = CachedMessage::new(msg.author.id, msg.content.clone(), now);
                self.cache.insert(guild, msg.id, cached, now);
                // Embed-only updates keep the content and are not worth a log.
                if old.as_deref() == Some(msg.content.as_str()) {
                    return None;
                }
                Some(Entry {
                    kind: LogKind::Message,
                    channel: Some(msg.channel_id),
                    user: Some(msg.author.id),
                    text: render::message_edit(old.as_deref(), msg),
                })
            }
            Event::MessageDelete(m) => {
                let cached = self.cache.remove(m.guild_id?, m.id);
                Some(Entry {
                    kind: LogKind::Message,
                    channel: Some(m.channel_id),
                    user: cached.as_ref().map(|c| c.author),
                    text: render::message_delete(m.id, m.channel_id, cached.as_ref()),
                })
            }
            Event::MessageDeleteBulk(m) => Some(Entry {
                kind: LogKind::Message,
                channel: Some(m.channel_id),
                user: None,
                text: render::bulk_delete(m.channel_id, m.ids.len()),
            }),
            Event::MemberAdd(m) => Some(Entry {
                kind: LogKind::Member,
                channel: None,
                user: Some(m.member.user.id),
                text: render::member_add(&m.member.user),
            }),
            Event::MemberRemove(m) => Some(Entry {
                kind: LogKind::Member,
                channel: None,
                user: Some(m.user.id),
                text: render::member_remove(&m.user),
            }),
            Event::GuildAuditLogEntryCreate(e) => render::audit(&e.0),
            _ => None,
        }
    }
}

/// Keeps the first `MAX_CHARS` characters.
fn clamp(s: String) -> String {
    if s.chars().count() <= MAX_CHARS {
        return s;
    }
    s.chars().take(MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::{
        gateway::payload::incoming::GuildAuditLogEntryCreate, guild::audit_log::AuditLogEntry,
    };

    #[test]
    fn audit_entry_routes_to_role_log_with_changes() {
        let entry: AuditLogEntry = serde_json::from_value(serde_json::json!({
            "action_type": 31,
            "id": "1",
            "user_id": "10",
            "target_id": "20",
            "reason": "cleanup",
            "changes": [{ "key": "name", "old_value": "mods", "new_value": "staff" }]
        }))
        .unwrap();
        let event = Event::GuildAuditLogEntryCreate(Box::new(GuildAuditLogEntryCreate(entry)));

        let cfg = LogConfig {
            fallback: Some(Id::new(5)),
            ..Default::default()
        };
        let out = Logger::default().handle(&cfg, &event, Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel_id, Id::new(5));
        assert_eq!(out[0].kind, LogKind::Role);
        assert!(out[0].content.contains("- name: mods → staff"));
        assert!(out[0].content.contains("Reason: cleanup"));
    }

    #[test]
    fn unrouted_log_is_dropped() {
        let entry: AuditLogEntry = serde_json::from_value(serde_json::json!({
            "action_type": 22, "id": "1", "user_id": "10", "target_id": "20", "changes": []
        }))
        .unwrap();
        let event = Event::GuildAuditLogEntryCreate(Box::new(GuildAuditLogEntryCreate(entry)));
        let cfg = LogConfig::default();
        assert!(Logger::default()
            .handle(&cfg, &event, Instant::now())
            .is_empty());
    }

    #[test]
    fn clamp_limits_characters() {
        assert_eq!(clamp("é".repeat(MAX_CHARS + 5)).chars().count(), MAX_CHARS);
    }
}
