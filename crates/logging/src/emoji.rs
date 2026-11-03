//! Tommybot's custom emojis, the one place that maps them to ids. The SVG sources are in
//! `assets/icons`; `node tools/emojis.mjs upload` uploads them as application emojis and the bot
//! calls [`load`] with their ids at startup. An emoji that was not uploaded shows its Unicode
//! fallback, so a fresh or self-hosted bot still works.
use std::{collections::HashMap, fmt, sync::OnceLock};
use twilight_model::{
    channel::message::{component::Button, EmojiReactionType},
    id::Id,
};

static IDS: OnceLock<HashMap<String, u64>> = OnceLock::new();

/// Stores the application's emojis as (name, id). Only the first call counts.
pub fn load(ids: impl IntoIterator<Item = (String, u64)>) {
    let _ = IDS.set(ids.into_iter().collect());
}

#[derive(Clone, Copy)]
pub struct Emoji {
    /// File name in `assets/icons` and the emoji's name on Discord.
    pub name: &'static str,
    pub fallback: &'static str,
}

impl Emoji {
    fn id(self) -> Option<u64> {
        IDS.get()?.get(self.name).copied()
    }

    /// Puts the emoji on a button and drops the fallback from its label. Without the uploaded
    /// emoji the button keeps its text label, since symbols like `‹` are not valid button emojis.
    pub fn on(self, mut b: Button) -> Button {
        if let Some(id) = self.id() {
            b.emoji = Some(EmojiReactionType::Custom {
                animated: false,
                id: Id::new(id),
                name: Some(self.name.into()),
            });
            b.label = b
                .label
                .map(|l| l.replace(self.fallback, "").trim().to_owned())
                .filter(|l| !l.is_empty());
        }
        b
    }
}

/// `<:name:id>` in message text, or the fallback.
impl fmt::Display for Emoji {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.id() {
            Some(id) => write!(f, "<:{}:{id}>", self.name),
            None => f.write_str(self.fallback),
        }
    }
}

impl From<Emoji> for String {
    fn from(e: Emoji) -> Self {
        e.to_string()
    }
}

const fn e(name: &'static str, fallback: &'static str) -> Emoji {
    Emoji { name, fallback }
}

pub const ARROW: Emoji = e("right_sort", "▸");
pub const YES: Emoji = e("success", "✅");
pub const NO: Emoji = e("error", "❌");
pub const REASON: Emoji = e("reason", "📄");
pub const MODERATOR: Emoji = e("moderator_field", "👤");
pub const MEMBER: Emoji = e("member", "👤");
pub const AGE: Emoji = e("account_age", "🎂");
pub const TARGET: Emoji = e("target", "🎯");
pub const QUARANTINED: Emoji = e("quarantine_role", "🧍");
pub const PANIC: Emoji = e("panic", "🛑");
pub const LOCKDOWN: Emoji = e("lockdown", "🔒");
pub const HELP: Emoji = e("help", "🛟");
pub const ONLINE: Emoji = e("online_dot", "🟢");
pub const CHEVRON: Emoji = e("chevron_right", "❯");
pub const LINK: Emoji = e("external_link", "↗");
pub const RUNNING: Emoji = e("loading", "▶");
pub const WAITING: Emoji = e("pending", "▫");
pub const PAGE_FIRST: Emoji = e("page_first", "≪");
pub const PAGE_PREV: Emoji = e("page_prev", "‹");
pub const PAGE_NEXT: Emoji = e("page_next", "›");
pub const PAGE_LAST: Emoji = e("page_last", "≫");
pub const CLOSE: Emoji = e("close", "✖");
pub const ON: Emoji = e("toggle_on", "🟢");
pub const OFF: Emoji = e("toggle_off", "🔴");
pub const ANTI_NUKE: Emoji = e("anti_nuke", "▸");
pub const HEAT: Emoji = e("heat", "▸");
pub const VERIFICATION: Emoji = e("verification", "▸");
pub const JOIN_GATE: Emoji = e("join_gate", "▸");
pub const JOIN_RAID: Emoji = e("join_raid", "▸");
pub const PERSISTENCE: Emoji = e("quarantine", "▸");
pub const LATENCY: Emoji = e("duration", "▸");
pub const UPTIME: Emoji = e("clock", "▸");
pub const RESOURCES: Emoji = e("analytics", "▸");
pub const RAM: Emoji = e("database", "▸");
pub const CPU: Emoji = e("meter_high", "▸");
pub const SIZE: Emoji = e("stats", "▸");
pub const SERVERS: Emoji = e("server", "▸");

