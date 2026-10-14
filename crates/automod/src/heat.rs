//! Heat scoring. `heat()` is a pure function returning per-filter contributions so a log can
//! say *why* someone triggered.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HeatCfg {
    pub max: f64,
    /// Fraction of total heat lost per second (0.1 = 10%/s).
    pub decay_per_sec: f64,
    pub base: f64,
    pub per_char: f64,
    /// Characters beyond this count add `per_char` each.
    pub free_chars: usize,
    pub per_newline: f64,
    pub per_emoji: f64,
    pub per_mention: f64,
    pub per_role_mention: f64,
    pub per_everyone: f64,
    pub per_image: f64,
    /// Attachments that are not images.
    pub per_file: f64,
    pub per_embed: f64,
    pub per_link: f64,
    pub per_sticker: f64,
    pub inactive_channel: f64,
    pub per_blacklisted_word: f64,
    pub suspicious_account: f64,
    /// Added at full weight when similarity to a recent message is 1.0.
    pub similarity: f64,
    /// Only similarities above this count.
    pub similarity_floor: f64,
    pub webhook_multiplier: f64,
    pub zalgo: f64,
    /// Per uppercase / lowercase letter, on top of `per_char`.
    pub per_upper: f64,
    pub per_lower: f64,
    /// Link filters; at the default 100 one hit reaches max heat.
    pub invite: f64,
    pub malicious: f64,
    pub nsfw: f64,
    pub blacklisted_link: f64,
}

impl Default for HeatCfg {
    fn default() -> Self {
        Self {
            max: 100.0,
            decay_per_sec: 0.1,
            base: 18.0,
            per_char: 0.05,
            free_chars: 200,
            per_newline: 2.0,
            per_emoji: 3.0,
            per_mention: 15.0,
            per_role_mention: 20.0,
            per_everyone: 100.0,
            per_image: 20.0,
            per_file: 15.0,
            per_embed: 15.0,
            per_link: 10.0,
            per_sticker: 15.0,
            inactive_channel: 10.0,
            per_blacklisted_word: 40.0,
            suspicious_account: 10.0,
            similarity: 40.0,
            similarity_floor: 0.8,
            webhook_multiplier: 1.5,
            zalgo: 40.0,
            per_upper: 0.0,
            per_lower: 0.0,
            invite: 100.0,
            malicious: 100.0,
            nsfw: 100.0,
            blacklisted_link: 100.0,
        }
    }
}

/// Which filters the author is whitelisted from (the whitelist tabs).
#[derive(Debug, Clone, Copy, Default)]
pub struct Whitelist {
    pub spam: bool,
    /// @user and @role mentions.
    pub mention_spam: bool,
    pub advertising: bool,
    /// @everyone and @here.
    pub everyone: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MsgFacts<'a> {
    pub content: &'a str,
    pub emojis: usize,
    pub user_mentions: usize,
    /// @everyone / @here
    pub everyone_mentions: usize,
    pub role_mentions: usize,
    pub images: usize,
    /// Attachments that are not images.
    pub files: usize,
    pub embeds: usize,
    pub links: usize,
    pub stickers: usize,
    pub channel_inactive: bool,
    pub blacklisted_words: usize,
    pub invite_link: bool,
    pub nsfw_link: bool,
    pub malicious_link: bool,
    pub blacklisted_link: bool,
    pub from_webhook: bool,
    pub suspicious_account: bool,
    pub zalgo: bool,
    pub upper_chars: usize,
    pub lower_chars: usize,
    pub whitelist: Whitelist,
}

/// Recent messages from one user, newest last.
#[derive(Debug, Clone, Default)]
pub struct UserHistory {
    recent: VecDeque<String>,
    /// (channel, message) ids of the latest messages, for the purge after a strike.
    msgs: VecDeque<(u64, u64)>,
}

impl UserHistory {
    const KEEP: usize = 5;
    const KEEP_MSGS: usize = 50;

    pub fn track(&mut self, channel: u64, msg: u64) {
        if self.msgs.len() == Self::KEEP_MSGS {
            self.msgs.pop_front();
        }
        self.msgs.push_back((channel, msg));
    }

    pub fn take_msgs(&mut self) -> Vec<(u64, u64)> {
        self.msgs.drain(..).collect()
    }

    pub fn remember(&mut self, content: &str) {
        if self.recent.len() == Self::KEEP {
            self.recent.pop_front();
        }
        self.recent.push_back(content.to_lowercase());
    }

