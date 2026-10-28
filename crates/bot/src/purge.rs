//! Purge: pages back through a channel until `count` messages match every filter or 1000 were scanned.
use std::collections::{hash_map::Entry, HashMap};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use twilight_model::{
    channel::Message,
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    automod::count_emojis,
    modcmds::Anchor,
};

pub const MAX: usize = 1000;
/// Discord refuses bulk deletes of messages older than 14 days (a minute of margin).
const BULK_AGE_S: i64 = 14 * 86_400 - 60;

/// Wick's purge types plus the role and webhook targets. A purge keeps messages matching all of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    Embeds,
    Suspicious,
    Bots,
    Mentions,
    Reactions,
    Emojis,
    Text,
    NoAvatar,
    NoRole,
    Attachments,
    Links,
    After(u64),
    Before(u64),
    Equals(String),
    Has(String),
    Starts(String),
    Webhook(u64),
    Role(u64),
}

pub const TYPES: &str = "embeds, suspicious, bots, mentions, reactions, emojis, text, noavatar, norole, attachments, links, after ID, before ID, equals TEXT, has TEXT, starts TEXT, webhook ID";

impl Filter {
    /// `?t TYPE [input]` with Wick's aliases.
    pub fn parse(kind: &str, input: Option<&str>) -> Result<Self, String> {
        let text = || {
            input
                .map(str::to_lowercase)
                .ok_or_else(|| format!("`{kind}` needs text after it"))
        };
        let id = || {
            input
                .and_then(|i| i.trim().parse().ok())
                .ok_or_else(|| format!("`{kind}` needs a message or webhook id after it"))
        };
        Ok(match kind {
            "embeds" | "embed" | "e" => Self::Embeds,
            "suspicious" | "sus" => Self::Suspicious,
            "bots" | "bot" | "b" => Self::Bots,
            "mentions" | "mention" | "pings" | "m" => Self::Mentions,
            "reactions" | "reaction" | "r" => Self::Reactions,
            "emojis" | "emoji" | "emotes" => Self::Emojis,
            "text" | "txt" | "t" => Self::Text,
            "noavatar" | "nopfp" | "nopic" => Self::NoAvatar,
            "norole" | "noroles" => Self::NoRole,
            "attachments" | "attachment" | "files" | "images" | "a" => Self::Attachments,
            "links" | "link" | "urls" | "l" => Self::Links,
            "after" => Self::After(id()?),
            "before" => Self::Before(id()?),
            "equals" | "eq" | "is" => Self::Equals(text()?),
            "has" | "contains" | "includes" => Self::Has(text()?),
            "starts" | "startswith" | "prefix" => Self::Starts(text()?),
            "webhook" | "webhooks" | "wh" => Self::Webhook(id()?),
            _ => return Err(format!("Unknown purge type `{kind}`. Types: {TYPES}")),
        })
    }

    fn needs_roles(&self) -> bool {
        matches!(self, Self::NoRole | Self::Role(_))
    }
}

/// The parts of a message the filters look at.
#[derive(Debug, Default)]
pub struct Msg {
    pub id: u64,
    pub author: u64,
    pub ts: i64,
    pub bot: bool,
    pub avatar: bool,
    pub webhook: Option<u64>,
    pub content: String,
    pub embeds: usize,
    pub attachments: usize,
    pub reactions: usize,
    pub mentions: usize,
}

impl From<&Message> for Msg {
    fn from(m: &Message) -> Self {
        Self {
            id: m.id.get(),
            author: m.author.id.get(),
            ts: m.timestamp.as_secs(),
            bot: m.author.bot,
            avatar: m.author.avatar.is_some(),
            webhook: m.webhook_id.map(|w| w.get()),
            content: m.content.clone(),
            embeds: m.embeds.len(),
            attachments: m.attachments.len(),
            reactions: m.reactions.len(),
            mentions: m.mentions.len() + m.mention_roles.len() + usize::from(m.mention_everyone),
        }
    }
}

/// `roles` is the author's roles (None when not a member, or not looked up); `min_age_s` is join gate's minimum age.
fn matches(f: &Filter, m: &Msg, roles: Option<&[u64]>, min_age_s: i64, now: i64) -> bool {
    let text = m.content.to_lowercase();
    match f {
        Filter::Embeds => m.embeds > 0,
        Filter::Suspicious => {
            let created = ((m.author >> 22) + 1_420_070_400_000) as i64 / 1000;
            !m.avatar || now - created < min_age_s
        }
        Filter::Bots => m.bot,
        Filter::Mentions => m.mentions > 0,
        Filter::Reactions => m.reactions > 0,
        Filter::Emojis => count_emojis(&m.content) > 0,
        Filter::Text => m.embeds == 0 && m.attachments == 0 && !m.content.is_empty(),
        Filter::NoAvatar => !m.avatar,
        Filter::NoRole => roles.is_some_and(|r| r.is_empty()),
        Filter::Attachments => m.attachments > 0,
        Filter::Links => text.contains("http://") || text.contains("https://"),
        Filter::After(id) => m.id > *id,
        Filter::Before(id) => m.id < *id,
        Filter::Equals(s) => text == *s,
        Filter::Has(s) => text.contains(s.as_str()),
        Filter::Starts(s) => text.starts_with(s.as_str()),
        Filter::Webhook(id) => m.webhook == Some(*id),
        Filter::Role(id) => roles.is_some_and(|r| r.contains(id)),
    }
}

