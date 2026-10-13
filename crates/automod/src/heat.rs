//! Heat scoring. `heat()` is a pure function returning per-filter contributions so a log can
//! say *why* someone triggered.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub per_mass_mention: f64,
    pub per_attachment: f64,
    pub per_embed: f64,
    pub per_sticker: f64,
    pub inactive_channel: f64,
    pub per_blacklisted_word: f64,
    pub suspicious_account: f64,
    /// Added at full weight when similarity to a recent message is 1.0.
    pub similarity: f64,
    /// Only similarities above this count.
    pub similarity_floor: f64,
    pub webhook_multiplier: f64,
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
            per_mass_mention: 60.0,
            per_attachment: 10.0,
            per_embed: 8.0,
            per_sticker: 8.0,
            inactive_channel: 10.0,
            per_blacklisted_word: 40.0,
            suspicious_account: 10.0,
            similarity: 40.0,
            similarity_floor: 0.8,
            webhook_multiplier: 1.5,
        }
    }
}

/// Which filters the author is whitelisted from.
#[derive(Debug, Clone, Copy, Default)]
pub struct Whitelist {
    pub spam: bool,
    pub mention_spam: bool,
    pub advertising: bool,
    pub public_role_mention: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MsgFacts<'a> {
    pub content: &'a str,
    pub emojis: usize,
    pub user_mentions: usize,
    /// @everyone / @here
    pub everyone_mentions: usize,
    pub role_mentions: usize,
    pub attachments: usize,
    pub embeds: usize,
    pub stickers: usize,
    pub channel_inactive: bool,
    pub blacklisted_words: usize,
    pub invite_link: bool,
    pub nsfw_link: bool,
    pub malicious_link: bool,
    pub blacklisted_link: bool,
    pub from_webhook: bool,
    pub suspicious_account: bool,
    pub whitelist: Whitelist,
}

/// Recent messages from one user, newest last.
#[derive(Debug, Clone, Default)]
pub struct UserHistory {
    recent: VecDeque<String>,
}

impl UserHistory {
    const KEEP: usize = 5;

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
    /// Advert / NSFW / malicious / blacklisted link: punish immediately.
    pub instant_max: bool,
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
    let mut b = Breakdown {
        instant_max: (m.invite_link && !w.advertising)
            || m.nsfw_link
            || m.malicious_link
            || m.blacklisted_link,
        ..Default::default()
    };
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
        b.add(
            "newlines",
            cfg.per_newline * m.content.matches('\n').count() as f64,
        );
        b.add("emojis", cfg.per_emoji * m.emojis as f64);
        b.add("attachments", cfg.per_attachment * m.attachments as f64);
        b.add("embeds", cfg.per_embed * m.embeds as f64);
        b.add("stickers", cfg.per_sticker * m.stickers as f64);
        if m.channel_inactive {
            b.add("inactive_channel", cfg.inactive_channel);
        }
    }
    if !w.mention_spam {
        b.add("mentions", cfg.per_mention * m.user_mentions as f64);
    }
    let mass = m.everyone_mentions
        + if w.public_role_mention {
            0
        } else {
            m.role_mentions
        };
    if !w.mention_spam {
        b.add("mass_mentions", cfg.per_mass_mention * mass as f64);
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
        self.value = if b.instant_max {
            cfg.max
        } else {
            self.decayed(now, cfg) + b.total()
        };
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
    fn invite_is_instant_max_unless_whitelisted() {
        let cfg = HeatCfg::default();
        let mut m = msg("discord.gg/x");
        m.invite_link = true;
        let mut s = HeatState::default();
        assert!(s.apply(&heat(&m, &UserHistory::default(), &cfg), 0, &cfg));
        m.whitelist.advertising = true;
        assert!(!heat(&m, &UserHistory::default(), &cfg).instant_max);
    }

    #[test]
    fn mention_and_whitelist_rules() {
        let cfg = HeatCfg::default();
        let mut m = msg("x");
        m.everyone_mentions = 1;
        m.role_mentions = 1;
        let full = heat(&m, &UserHistory::default(), &cfg).total();
        m.whitelist.public_role_mention = true;
        assert!(heat(&m, &UserHistory::default(), &cfg).total() < full);
        m.whitelist.mention_spam = true;
        assert_eq!(heat(&m, &UserHistory::default(), &cfg).total(), cfg.base);
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