    fn best_similarity(&self, content: &str) -> f64 {
        let c = content.to_lowercase();
        self.recent
            .iter()
            .map(|r| similarity(r, &c))
            .fold(0.0, f64::max)
    }
}

/// Per-filter contributions of one message, in heat points.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Breakdown {
    pub parts: Vec<(&'static str, f64)>,
}

impl Breakdown {
    fn add(&mut self, name: &'static str, v: f64) {
        if v > 0.0 {
            self.parts.push((name, v));
        }
    }
    pub fn total(&self) -> f64 {
        self.parts.iter().map(|p| p.1).sum()
    }
}

/// Normalized Levenshtein similarity in 0.0..=1.0.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let n = a.len().max(b.len());
    if n == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f64 / n as f64
}

pub fn heat(m: &MsgFacts, hist: &UserHistory, cfg: &HeatCfg) -> Breakdown {
    let w = m.whitelist;
    let mut b = Breakdown::default();
    let hit = |on: bool, v: f64| if on { v } else { 0.0 };
    b.add("invites", hit(m.invite_link && !w.advertising, cfg.invite));
    b.add("malicious", hit(m.malicious_link, cfg.malicious));
    b.add("nsfw", hit(m.nsfw_link, cfg.nsfw));
    b.add(
        "links_blacklist",
        hit(m.blacklisted_link, cfg.blacklisted_link),
    );
    b.add("message", cfg.base);
    if !w.spam {
        let sim = hist.best_similarity(m.content);
        if sim >= cfg.similarity_floor && !m.content.is_empty() {
            b.add("similarity", cfg.similarity * sim);
        }
        b.add(
            "characters",
            cfg.per_char * m.content.chars().count().saturating_sub(cfg.free_chars) as f64,
        );
        b.add("uppercase", cfg.per_upper * m.upper_chars as f64);
        b.add("lowercase", cfg.per_lower * m.lower_chars as f64);
        if m.zalgo {
            b.add("zalgo", cfg.zalgo);
        }
        b.add(
            "newlines",
            cfg.per_newline * m.content.matches('\n').count() as f64,
        );
        b.add("emojis", cfg.per_emoji * m.emojis as f64);
        b.add("images", cfg.per_image * m.images as f64);
        b.add("files", cfg.per_file * m.files as f64);
        b.add("embeds", cfg.per_embed * m.embeds as f64);
        b.add("links", cfg.per_link * m.links as f64);
        b.add("stickers", cfg.per_sticker * m.stickers as f64);
        if m.channel_inactive {
            b.add("inactive_channel", cfg.inactive_channel);
        }
    }
    if !w.mention_spam {
        b.add("mentions", cfg.per_mention * m.user_mentions as f64);
        b.add(
            "role_mentions",
            cfg.per_role_mention * m.role_mentions as f64,
        );
    }
    if !w.everyone {
        b.add("everyone", cfg.per_everyone * m.everyone_mentions as f64);
    }
    b.add(
        "blacklisted_words",
        cfg.per_blacklisted_word * m.blacklisted_words as f64,
    );
    if m.suspicious_account {
        b.add("suspicious_account", cfg.suspicious_account);
    }
    if m.from_webhook {
        for p in &mut b.parts {
            p.1 *= cfg.webhook_multiplier;
        }
    }
    b
}

/// Quiet-channel detector for the inactive channel filter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelActivity {
    last_ms: i64,
    burst_left: u32,
}

impl ChannelActivity {
    /// True for the first `burst` messages after the channel was quiet for `quiet_hours`.
    pub fn on_message(&mut self, now: i64, burst: u32, quiet_hours: u32) -> bool {
        if self.last_ms > 0 && now - self.last_ms >= i64::from(quiet_hours.max(1)) * 3_600_000 {
            self.burst_left = burst;
        }
        self.last_ms = now;
        let inactive = self.burst_left > 0;
        self.burst_left = self.burst_left.saturating_sub(1);
        inactive
    }
}

/// A user's running heat. Decay is applied lazily when read.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeatState {
    pub value: f64,
    pub at_ms: i64,
}

impl HeatState {
    pub fn decayed(&self, now: i64, cfg: &HeatCfg) -> f64 {
        let dt = (now - self.at_ms).max(0) as f64 / 1000.0;
        self.value * (1.0 - cfg.decay_per_sec).powf(dt)
    }

    /// Apply a message's breakdown; returns true when the automod should trigger.
    pub fn apply(&mut self, b: &Breakdown, now: i64, cfg: &HeatCfg) -> bool {
        self.value = self.decayed(now, cfg) + b.total();
        self.at_ms = now;
        self.value >= cfg.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(content: &str) -> MsgFacts<'_> {
        MsgFacts {
            content,
            ..Default::default()
        }
    }