/// Picks up to `count` ids from `msgs` (newest first) by `user` matching every filter, younger than 14 days.
fn pick(
    msgs: &[Msg],
    count: usize,
    user: Option<u64>,
    filters: &[Filter],
    roles: &HashMap<u64, Vec<u64>>,
    min_age_s: i64,
    now: i64,
) -> Vec<u64> {
    msgs.iter()
        .filter(|m| user.is_none_or(|u| u == m.author) && now - m.ts < BULK_AGE_S)
        .filter(|m| {
            filters.iter().all(|f| {
                matches(
                    f,
                    m,
                    roles.get(&m.author).map(Vec::as_slice),
                    min_age_s,
                    now,
                )
            })
        })
        .take(count)
        .map(|m| m.id)
        .collect()
}

/// Returns how many messages were deleted. A purge menu anchor reads one page from the clicked message.
pub async fn run(
    app: &App,
    gid: Id<GuildMarker>,
    ch: Id<ChannelMarker>,
    count: usize,
    user: Option<u64>,
    filters: &[Filter],
    from: Option<Anchor>,
) -> Result<usize> {
    let min_age_s = app.cfg(gid.get()).await?.joingate.min_age_ms() / 1000;
    let now = now_secs();
    let mut roles: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut ids = vec![];
    let mut before: Option<u64> = None;
    let mut scanned = 0;
    while ids.len() < count && scanned < MAX {
        let req = app.http.channel_messages(ch).limit(100);
        let page = match (from, before) {
            // `after` is exclusive, so start one id earlier to include the clicked message.
            (Some(Anchor::From(m)), _) => req.after(Id::new(m.max(2) - 1)).await?,
            (Some(Anchor::Until(m)), None) => req.before(Id::new(m)).await?,
            (_, Some(b)) => req.before(Id::new(b)).await?,
            (None, None) => req.await?,
        };
        let msgs: Vec<Msg> = page.models().await?.iter().map(Msg::from).collect();
        if filters.iter().any(Filter::needs_roles) {
            for m in &msgs {
                if let Entry::Vacant(e) = roles.entry(m.author) {
                    if let Ok(member) = app.http.guild_member(gid, Id::new(m.author)).await {
                        e.insert(
                            member
                                .model()
                                .await?
                                .roles
                                .iter()
                                .map(|r| r.get())
                                .collect(),
                        );
                    }
                }
            }
        }
        scanned += msgs.len();
        ids.extend(pick(
            &msgs,
            count - ids.len(),
            user,
            filters,
            &roles,
            min_age_s,
            now,
        ));
        let oldest = msgs.iter().map(|m| (m.id, m.ts)).min();
        match oldest {
            Some((id, ts)) if from.is_none() && msgs.len() == 100 && now - ts < BULK_AGE_S => {
                before = Some(id)
            }
            _ => break,
        }
    }
    for chunk in ids.chunks(100) {
        let chunk: Vec<Id<MessageMarker>> = chunk.iter().map(|i| Id::new(*i)).collect();
        match chunk.as_slice() {
            [one] => {
                app.http.delete_message(ch, *one).await?;
            }
            many => {
                app.http.delete_messages(ch, many).await?;
            }
        }
    }
    Ok(ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64, author: u64, ts: i64) -> Msg {
        Msg {
            id,
            author,
            ts,
            avatar: true,
            ..Default::default()
        }
    }

    #[test]
    fn picks_by_user_age_count() {
        let now = 10_000_000;
        let old = now - BULK_AGE_S - 1;
        let msgs = [
            msg(5, 1, now),
            msg(4, 2, now - 5),
            msg(3, 1, now - 10),
            msg(2, 1, old),
        ];
        let none = HashMap::new();
        assert_eq!(pick(&msgs, 2, None, &[], &none, 0, now), vec![5, 4]);
        assert_eq!(pick(&msgs, 10, Some(1), &[], &none, 0, now), vec![5, 3]);
    }

    #[test]
    fn filters_combine() {
        let now = 10_000_000;
        let mut a = msg(5, 1, now);
        a.content = "Free NITRO https://x.y".into();
        let mut b = msg(4, 2, now);
        b.bot = true;
        b.embeds = 1;
        let c = msg(3, 3, now);
        let msgs = [a, b, c];
        let roles = HashMap::from([(1, vec![7]), (3, vec![])]);
        let p = |f: &[Filter]| pick(&msgs, 10, None, f, &roles, 0, now);
        assert_eq!(p(&[Filter::Links]), vec![5]);
        assert_eq!(p(&[Filter::Has("nitro".into()), Filter::Role(7)]), vec![5]);
        assert_eq!(p(&[Filter::Bots, Filter::Embeds]), vec![4]);
        assert_eq!(p(&[Filter::NoRole]), vec![3]);
        assert_eq!(p(&[Filter::Before(5)]), vec![4, 3]);
    }

    #[test]
    fn parses_types() {
        assert_eq!(Filter::parse("pings", None), Ok(Filter::Mentions));
        assert_eq!(
            Filter::parse("has", Some("Hi There")),
            Ok(Filter::Has("hi there".into()))
        );
        assert!(Filter::parse("has", None).is_err());
        assert_eq!(Filter::parse("after", Some("123")), Ok(Filter::After(123)));
        assert!(Filter::parse("nope", None).is_err());
    }
}
