use crate::{
    cache::CachedMessage,
    embed::{self, Log, ARROW, GREEN, NO, ORANGE, RED, YES},
    Entry, LogEvent, LogKind,
};
use serde_json::Value;
use twilight_model::{
    channel::{message::Embed, Message},
    guild::{
        audit_log::{AuditLogEntry, AuditLogEventType as A, AuditLogOptionalEntryInfo},
        Permissions,
    },
    id::{
        marker::{ChannelMarker, MessageMarker, UserMarker},
        Id,
    },
    user::User,
};

/// Discord snowflakes count milliseconds from this epoch.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Wick's "Go to Message" / "Go to Channel" link button under a log.
pub type Link = (&'static str, String);

pub fn message_edit(old: Option<&str>, msg: &Message) -> (Embed, Option<Link>) {
    let embed = Log::new("Message Edited", ORANGE)
        .user("Responsible", msg.author.id.get())
        .line("Target", &embed::channel(msg.channel_id.get()))
        .time()
        .section("Changes")
        .text(&format!("{ARROW} **Content:**"))
        .diff(
            Some(old.unwrap_or("(previous content not cached)")),
            Some(&msg.content),
        )
        .build();
    let link = msg.guild_id.map(|g| {
        let url = format!(
            "https://discord.com/channels/{g}/{}/{}",
            msg.channel_id, msg.id
        );
        ("Go to Message", url)
    });
    (embed, link)
}

pub fn message_delete(
    id: Id<MessageMarker>,
    channel: Id<ChannelMarker>,
    cached: Option<&CachedMessage>,
    deleter: Option<Id<UserMarker>>,
) -> Embed {
    let mut log = Log::new("Message Deleted", RED);
    if let Some(d) = deleter {
        log = log.user("Responsible", d.get());
    }
    log = log
        .time()
        .section("Previous Settings")
        .line("Channel", &format!("<#{channel}>"))
        .line("Id", &id.to_string())
        .text(&format!("{ARROW} **Content:**"))
        .diff(
            Some(cached.map_or("(content not cached)", |c| c.content.as_str())),
            None,
        );
    if let Some(c) = cached {
        log = log.user("Author", c.author.get());
    }
    log.build()
}

pub fn bulk_delete(channel: Id<ChannelMarker>, count: usize) -> Embed {
    Log::new("Message Delete Bulk", RED)
        .line("Channel", &embed::channel(channel.get()))
        .time()
        .line("Count", &count.to_string())
        .build()
}

pub fn member_add(user: &User) -> Embed {
    let created = ((user.id.get() >> 22) + DISCORD_EPOCH_MS) / 1000;
    Log::new("Member Join", GREEN)
        .thumbnail(user)
        .user("Target", user.id.get())
        .time()
        .line("Creation", &format!("<t:{created}:F> (<t:{created}:R>)"))
        .build()
}

pub fn member_remove(user: &User) -> Embed {
    Log::new("Member Leave", RED)
        .thumbnail(user)
        .user("Target", user.id.get())
        .time()
        .build()
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
        | A::MemberMove
        | A::MemberDisconnect
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

    let a = e.action_type;
    let name = format!("{a:?}");
    let created = name.ends_with("Create") || matches!(a, A::BotAdd | A::MessagePin);
    let deleted = name.ends_with("Delete");
    let opts = e.options.as_ref();
    let overwrite = matches!(
        a,
        A::ChannelOverwriteCreate | A::ChannelOverwriteUpdate | A::ChannelOverwriteDelete
    );

    let mut log =
        Log::new(&words(&name), color(a)).maybe_user("Responsible", e.user_id.map(Id::get));
    // Overwrite entries target the channel; Wick shows the role or member as the target.
    let channel = if overwrite {
        e.target_id.map(Id::get)
    } else {
        opts.and_then(|o| o.channel_id).map(Id::get)
    };
    if let Some(c) = channel {
        log = log.line("Channel", &embed::channel(c));
    }
    let target = if overwrite {
        opts.and_then(|o| o.id).map(|id| {
            if opts.and_then(|o| o.kind.as_deref()) == Some("1") {
                embed::user(id.get())
            } else {
                embed::role(id.get())
            }
        })
    } else {
        e.target_id.map(|t| target_ref(kind, t.get()))
    };
    if let Some(t) = target {
        log = log.line("Target", &t);
    }
    log = log.time();
    if let Some(reason) = &e.reason {
        log = log.line("Reason", reason);
    }

    log = changes(log, e, kind, overwrite, created, deleted);
    log = additional_info(log, opts);

    // Wick links the channel of channel and thread logs, except when it is gone.
    let link = match (kind, e.guild_id, channel.or(e.target_id.map(Id::get))) {
        (LogKind::Channel | LogKind::Thread, Some(g), Some(c)) if !deleted => Some((
            "Go to Channel",
            format!("https://discord.com/channels/{g}/{c}"),
        )),
        _ => None,
    };

    Some(Entry {
        kind,
        event: LogEvent::from_action(a),
        channel: None,
        user: None,
        embed: log.build(),
        link,
    })
}

/// Wick's "Settings" (created), "Previous Settings" (deleted) or "Changes" (updated) section.
fn changes(
    mut log: Log,
    e: &AuditLogEntry,
    kind: LogKind,
    overwrite: bool,
    created: bool,
    deleted: bool,
) -> Log {
    if !e.changes.is_empty() {
        log = log.section(if created {
            "Settings"
        } else if deleted {
            "Previous Settings"
        } else {
            "Changes"
        });
    }
    for change in &e.changes {
        // The serde shape is {"key", "old_value", "new_value"} for every change variant.
        let Ok(v) = serde_json::to_value(change) else {
            continue;
        };
        let (old, new) = (v.get("old_value"), v.get("new_value"));
        log = match v["key"].as_str().unwrap_or("?") {
            // Changes twilight cannot decode, and ids the target line and Additional Information show.
            "other" | "id" => log,
            "type" if overwrite => log,
            "$add" => log.line("Roles Added", &roles(new)),
            "$remove" => log.line("Roles Removed", &roles(new)),
            key @ ("permissions" | "allow" | "deny") => {
                let label = match key {
                    "allow" => format!("{YES} **Allowed:**"),
                    "deny" => format!("{NO} **Denied:**"),
                    _ => format!("{ARROW} **Permissions:**"),
                };
                let (added, removed) = perm_diff(old, new);
                if added.is_empty() && removed.is_empty() {
                    continue;
                }
                let mut l = log.text(&label);
                if !added.is_empty() {
                    l = l.text(&format!("{ARROW} **Additions:** {added}"));
                }
                if !removed.is_empty() {
                    l = l.text(&format!("{ARROW} **Removals:** {removed}"));
                }
                l
            }
            key if created => log.line(&words(key), &show(key, kind, new)),
            key if deleted => log.line(&words(key), &show(key, kind, old)),
            key => log
                .text(&format!("{ARROW} **{}:**", words(key)))
                .diff(Some(&show(key, kind, old)), Some(&show(key, kind, new))),
        };
    }

    log
}

/// The audit entry's options, as Wick's "Additional Information" section.
fn additional_info(mut log: Log, opts: Option<&AuditLogOptionalEntryInfo>) -> Log {
    let info: Vec<(&str, String)> = opts
        .map(|o| {
            [
                ("Id", o.id.map(|i| i.to_string())),
                ("Role Name", o.role_name.clone()),
                (
                    "Type",
                    o.kind
                        .as_deref()
                        .map(|k| if k == "1" { "Member" } else { "Role" }.into()),
                ),
                ("Count", o.count.clone()),
                ("Delete Member Days", o.delete_member_days.clone()),
                ("Members Removed", o.members_removed.clone()),
                ("Message Id", o.message_id.map(|i| i.to_string())),
            ]
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect()
        })
        .unwrap_or_default();
    if !info.is_empty() {
        log = log.section("Additional Information");
        for (k, v) in info {
            log = log.line(k, &format!("`{v}`"));
        }
    }

    log
}

/// Green for things added, red for things removed, orange for changes.
fn color(a: A) -> u32 {
    let name = format!("{a:?}");
    if name.ends_with("Create") || matches!(a, A::BotAdd | A::MemberBanRemove | A::MessagePin) {
        GREEN
    } else if name.ends_with("Delete")
        || matches!(
            a,
            A::MemberKick
                | A::MemberPrune
                | A::MemberBanAdd
                | A::MemberDisconnect
                | A::MessageUnpin
        )
    {
        RED
    } else {
        ORANGE
    }
}

/// The `$add` and `$remove` changes list partial roles: `[{"id", "name"}]`.
fn roles(v: Option<&Value>) -> String {
    let list: Vec<String> = v
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(|id| format!("<@&{id}>")))
        .collect();
    if list.is_empty() {
        "none".into()
    } else {
        list.join(" ")
    }
}