    #[test]
    fn history_hands_out_tracked_messages_once() {
        let mut h = UserHistory::default();
        (0..60).for_each(|i| h.track(1, i));
        let got = h.take_msgs();
        assert_eq!((got.len(), got[0].1), (UserHistory::KEEP_MSGS, 10));
        assert!(h.take_msgs().is_empty());
    }

    #[test]
    fn quiet_channel_flags_the_next_burst() {
        let mut c = ChannelActivity::default();
        assert!(!c.on_message(1, 2, 2));
        let later = 1 + 3_600_000;
        assert!(
            !c.on_message(later, 2, 2),
            "one hour is not quiet for a 2 h setting"
        );
        let later = later + 2 * 3_600_000;
        assert_eq!(
            [later, later + 1, later + 2].map(|t| c.on_message(t, 2, 2)),
            [true, true, false]
        );
    }

    #[test]
    fn decay_is_exponential() {
        let cfg = HeatCfg::default();
        let s = HeatState {
            value: 100.0,
            at_ms: 0,
        };
        assert!((s.decayed(2000, &cfg) - 81.0).abs() < 1e-9);
        assert_eq!(s.decayed(-5, &cfg), 100.0);
    }

    #[test]
    fn rapid_messages_trigger_slow_ones_do_not() {
        let cfg = HeatCfg::default();
        let hist = UserHistory::default();
        let mut fast = HeatState::default();
        let hits: Vec<bool> = (0..6)
            .map(|i| fast.apply(&heat(&msg("hi"), &hist, &cfg), i * 100, &cfg))
            .collect();
        assert!(!hits[0] && hits[5]);
        let mut slow = HeatState::default();
        assert!((0..20).all(|i| !slow.apply(&heat(&msg("hi"), &hist, &cfg), i * 10_000, &cfg)));
    }

    #[test]
    fn similar_messages_add_heat() {
        let cfg = HeatCfg::default();
        let mut h = UserHistory::default();
        h.remember("buy my cool stuff now");
        let b = heat(&msg("Buy my cool stuff now"), &h, &cfg);
        assert!(b.parts.iter().any(|p| p.0 == "similarity"));
        assert!(heat(&msg("something else entirely"), &h, &cfg)
            .parts
            .iter()
            .all(|p| p.0 != "similarity"));
    }

    #[test]
    fn invite_trips_at_once_unless_whitelisted() {
        let cfg = HeatCfg::default();
        let mut m = msg("discord.gg/x");
        m.invite_link = true;
        let mut s = HeatState::default();
        assert!(s.apply(&heat(&m, &UserHistory::default(), &cfg), 0, &cfg));
        m.whitelist.advertising = true;
        assert_eq!(heat(&m, &UserHistory::default(), &cfg).total(), cfg.base);
    }

    #[test]
    fn mention_and_whitelist_rules() {
        let cfg = HeatCfg::default();
        let mut m = msg("x");
        m.everyone_mentions = 1;
        m.role_mentions = 1;
        let full = heat(&m, &UserHistory::default(), &cfg).total();
        m.whitelist.mention_spam = true;
        assert!(heat(&m, &UserHistory::default(), &cfg).total() < full);
        m.whitelist.everyone = true;
        assert_eq!(heat(&m, &UserHistory::default(), &cfg).total(), cfg.base);
    }

    #[test]
    fn zalgo_and_case_weights() {
        let cfg = HeatCfg {
            per_upper: 2.0,
            per_lower: 1.0,
            ..Default::default()
        };
        let mut m = msg("x");
        m.zalgo = true;
        m.upper_chars = 3;
        m.lower_chars = 4;
        let b = heat(&m, &UserHistory::default(), &cfg);
        let part = |n| b.parts.iter().find(|p| p.0 == n).map(|p| p.1);
        assert_eq!(
            (part("uppercase"), part("lowercase"), part("zalgo")),
            (Some(6.0), Some(4.0), Some(40.0))
        );
    }

    #[test]
    fn webhook_is_harsher_and_breakdown_explains() {
        let cfg = HeatCfg::default();
        let mut m = msg("x\ny\nz");
        let normal = heat(&m, &UserHistory::default(), &cfg);
        m.from_webhook = true;
        let hook = heat(&m, &UserHistory::default(), &cfg);
        assert!(hook.total() > normal.total());
        assert!(normal.parts.iter().any(|p| p.0 == "newlines"));
    }
}
