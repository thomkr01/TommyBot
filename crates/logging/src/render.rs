use crate::{cache::CachedMessage, Entry, LogKind};
use serde_json::Value;
use twilight_model::{
    channel::Message,
    guild::audit_log::{AuditLogEntry, AuditLogEventType as A},
    id::{
        marker::{ChannelMarker, MessageMarker, UserMarker},
        Id,
    },
    user::User,
};

/// Discord snowflakes count milliseconds from this epoch.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

pub fn message_edit(old: Option<&str>, msg: &Message) -> String {
    let before = old.map_or("(previous content not cached)".into(), quote);
    format!(
        "**Message edited** in <#{}> by <@{}> (message {})\nBefore:\n{before}\nAfter:\n{}",
        msg.channel_id,
        msg.author.id,
        msg.id,
        quote(&msg.content)
    )
}

pub fn message_delete(
    id: Id<MessageMarker>,
    channel: Id<ChannelMarker>,
    cached: Option<&CachedMessage>,
    deleter: Option<Id<UserMarker>>,
) -> String {
    let by = deleter.map_or(String::new(), |d| format!(" by <@{d}>"));
    match cached {
        Some(c) => format!(
            "**Message deleted**{by} in <#{channel}> (posted by <@{}>, message {id})\n{}",
            c.author,
            quote(&c.content)
        ),
        None => {
            format!("**Message deleted**{by} in <#{channel}> (message {id}, content not cached)")
        }
    }
}

pub fn bulk_delete(channel: Id<ChannelMarker>, count: usize) -> String {
    format!("**{count} messages bulk deleted** in <#{channel}>")
}

pub fn member_add(user: &User) -> String {
    let created = ((user.id.get() >> 22) + DISCORD_EPOCH_MS) / 1000;
    format!(
        "**Member joined**: <@{}>, account created <t:{created}:R>",
        user.id
    )
}

pub fn member_remove(user: &User) -> String {
    format!("**Member left**: <@{}>", user.id)
}

/// Turns a guild audit log entry into a log, or `None` for types we do not log.
pub fn audit(e: &AuditLogEntry) -> Option<Entry> {
    let kind = match e.action_type {
        A::GuildUpdate => LogKind::Server,
        A::ChannelCreate
        | A::ChannelUpdate
        | A::ChannelDelete
        | A::ChannelOverwriteCreate
        | A::ChannelOverwriteUpdate
        | A::ChannelOverwriteDelete => LogKind::Channel,
        A::RoleCreate | A::RoleUpdate | A::RoleDelete => LogKind::Role,
        A::MemberKick
        | A::MemberPrune
        | A::MemberBanAdd
        | A::MemberBanRemove
        | A::MemberUpdate
        | A::MemberRoleUpdate
        | A::BotAdd => LogKind::Member,
        A::InviteCreate | A::InviteUpdate | A::InviteDelete => LogKind::Invite,
        A::WebhookCreate | A::WebhookUpdate | A::WebhookDelete => LogKind::Webhook,
        A::EmojiCreate | A::EmojiUpdate | A::EmojiDelete => LogKind::Emoji,
        A::StickerCreate | A::StickerUpdate | A::StickerDelete => LogKind::Sticker,
        A::GuildScheduledEventCreate
        | A::GuildScheduledEventUpdate
        | A::GuildScheduledEventDelete => LogKind::ScheduledEvent,
        A::ThreadCreate | A::ThreadUpdate | A::ThreadDelete => LogKind::Thread,
        A::MessagePin | A::MessageUnpin => LogKind::Message,
        // Message deletes are logged from the gateway event, which has the cached content.
        _ => return None,
    };

    let mut text = format!("**{}**", words(&format!("{:?}", e.action_type)));
    if let Some(user) = e.user_id {
        text += &format!(" by <@{user}>");
    }
    if let Some(target) = e.target_id {
        text += &format!(" on {}", target_ref(kind, target.get()));
    }
    if let Some(channel) = e.options.as_ref().and_then(|o| o.channel_id) {
        text += &format!(" in <#{channel}>");
    }
    for change in &e.changes {
        // The serde shape is {"key", "old_value", "new_value"} for every change variant.
        let Ok(v) = serde_json::to_value(change) else {
            continue;
        };
        let key = v["key"].as_str().unwrap_or("?");
        text += &format!(
            "\n- {key}: {} → {}",
            show(v.get("old_value")),
            show(v.get("new_value"))
        );
    }
    if let Some(reason) = &e.reason {
        text += &format!("\nReason: {reason}");
    }

    Some(Entry {
        kind,
        channel: None,
        user: None,
        text,
    })
}

fn target_ref(kind: LogKind, id: u64) -> String {
    match kind {
        LogKind::Channel | LogKind::Thread => format!("<#{id}>"),
        LogKind::Role => format!("<@&{id}>"),
        LogKind::Member => format!("<@{id}>"),
        _ => id.to_string(),
    }
}

fn show(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "none".into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

/// `CamelCase` to `Camel Case`.
fn words(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// Shows user text as a Discord block quote, so it can't read as formatting.
fn quote(s: &str) -> String {
    if s.is_empty() {
        return "> (no text)".into();
    }
    s.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_case() {
        assert_eq!(words("ChannelOverwriteCreate"), "Channel Overwrite Create");
    }

    #[test]
    fn quotes_every_line() {
        assert_eq!(quote("a\nb"), "> a\n> b");
        assert_eq!(quote(""), "> (no text)");
    }
}
