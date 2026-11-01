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

#[derive(Clone, Copy, Debug, PartialEq)]
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
pub const NO: Emoji = e("error", "❎");
pub const REASON: Emoji = e("reason", "📄");
pub const MODERATOR: Emoji = e("moderator_field", "👤");
pub const MEMBER: Emoji = e("member", "👤");
pub const AGE: Emoji = e("account_age", "🎂");
pub const TARGET: Emoji = e("target", "🎟️");
pub const QUARANTINED: Emoji = e("quarantine_role", "🧍");
pub const PANIC: Emoji = e("panic", "🛑");
pub const LOCKDOWN: Emoji = e("lockdown", "🔒");
pub const HELP: Emoji = e("help", "🫴");
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
        assert_eq!(NO.to_string(), "❎");
        let l = LINK.on(b());
        assert_eq!(l.label.as_deref(), Some("View Message"));
        assert!(matches!(l.emoji, Some(EmojiReactionType::Custom { .. })));
    }
}