pub const CLOCK: Emoji = e("clock", "▸");

pub const TICKET: Emoji = e("modmail", "📨");
pub const TICKET_NEW: Emoji = e("modmail_new", "📬");
pub const TICKET_REPLY: Emoji = e("modmail_reply", "💬");
pub const TICKET_CLOSE: Emoji = e("modmail_close", "📪");
pub const TICKET_BLOCK: Emoji = e("modmail_block", "⛔");

pub const SHIELD: Emoji = e("shield_check", "🛡️");
pub const CRITICAL: Emoji = e("shield_x", "⛔");
pub const WARNING: Emoji = e("warning", "⚠️");
pub const TIP: Emoji = e("info", "ℹ️");

/// Label word → icon name for [`for_label`]; first match wins.
const BY_WORD: &[(&str, &str)] = &[
    ("kicked", "kick"),
    ("revert", "restore"),
    ("recreat", "backup"),
    ("responsible", "moderator_field"),
    ("moderator", "moderator_field"),
    ("executor", "moderator_field"),
    ("inviter", "invite"),
    ("closed by", "moderator_field"),
    ("opened by", "member"),
    ("owner", "owner"),
    ("author", "member"),
    ("attachment", "attachments"),
    ("sent", "calendar"),
    ("username", "nickname"),
    ("nick", "nickname"),
    ("rate limit", "slowmode"),
    ("user limit", "members"),
    ("member", "member"),
    ("user", "member"),
    ("target", "target"),
    ("channel", "channel_text"),
    ("roles added", "role_add"),
    ("roles removed", "role_remove"),
    ("role", "role"),
    ("permission", "permissions"),
    ("additions", "plus"),
    ("removals", "minus"),
    ("type", "tag"),
    ("explicit", "nsfw_filter"),
    ("notification", "notifications_feed"),
    ("message id", "message_id"),
    ("content", "message"),
    ("message", "message"),
    ("reason", "reason"),
    ("case", "case"),
    ("action", "settings"),
    ("duration", "duration"),
    ("uptime", "clock"),
    ("creat", "account_age"),
    ("joined", "joined_date"),
    ("invite", "invite_code"),
    ("position", "position"),
    ("name", "nickname"),
    ("tag", "tag"),
    ("topic", "reason"),
    ("nsfw", "nsfw_filter"),
    ("slowmode", "slowmode"),
    ("emoji", "emoji"),
    ("webhook", "webhook"),
    ("avatar", "avatar_field"),
    ("badge", "nitro_badge"),
    ("bot", "bot"),
    ("count", "counting"),
    ("strike", "warn_points"),
    ("rule", "rules"),
    ("factor", "heat"),
    ("error", "error"),
    ("result", "logs"),
    ("appeal", "appeal"),
    ("banned", "ban"),
    ("timed out", "timeout"),
    ("quarantine", "quarantine"),
    ("server", "server"),
    ("version", "info"),
    ("suspicious", "suspicious_account"),
    ("whitelist", "whitelist"),
    ("warn", "warn_points"),
    ("same ip", "fingerprint"),
    ("categor", "category"),
    ("accessor", "avatar_field"),
    ("price", "coin"),
    ("communication", "timeout"),
    ("mention", "mentions"),
    ("color", "palette"),
    ("colour", "palette"),
    ("afk", "idle_dot"),
    ("verification", "verification"),
    ("code", "invite_code"),
    ("mute", "mute"),
    ("deaf", "mute"),
    ("bitrate", "channel_voice"),
    ("sticker", "sticker"),
    ("banner", "image"),
    ("icon", "image"),
    ("splash", "image"),
    ("descr", "reason"),
    ("id", "user_id"),
];

