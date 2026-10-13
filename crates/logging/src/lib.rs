//! Event logging. The bot passes every gateway event of a guild to [`Logger::handle`] with that
//! guild's [`LogConfig`] and sends the returned [`LogMessage`]s. Send them with allowed mentions
//! set to none, since logged user text must not ping anyone.

mod cache;
mod config;
mod render;

use cache::{CachedMessage, MessageCache};
pub use config::{CustomLog, LogConfig, LogEvent, LogKind};

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use twilight_model::{
    gateway::event::Event,
    guild::audit_log::{AuditLogEntry, AuditLogEventType},
    id::{
        marker::{ChannelMarker, MessageMarker, UserMarker},
        Id,
    },
};

/// Discord rejects message content longer than this.
const MAX_CHARS: usize = 2000;
/// How long a moderator's message delete is kept to name the deleter of a gateway delete.
const DELETER_WINDOW: Duration = Duration::from_secs(60);
/// Caps the audit delete and removal lists.
const MAX_RECENT: usize = 1000;
/// A delete log waits this long so its audit entry, which can arrive after the delete, is seen.
const HOLD: Duration = Duration::from_secs(10);
/// A kick or ban audit entry suppresses the member leave for the same user within this window.
const REMOVAL_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMessage {
    pub channel_id: Id<ChannelMarker>,
    pub kind: LogKind,
    pub content: String,
}

pub(crate) struct Entry {
    kind: LogKind,
    event: Option<LogEvent>,
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

/// A message delete waiting for its audit entry.
struct HeldDelete {
    at: Instant,
    log: Id<ChannelMarker>,
    id: Id<MessageMarker>,
    channel: Id<ChannelMarker>,
    cached: Option<CachedMessage>,
}

pub struct Logger {
    /// The bot's own user id. Its kicks and bans show only as member leaves.
    me: Id<UserMarker>,
    cache: MessageCache,
    deletes: VecDeque<AuditDelete>,
    /// Users removed by a staff kick or ban, from the audit log.
    removals: VecDeque<(Instant, Id<UserMarker>)>,
    held: VecDeque<HeldDelete>,
}

impl Logger {
    pub fn new(me: Id<UserMarker>) -> Self {
        Self {
            me,
            cache: MessageCache::default(),
            deletes: VecDeque::new(),
            removals: VecDeque::new(),
            held: VecDeque::new(),
        }
    }

    /// Returns the logs to send for one event. Call it for every event of a guild, even when
    /// logging is off, so the message cache can serve edits and deletes.
    pub fn handle(&mut self, cfg: &LogConfig, event: &Event, now: Instant) -> Vec<LogMessage> {
        let mut out = self.flush(now);
        if let Event::MessageDelete(m) = event {
            let Some(guild) = m.guild_id else {
                return out;
            };
            let cached = self.cache.remove(guild, m.id);
            let user = cached.as_ref().map(|c| c.author);
            if let Some(log) = cfg.target(LogKind::Message, Some(LogEvent::MessageDelete)) {
                if !ignored(cfg, Some(m.channel_id), user) {
                    self.held.push_back(HeldDelete {
                        at: now,
                        log,
                        id: m.id,
                        channel: m.channel_id,
                        cached,
                    });
                }
            }
            return out;
        }
        let Some(entry) = self.entry(event, now) else {
            return out;
        };
        if let Some(channel_id) = cfg.target(entry.kind, entry.event) {
            if !ignored(cfg, entry.channel, entry.user) {
                out.push(LogMessage {
                    channel_id,
                    kind: entry.kind,
                    content: clamp(entry.text),
                });
            }
        }
        out
    }

