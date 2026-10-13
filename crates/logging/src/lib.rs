//! Event logging. The bot passes every gateway event of a guild to [`Logger::handle`] with that
//! guild's [`LogConfig`] and sends the returned [`LogMessage`]s. Send them with allowed mentions
//! set to none, since logged user text must not ping anyone.

mod cache;
mod config;
mod render;

use cache::{CachedMessage, MessageCache};
pub use config::{LogConfig, LogKind};

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use twilight_model::{
    gateway::event::Event,
    guild::audit_log::{AuditLogEntry, AuditLogEventType},
    id::{
        marker::{ChannelMarker, UserMarker},
        Id,
    },
};

/// Discord rejects message content longer than this.
const MAX_CHARS: usize = 2000;
/// How long a moderator's message delete is kept to name the deleter of a gateway delete.
const DELETER_WINDOW: Duration = Duration::from_secs(60);
const MAX_DELETERS: usize = 1000;

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

/// One audit log message delete: `executor` deleted a message by `target` in `channel`.
struct AuditDelete {
    at: Instant,
    executor: Id<UserMarker>,
    target: Id<UserMarker>,
    channel: Id<ChannelMarker>,
}

pub struct Logger {
    /// The bot's own user id. Its kicks and bans are already logged as member leaves.
    me: Id<UserMarker>,
    cache: MessageCache,
    deletes: VecDeque<AuditDelete>,
}

impl Logger {
    pub fn new(me: Id<UserMarker>) -> Self {
        Self {
            me,
            cache: MessageCache::default(),
            deletes: VecDeque::new(),
        }
    }

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
                let deleter = cached
                    .as_ref()
                    .and_then(|c| self.deleter(c.author, m.channel_id, now));
                Some(Entry {
                    kind: LogKind::Message,
                    channel: Some(m.channel_id),
                    user: cached.as_ref().map(|c| c.author),
                    text: render::message_delete(m.id, m.channel_id, cached.as_ref(), deleter),
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
            Event::GuildAuditLogEntryCreate(e) => self.audit(&e.0, now),
            _ => None,
        }
    }

    fn audit(&mut self, e: &AuditLogEntry, now: Instant) -> Option<Entry> {
        match e.action_type {
            // shortcut: staff-run kicks and bans still log twice (leave plus audit entry), since
            // Discord sends no audit entry for a plain leave to merge with.
            AuditLogEventType::MemberKick | AuditLogEventType::MemberBanAdd
                if e.user_id == Some(self.me) =>
            {
                None
            }
            AuditLogEventType::MessageDelete => {
                self.remember_delete(e, now);
                None
            }
            _ => render::audit(e),
        }
    }

    /// Audit entries carry the executor but no content. The gateway delete comes with the
    /// content but no executor, so the two are joined on author and channel.
    fn remember_delete(&mut self, e: &AuditLogEntry, now: Instant) {
        let (Some(executor), Some(target), Some(channel)) = (
            e.user_id,
            e.target_id,
            e.options.as_ref().and_then(|o| o.channel_id),
        ) else {
            return;
        };
        while self.deletes.len() >= MAX_DELETERS
            || self
                .deletes
                .front()
                .is_some_and(|d| now.duration_since(d.at) >= DELETER_WINDOW)
        {
            self.deletes.pop_front();
        }
        self.deletes.push_back(AuditDelete {
            at: now,
            executor,
            target: target.cast(),
            channel,
        });
    }

    fn deleter(
        &self,
        author: Id<UserMarker>,
        channel: Id<ChannelMarker>,
        now: Instant,
    ) -> Option<Id<UserMarker>> {
        self.deletes
            .iter()
            .rev()
            .find(|d| {
                d.target == author
                    && d.channel == channel
                    && now.duration_since(d.at) < DELETER_WINDOW
            })
            .map(|d| d.executor)
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
        let out = Logger::new(Id::new(1)).handle(&cfg, &event, Instant::now());
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
        assert!(Logger::new(Id::new(1))
            .handle(&cfg, &event, Instant::now())
            .is_empty());
    }

    fn audit_event(action: u16, executor: u64) -> Event {
        let entry: AuditLogEntry = serde_json::from_value(serde_json::json!({
            "action_type": action, "id": "1", "user_id": executor.to_string(),
            "target_id": "20", "options": { "channel_id": "30" }, "changes": []
        }))
        .unwrap();
        Event::GuildAuditLogEntryCreate(Box::new(GuildAuditLogEntryCreate(entry)))
    }

    #[test]
    fn bot_kick_is_not_logged_twice() {
        let cfg = LogConfig {
            fallback: Some(Id::new(5)),
            ..Default::default()
        };
        let now = Instant::now();
        assert!(Logger::new(Id::new(1))
            .handle(&cfg, &audit_event(20, 1), now)
            .is_empty());
        assert_eq!(
            Logger::new(Id::new(1))
                .handle(&cfg, &audit_event(20, 10), now)
                .len(),
            1
        );
    }

    #[test]
    fn moderator_delete_names_the_deleter_within_window() {
        let mut logger = Logger::new(Id::new(1));
        let now = Instant::now();
        logger.handle(&LogConfig::default(), &audit_event(72, 10), now);
        assert_eq!(
            logger.deleter(Id::new(20), Id::new(30), now),
            Some(Id::new(10))
        );
        assert_eq!(logger.deleter(Id::new(20), Id::new(31), now), None);
        assert_eq!(logger.deleter(Id::new(21), Id::new(30), now), None);
        assert_eq!(
            logger.deleter(Id::new(20), Id::new(30), now + DELETER_WINDOW),
            None
        );
    }

    #[test]
    fn clamp_limits_characters() {
        assert_eq!(clamp("é".repeat(MAX_CHARS + 5)).chars().count(), MAX_CHARS);
    }
}