/// The icon in front of a `**Label:**` line, matched on words in the label (first match wins),
/// per-field icons. Unknown labels keep the plain arrow.
pub fn for_label(label: &str) -> Emoji {
    let l = label.to_ascii_lowercase();
    // Keys match at the start of a word, so "Account" is not "count" and "Essential" is not
    // "sent". "id" must be the whole word, or "identity" would match.
    let word = |w: &str| {
        l.match_indices(w).any(|(i, _)| {
            let b = l.as_bytes();
            let start = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            let end = w != "id" || b.get(i + 2).is_none_or(|c| !c.is_ascii_alphanumeric());
            start && end
        })
    };
    BY_WORD
        .iter()
        .find(|(w, _)| word(w))
        .map_or(ARROW, |&(_, name)| e(name, "▸"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_then_uses_ids() {
        let b = || Button {
            id: None,
            custom_id: Some("x".into()),
            disabled: false,
            emoji: None,
            label: Some("View Message ↗".into()),
            style: twilight_model::channel::message::component::ButtonStyle::Link,
            url: None,
            sku_id: None,
        };
        assert_eq!(YES.to_string(), "✅");
        assert_eq!(LINK.on(b()).label.as_deref(), Some("View Message ↗"));
        load([("success".into(), 5), ("external_link".into(), 6)]);
        assert_eq!(YES.to_string(), "<:success:5>");
        assert_eq!(NO.to_string(), "❌");
        let l = LINK.on(b());
        assert_eq!(l.label.as_deref(), Some("View Message"));
        assert!(matches!(l.emoji, Some(EmojiReactionType::Custom { .. })));
        assert_eq!(for_label("Role Name").name, "role");
        assert_eq!(for_label("Id").name, "user_id");
        assert_eq!(for_label("Recreated").name, "backup");
        assert_eq!(for_label("Valid").name, "right_sort");
        assert_eq!(for_label("Account Accessories:").name, "avatar_field");
        assert_eq!(for_label("Essential Roles").name, "role");
        assert_eq!(for_label("Rate limit per user").name, "slowmode");
        assert_eq!(for_label("Message Id").name, "message_id");
        assert_eq!(for_label("Case").name, "case");
    }

    #[test]
    fn every_icon_has_an_svg() {
        let consts = [
            ARROW,
            YES,
            NO,
            REASON,
            MODERATOR,
            MEMBER,
            AGE,
            TARGET,
            QUARANTINED,
            PANIC,
            LOCKDOWN,
            HELP,
            ONLINE,
            CHEVRON,
            LINK,
            RUNNING,
            WAITING,
            PAGE_FIRST,
            PAGE_PREV,
            PAGE_NEXT,
            PAGE_LAST,
            CLOSE,
            ON,
            OFF,
            ANTI_NUKE,
            HEAT,
            VERIFICATION,
            JOIN_GATE,
            JOIN_RAID,
            PERSISTENCE,
            LATENCY,
            UPTIME,
            RESOURCES,
            RAM,
            CPU,
            SIZE,
            SERVERS,
            CLOCK,
            TICKET,
            TICKET_NEW,
            TICKET_REPLY,
            TICKET_CLOSE,
            TICKET_BLOCK,
        ];
        let names = consts
            .iter()
            .map(|e| e.name)
            .chain(BY_WORD.iter().map(|w| w.1));
        for n in names {
            let path = format!("{}/../../assets/icons/{n}.svg", env!("CARGO_MANIFEST_DIR"));
            assert!(std::path::Path::new(&path).exists(), "{n}.svg missing");
        }
    }
}