    /// Returns the held delete logs that are due. Called from `handle`, and by the bot on a timer
    /// so a quiet guild still gets its logs.
    pub fn flush(&mut self, now: Instant) -> Vec<LogMessage> {
        let mut out = Vec::new();
        while self
            .held
            .front()
            .is_some_and(|h| now.duration_since(h.at) >= HOLD)
        {
            let Some(h) = self.held.pop_front() else {
                break;
            };
            let deleter = h
                .cached
                .as_ref()
                .and_then(|c| self.deleter(c.author, h.channel, now));
            out.push(LogMessage {
                channel_id: h.log,
                kind: LogKind::Message,
                content: clamp(render::message_delete(
                    h.id,
                    h.channel,
                    h.cached.as_ref(),
                    deleter,
                )),
            });
        }
        out
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
                    event: Some(LogEvent::MessageUpdate),
                    channel: Some(msg.channel_id),
                    user: Some(msg.author.id),
                    text: render::message_edit(old.as_deref(), msg),
                })
            }
            Event::MessageDeleteBulk(m) => Some(Entry {
                kind: LogKind::Message,
                event: Some(LogEvent::MessageDeleteBulk),
                channel: Some(m.channel_id),
                user: None,
                text: render::bulk_delete(m.channel_id, m.ids.len()),
            }),
            Event::MemberAdd(m) => Some(Entry {
                kind: LogKind::Member,
                event: None,
                channel: None,
                user: Some(m.member.user.id),
                text: render::member_add(&m.member.user),
            }),
            Event::MemberRemove(m) => {
                if self.removed_by_audit(m.user.id, now) {
                    return None;
                }
                Some(Entry {
                    kind: LogKind::Member,
                    event: None,
                    channel: None,
                    user: Some(m.user.id),
                    text: render::member_remove(&m.user),
                })
            }
            Event::GuildAuditLogEntryCreate(e) => self.audit(&e.0, now),
            _ => None,
        }
    }

    fn audit(&mut self, e: &AuditLogEntry, now: Instant) -> Option<Entry> {
        match e.action_type {
            AuditLogEventType::MemberKick | AuditLogEventType::MemberBanAdd
                if e.user_id == Some(self.me) =>
            {
                None
            }
            // shortcut: if Discord sends the member leave before this entry, a staff kick or ban logs twice. Needs a live check.
            AuditLogEventType::MemberKick | AuditLogEventType::MemberBanAdd => {
                if let Some(user) = e.target_id {
                    while self
                        .removals
                        .front()
                        .is_some_and(|(at, _)| now.duration_since(*at) >= REMOVAL_WINDOW)
                        || self.removals.len() >= MAX_RECENT
                    {
                        self.removals.pop_front();
                    }
                    self.removals.push_back((now, user.cast()));
                }
                render::audit(e)
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
        while self.deletes.len() >= MAX_RECENT
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

    fn removed_by_audit(&self, user: Id<UserMarker>, now: Instant) -> bool {
        self.removals
            .iter()
            .any(|(at, u)| *u == user && now.duration_since(*at) < REMOVAL_WINDOW)
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

/// Whether a log for this channel or user is on an ignore list.
fn ignored(
    cfg: &LogConfig,
    channel: Option<Id<ChannelMarker>>,
    user: Option<Id<UserMarker>>,
) -> bool {
    channel.is_some_and(|c| cfg.ignored_channels.contains(&c))
        || user.is_some_and(|u| cfg.ignored_users.contains(&u))
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

    fn user(id: u64) -> twilight_model::user::User {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "username": "u", "discriminator": 0, "avatar": null, "bot": false
        }))
        .unwrap()
    }

    #[test]
    fn staff_kick_logs_once_as_audit_entry() {
        let cfg = LogConfig {
            fallback: Some(Id::new(5)),
            ..Default::default()
        };
        let now = Instant::now();
        let mut logger = Logger::new(Id::new(1));
        assert_eq!(logger.handle(&cfg, &audit_event(20, 10), now).len(), 1);
        let leave = Event::MemberRemove(twilight_model::gateway::payload::incoming::MemberRemove {
            guild_id: Id::new(9),
            user: user(20),
        });
        assert!(logger.handle(&cfg, &leave, now).is_empty());
    }

    #[test]
    fn delete_waits_for_late_audit_entry() {
        let cfg = LogConfig {
            fallback: Some(Id::new(5)),
            ..Default::default()
        };
        let t0 = Instant::now();
        let mut logger = Logger::new(Id::new(1));
        logger.cache.insert(
            Id::new(9),
            Id::new(7),
            CachedMessage::new(Id::new(20), "hi".into(), t0),
            t0,
        );
        let delete =
            Event::MessageDelete(twilight_model::gateway::payload::incoming::MessageDelete {
                channel_id: Id::new(30),
                guild_id: Some(Id::new(9)),
                id: Id::new(7),
            });
        assert!(logger.handle(&cfg, &delete, t0).is_empty());
        let late = t0 + Duration::from_secs(3);
        assert!(logger.handle(&cfg, &audit_event(72, 10), late).is_empty());
        let out = logger.flush(t0 + HOLD);
        assert_eq!(out.len(), 1);
        assert!(out[0].content.contains("**Message deleted** by <@10>"));
        assert!(logger.flush(t0 + HOLD).is_empty());
    }

    #[test]
    fn custom_type_takes_its_event_channel() {
        let cfg = LogConfig {
            fallback: Some(Id::new(5)),
            custom: vec![CustomLog {
                events: vec![LogEvent::MemberKick],
                channel: Id::new(6),
            }],
            ..Default::default()
        };
        let out = Logger::new(Id::new(1)).handle(&cfg, &audit_event(20, 10), Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel_id, Id::new(6));
    }

    #[test]
    fn clamp_limits_characters() {
        assert_eq!(clamp("é".repeat(MAX_CHARS + 5)).chars().count(), MAX_CHARS);
    }
}
