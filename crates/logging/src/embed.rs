//! Wick's log layout, shared by every log in the bot: a bold title, a colour bar, then
//! description lines that start with an arrow (`▸ **Responsible:** @user *`id`*`), bold section
//! headings ("Changes", "Previous Settings") and red/green diff blocks for before/after text.
use twilight_model::{channel::message::Embed, user::User};
use twilight_util::builder::embed::{
    EmbedBuilder, EmbedFieldBuilder, EmbedFooterBuilder, ImageSource,
};

/// Colours sampled from Wick's screenshots: Discord logs (create, update, delete) and Wick's own
/// logs (alerts and reports).
pub const GREEN: u32 = 0x1BE725;
pub const ORANGE: u32 = 0xE77E1B;
pub const RED: u32 = 0xE71B1B;
pub const ALERT: u32 = 0xD0021A;
pub const DONE: u32 = 0x08E36B;
pub const HANDLED: u32 = 0x00CB73;
pub const BLUE: u32 = 0x0D9DF8;

/// Wick prefixes each line with a custom arrow emoji; this is the closest Unicode one.
pub const ARROW: &str = "▸";
pub const YES: &str = "✅";
pub const NO: &str = "❌";

/// Discord's embed limits.
const TITLE: usize = 256;
const DESCRIPTION: usize = 4096;
const FIELD_NAME: usize = 256;
const FIELD_VALUE: usize = 1024;
const FIELDS: usize = 25;
/// Discord rejects an embed whose title, description, fields and footer add up to more.
const TOTAL: usize = 6000;

pub struct Log {
    embed: EmbedBuilder,
    title: String,
    lines: Vec<String>,
    fields: Vec<(String, String, bool)>,
    footer: Option<String>,
}

impl Log {
    pub fn new(title: &str, color: u32) -> Self {
        Self {
            embed: EmbedBuilder::new().color(color),
            title: clip(title, TITLE),
            lines: Vec::new(),
            fields: Vec::new(),
            footer: None,
        }
    }

    /// `▸ **label:** value`
    pub fn line(mut self, label: &str, value: &str) -> Self {
        self.lines.push(format!("{ARROW} **{label}:** {value}"));
        self
    }

    /// A line as given, for Wick's ✅/❌ status lines and free text.
    pub fn text(mut self, text: &str) -> Self {
        self.lines.push(text.into());
        self
    }

    /// `▸ <time>` with the current time, as on Wick's Discord logs.
    pub fn time(self) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.text(&format!("{ARROW} <t:{now}:F>"))
    }

    /// A bold heading ("Changes", "Settings", "Previous Settings", "Additional Information").
    pub fn section(mut self, name: &str) -> Self {
        self.lines.push(format!("**{name}**"));
        self
    }

    /// Red `- old` and green `+ new` diff blocks, Wick's before/after display.
    pub fn diff(mut self, old: Option<&str>, new: Option<&str>) -> Self {
        for (sign, text) in [("-", old), ("+", new)] {
            if let Some(t) = text {
                let body: Vec<String> = t
                    .replace("```", "`\u{200b}``")
                    .lines()
                    .map(|l| format!("{sign} {l}"))
                    .collect();
                let body = if body.is_empty() {
                    format!("{sign} ")
                } else {
                    body.join("\n")
                };
                self.lines
                    .push(format!("```diff\n{}\n```", clip(&body, 1500)));
            }
        }
        self
    }

    /// `▸ **label:** @user *`id`*`
    pub fn user(self, label: &str, id: u64) -> Self {
        self.line(label, &user(id))
    }

    /// Like [`Log::user`], with "Unknown" when the audit log did not name anyone.
    pub fn maybe_user(self, label: &str, id: Option<u64>) -> Self {
        self.line(label, &id.map_or("Unknown".into(), user))
    }

    /// A field below the description, for Wick's columns ("Initial Flags:", "Roles:").
    pub fn field(mut self, name: &str, value: &str) -> Self {
        self.fields.push((name.into(), value.into(), false));
        self
    }

    pub fn inline(mut self, name: &str, value: &str) -> Self {
        self.fields.push((name.into(), value.into(), true));
        self
    }

    pub fn thumbnail(mut self, u: &User) -> Self {
        if let Ok(src) = ImageSource::url(avatar(u)) {
            self.embed = self.embed.thumbnail(src);
        }
        self
    }

    pub fn footer(mut self, text: &str) -> Self {
        self.footer = Some(clip(text, 2048));
        self
    }

    /// Cuts the parts to Discord's limits, description first, so a long log is shortened
    /// instead of rejected.
    pub fn build(self) -> Embed {
        let mut left = TOTAL - self.title.chars().count();
        // Wick's mod logs have no title, and Discord rejects an empty one.
        let mut embed = if self.title.is_empty() {
            self.embed
        } else {
            self.embed.title(self.title)
        };
        if let Some(f) = self.footer {
            left -= f.chars().count();
            embed = embed.footer(EmbedFooterBuilder::new(f));
        }
        if !self.lines.is_empty() {
            let d = clip(&self.lines.join("\n"), DESCRIPTION.min(left));
            left -= d.chars().count();
            embed = embed.description(d);
        }
        for (name, value, inline) in self.fields.into_iter().take(FIELDS) {
            let name = clip(&name, FIELD_NAME);
            let room = left.saturating_sub(name.chars().count()).min(FIELD_VALUE);
            if room < 2 {
                break;
            }
            let value = value_of(&value, room);
            left -= name.chars().count() + value.chars().count();
            let f = EmbedFieldBuilder::new(name, value);
            embed = embed.field(if inline { f.inline() } else { f });
        }
        embed.build()
    }
}