/// Permission bitfields (strings) as "Create Instant Invite, View Channel" for the added and
/// removed bits.
fn perm_diff(old: Option<&Value>, new: Option<&Value>) -> (String, String) {
    let bits = |v: Option<&Value>| {
        Permissions::from_bits_truncate(
            v.and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        )
    };
    let (old, new) = (bits(old), bits(new));
    (perm_names(new - old), perm_names(old - new))
}

fn perm_names(p: Permissions) -> String {
    p.iter_names()
        .map(|(n, _)| {
            n.split('_')
                .map(|w| {
                    let mut c = w.chars();
                    c.next().map_or(String::new(), |f| {
                        f.to_string() + &c.as_str().to_lowercase()
                    })
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn target_ref(kind: LogKind, id: u64) -> String {
    match kind {
        LogKind::Channel | LogKind::Thread => embed::channel(id),
        LogKind::Role => embed::role(id),
        // A pin's target is the message author.
        LogKind::Member | LogKind::Message => embed::user(id),
        _ => format!("`{id}`"),
    }
}

/// A change value as Wick shows it: mentions for ids, names for channel types, hex colours.
fn show(key: &str, kind: LogKind, v: Option<&Value>) -> String {
    match (key, v) {
        (_, None | Some(Value::Null)) => "none".into(),
        ("channel_id", Some(Value::String(s))) => s.parse().map_or(s.clone(), embed::channel),
        ("inviter_id" | "application_id", Some(Value::String(s))) => {
            s.parse().map_or(s.clone(), embed::user)
        }
        ("color", Some(Value::Number(n))) => format!("#{:06X}", n.as_u64().unwrap_or(0)),
        ("permission_overwrites", Some(Value::Array(a))) => a.len().to_string(),
        ("type", Some(Value::Number(n))) if matches!(kind, LogKind::Channel | LogKind::Thread) => {
            channel_type(n.as_u64().unwrap_or(u64::MAX)).into()
        }
        (_, Some(Value::String(s))) => s.clone(),
        (_, Some(v)) => v.to_string(),
    }
}

fn channel_type(t: u64) -> &'static str {
    match t {
        0 => "Text",
        2 => "Voice",
        4 => "Category",
        5 => "Announcement",
        10 => "Announcement Thread",
        11 => "Public Thread",
        12 => "Private Thread",
        13 => "Stage",
        15 => "Forum",
        16 => "Media",
        _ => "Other",
    }
}

/// `CamelCase` to `Camel Case`, `snake_case` to `Snake case`.
fn words(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        match ch {
            '_' => out.push(' '),
            c if i == 0 => out.extend(c.to_uppercase()),
            c if c.is_uppercase() => {
                out.push(' ');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_case() {
        assert_eq!(words("ChannelOverwriteCreate"), "Channel Overwrite Create");
        assert_eq!(words("rate_limit_per_user"), "Rate limit per user");
    }

    #[test]
    fn colours_follow_the_action() {
        assert_eq!(color(A::RoleCreate), GREEN);
        assert_eq!(color(A::MemberBanAdd), RED);
        assert_eq!(color(A::RoleUpdate), ORANGE);
    }

    #[test]
    fn permission_bits_become_names() {
        let (added, removed) = perm_diff(
            Some(&Value::String("1024".into())),
            Some(&Value::String("3072".into())),
        );
        assert_eq!(added, "Send Messages");
        assert_eq!(removed, "");
    }

    #[test]
    fn overwrite_shows_channel_and_role_target_with_denied_names() {
        let e: AuditLogEntry = serde_json::from_value(serde_json::json!({
            "action_type": 13, "id": "1", "guild_id": "9", "user_id": "10", "target_id": "30",
            "options": { "id": "40", "type": "0", "role_name": "Quarantine" },
            "changes": [{ "key": "deny", "new_value": "2048" }]
        }))
        .unwrap();
        let entry = audit(&e).unwrap();
        let d = entry.embed.description.unwrap();
        assert!(d.contains("▸ **Channel:** <#30> *`30`*\n▸ **Target:** <@&40> *`40`*"));
        assert!(d.contains("**Settings**\n❌ **Denied:**\n▸ **Additions:** Send Messages"));
        assert!(d.ends_with("**Additional Information**\n▸ **Id:** `40`\n▸ **Role Name:** `Quarantine`\n▸ **Type:** `Role`"));
        assert_eq!(entry.link.unwrap().1, "https://discord.com/channels/9/30");
    }

    #[test]
    fn change_values_read_like_wick() {
        let e: AuditLogEntry = serde_json::from_value(serde_json::json!({
            "action_type": 10, "id": "1", "guild_id": "9", "user_id": "10", "target_id": "30",
            "changes": [
                { "key": "type", "new_value": 0 },
                { "key": "permission_overwrites", "new_value": [] },
                { "key": "id", "new_value": "30" }
            ]
        }))
        .unwrap();
        let d = audit(&e).unwrap().embed.description.unwrap();
        assert!(d.ends_with("**Settings**\n▸ **Type:** Text\n▸ **Permission overwrites:** 0"));
        let v = serde_json::json!("5");
        assert_eq!(show("channel_id", LogKind::Invite, Some(&v)), "<#5> *`5`*");
        assert_eq!(
            show("color", LogKind::Role, Some(&serde_json::json!(3447003))),
            "#3498DB"
        );
    }

    #[test]
    fn role_changes_show_mentions() {
        let v = serde_json::json!([{ "id": "5", "name": "a" }, { "id": "6", "name": "b" }]);
        assert_eq!(roles(Some(&v)), "<@&5> <@&6>");
        assert_eq!(roles(None), "none");
    }
}