/// `@user *`id`*`: the id stays readable when the mention can't resolve.
pub fn user(id: u64) -> String {
    format!("<@{id}> *`{id}`*")
}

pub fn channel(id: u64) -> String {
    format!("<#{id}> *`{id}`*")
}

pub fn role(id: u64) -> String {
    format!("<@&{id}> *`{id}`*")
}

/// The user's avatar, or Discord's default one.
pub fn avatar(u: &User) -> String {
    match u.avatar {
        Some(h) => format!("https://cdn.discordapp.com/avatars/{}/{h}.png", u.id),
        None => format!(
            "https://cdn.discordapp.com/embed/avatars/{}.png",
            (u.id.get() >> 22) % 6
        ),
    }
}

/// Wick's short durations: `45s`, `30m`, `2h`, `7d`.
pub fn duration(secs: i64) -> String {
    match secs {
        s if s >= 86_400 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Discord rejects empty field values.
fn value_of(v: &str, max: usize) -> String {
    if v.trim().is_empty() {
        return "None".into();
    }
    clip(v, max)
}

/// Keeps the first `max` characters, ending in "…" when cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.into();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_marks_cut_text() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
        assert_eq!(clip(&"é".repeat(2000), 1024).chars().count(), 1024);
    }

    #[test]
    fn caps_fields_and_fills_empty_values() {
        let mut log = Log::new("t", RED);
        for i in 0..30 {
            log = log.field(&i.to_string(), "");
        }
        let e = log.build();
        assert_eq!(e.fields.len(), FIELDS);
        assert_eq!(e.fields[0].value, "None");
    }

    #[test]
    fn wick_lines_and_diff() {
        let e = Log::new("Message Edited", ORANGE)
            .user("Responsible", 7)
            .section("Changes")
            .diff(Some("a"), Some("b\nc"))
            .build();
        assert_eq!(
            e.description.unwrap(),
            "▸ **Responsible:** <@7> *`7`*\n**Changes**\n```diff\n- a\n```\n```diff\n+ b\n+ c\n```"
        );
    }

    #[test]
    fn long_logs_fit_discords_total_limit() {
        let mut log = Log::new("t", RED).text(&"d".repeat(5000));
        for _ in 0..6 {
            log = log.field("f", &"v".repeat(1024));
        }
        let e = log.build();
        let fields: usize = e
            .fields
            .iter()
            .map(|f| f.name.len() + f.value.chars().count())
            .sum();
        assert!(1 + e.description.unwrap().chars().count() + fields <= TOTAL);
        assert_eq!(e.fields[0].value.chars().count(), 1024);
    }

    #[test]
    fn short_durations() {
        assert_eq!(duration(1800), "30m");
        assert_eq!(duration(7200), "2h");
        assert_eq!(duration(90), "90s");
        assert_eq!(duration(604_800), "7d");
    }

    #[test]
    fn diff_cannot_close_its_block() {
        let e = Log::new("t", RED).diff(Some("x```y"), None).build();
        assert_eq!(e.description.unwrap().matches("```").count(), 2);
    }
}
