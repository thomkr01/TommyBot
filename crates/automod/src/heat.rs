//! Heat scoring. `heat()` is a pure function returning per-filter contributions so a log can
//! say *why* someone triggered.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HeatCfg {
    pub max: f64,
    /// Fraction of total heat lost per second (0.08 = 8%/s, default).
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
            decay_per_sec: 0.08,
            base: 15.0,
            // Long messages heat through per_upper/per_lower alone, as the reference defaults do.
            per_char: 0.0,
            free_chars: 200,
            per_newline: 5.0,
            per_emoji: 9.0,
            per_mention: 15.0,
            per_role_mention: 20.0,
            per_everyone: 100.0,
            per_image: 20.0,
            per_file: 15.0,
            per_embed: 15.0,
            per_link: 10.0,
            per_sticker: 15.0,
            inactive_channel: 25.0,
            per_blacklisted_word: 100.0,
            suspicious_account: 7.0,
            similarity: 22.0,
            similarity_floor: 0.8,
            webhook_multiplier: 1.5,
            zalgo: 1.5,
            per_upper: 0.12,
            per_lower: 0.08,
            invite: 100.0,
            malicious: 100.0,
            nsfw: 100.0,
            blacklisted_link: 100.0,
        }
    }
}

/// Max heat: above 0, or every message trips.
pub const HEAT_MAX: (f64, f64) = (1.0, 100_000.0);
/// Decay per second: above 0 (heat that never cools never gets calm) and below 1 (a negative base
/// makes `powf` NaN).
pub const HEAT_DECAY: (f64, f64) = (0.001, 0.99);

impl HeatCfg {
    /// Max and decay pulled inside their ranges: config also arrives from the prefix panel and MCP.
    pub fn clamped(self) -> Self {
        let fit =
            |v: f64, (lo, hi): (f64, f64), d: f64| if v.is_finite() { v.clamp(lo, hi) } else { d };
        let d = Self::default();
        Self {
            max: fit(self.max, HEAT_MAX, d.max),
            decay_per_sec: fit(self.decay_per_sec, HEAT_DECAY, d.decay_per_sec),
            ..self
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
    /// Similarity is quadratic in length: long walls of text are compared by their start only.
    const CMP_CHARS: usize = 500;

    pub fn track(&mut self, channel: u64, msg: u64) {
        if self.msgs.len() == Self::KEEP_MSGS {
            self.msgs.pop_front();
        }
        self.msgs.push_back((channel, msg));
    }

    /// Whether a tracked message in `channel` is newer than `since_ms` (unix ms).
    pub fn posted_in(&self, channel: u64, since_ms: i64) -> bool {
        self.msgs
            .iter()
            .any(|&(c, m)| c == channel && ((m >> 22) + 1_420_070_400_000) as i64 > since_ms)
    }

    pub fn take_msgs(&mut self) -> Vec<(u64, u64)> {
        self.msgs.drain(..).collect()
    }

    pub fn remember(&mut self, content: &str) {
        if self.recent.len() == Self::KEEP {
            self.recent.pop_front();
        }
        self.recent.push_back(cmp_form(content));
    }

    fn best_similarity(&self, content: &str) -> f64 {
        let c = cmp_form(content);
        self.recent
            .iter()
            .map(|r| similarity(r, &c))
            .fold(0.0, f64::max)
    }
}

fn cmp_form(content: &str) -> String {
    content
        .chars()
        .take(UserHistory::CMP_CHARS)
        .collect::<String>()
        .to_lowercase()
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
    if !w.spam {
        b.add("message", cfg.base);
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
    if m.suspicious_account && !w.spam {
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

/// Auto slowmode: a channel whose pooled heat floods gets a slowmode until it is calm again.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AutoSlowmodeCfg {
    pub enabled: bool,
    /// Trigger: the channel's pooled heat reaching this % of max heat.
    pub channel_pct: u32,
    pub slowmode_secs: u32,
    pub min_on_secs: u32,
    /// Off once the pool stayed under half the trigger this long.
    pub calm_secs: u32,
}

impl Default for AutoSlowmodeCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_pct: 300,
            slowmode_secs: 5,
            min_on_secs: 60,
            calm_secs: 120,
        }
    }
}

/// At least two members' worth: one member adds at most max heat.
pub const SLOW_PCT: (i64, i64) = (200, 5_000);
/// Discord's maximum slowmode is 6 hours.
pub const SLOW_SECS: (i64, i64) = (1, 21_600);
pub const SLOW_ON: (i64, i64) = (10, 3_600);
pub const SLOW_CALM: (i64, i64) = (10, 3_600);

impl AutoSlowmodeCfg {
    /// Every value pulled inside its range: config also arrives from the prefix panel and MCP.
    pub fn clamped(self) -> Self {
        let fit = |v: u32, (lo, hi): (i64, i64)| v.clamp(lo as u32, hi as u32);
        Self {
            channel_pct: fit(self.channel_pct, SLOW_PCT),
            slowmode_secs: fit(self.slowmode_secs, SLOW_SECS),
            min_on_secs: fit(self.min_on_secs, SLOW_ON),
            calm_secs: fit(self.calm_secs, SLOW_CALM),
            ..self
        }
    }

    fn trigger(&self, h: &HeatCfg) -> f64 {
        h.max * f64::from(self.channel_pct) / 100.0
    }
}

/// One channel's pooled heat for auto slowmode.
#[derive(Debug, Default)]
pub struct ChannelHeat {
    heat: HeatState,
    /// Each member's part of the pool, at most max heat, so 300% always takes three members.
    shares: HashMap<u64, HeatState>,
    /// Max heat and decay the pool was filled with; a change starts it over.
    filled_with: (f64, f64),
    pruned_ms: i64,
    /// When the pool last fell (or will fall) under half the trigger.
    calm_since: i64,
    /// A slowmode was claimed and not released yet.
    on: bool,
    /// When the last one was released; the flap guard counts from here.
    off_ms: i64,
    /// Last failure log, so a channel the bot cannot slow logs once an hour.
    pub warned_ms: i64,
}

impl ChannelHeat {
    /// Shares kept before cold ones are dropped.
    const KEEP: usize = 1_000;

    /// Adds a message's heat; true once per flip, when the slowmode should start.
    pub fn on_message(
        &mut self,
        now: i64,
        user: u64,
        b: &Breakdown,
        cfg: &AutoSlowmodeCfg,
        h: &HeatCfg,
    ) -> bool {
        // Old heat under another max or decay would no longer be the sum of capped shares.
        if self.filled_with != (h.max, h.decay_per_sec) {
            self.filled_with = (h.max, h.decay_per_sec);
            self.shares.clear();
            self.heat.value = 0.0;
        }
        // At most once a second, so a channel with many hot members is not scanned per message.
        if self.shares.len() >= Self::KEEP && now - self.pruned_ms >= 1_000 {
            self.pruned_ms = now;
            self.shares.retain(|_, s| s.decayed(now, h) >= h.max * 0.01);
        }
        // Shares decay like the pool, so the pool stays their sum.
        let share = self.shares.entry(user).or_default();
        let old = share.decayed(now, h);
        let add = b.total().min(h.max - old).max(0.0);
        *share = HeatState {
            value: old + add,
            at_ms: now,
            ..*share
        };
        self.heat = HeatState {
            value: self.heat.decayed(now, h) + add,
            at_ms: now,
            ..self.heat
        };
        let (trigger, value) = (cfg.trigger(h), self.heat.value);
        if value >= trigger / 2.0 {
            self.calm_since = now.saturating_add(fall_ms(value, trigger / 2.0, h.decay_per_sec));
        }
        let guard_ms = i64::from(cfg.min_on_secs + cfg.calm_secs) * 1000;
        let start =
            !self.on && value >= trigger && (self.off_ms == 0 || now - self.off_ms >= guard_ms);
        self.on |= start;
        start
    }

    /// The pool has stayed under half the trigger for `calm_secs`.
    pub fn is_calm(&self, now: i64, cfg: &AutoSlowmodeCfg, h: &HeatCfg) -> bool {
        self.heat.decayed(now, h) < cfg.trigger(h) / 2.0
            && now - self.calm_since >= i64::from(cfg.calm_secs) * 1000
    }

    /// Ends a claimed slowmode (turned off, or nothing was started); the flap guard starts here.
    /// Not for a start that found the slowmode already ours: that one stays claimed until it ends.
    pub fn release(&mut self, now: i64) {
        self.on = false;
        self.off_ms = now;
    }

    /// Nothing worth keeping: no slowmode, no heat for 10 minutes and no flap guard left
    /// (at most min on + calm).
    pub fn idle(&self, now: i64) -> bool {
        let guard_ms = (SLOW_ON.1 + SLOW_CALM.1) * 1000;
        !self.on && now - self.heat.at_ms > 600_000 && now - self.off_ms > guard_ms
    }
}

/// Ms until `v` decays under `to`.
fn fall_ms(v: f64, to: f64, decay: f64) -> i64 {
    match decay {
        d if d >= 1.0 => 0,
        d if d <= 0.0 => i64::MAX,
        d => ((to / v).ln() / (1.0 - d).ln() * 1000.0).ceil() as i64,
    }
}

/// A user's running heat. Decay is applied lazily when read.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeatState {
    pub value: f64,
    pub at_ms: i64,
    /// When the last punishment started; 0 if none.
    pub punished_ms: i64,
}

/// Messages already on their way when a punishment lands keep arriving for a moment.
pub const PUNISH_GRACE_MS: i64 = 10_000;

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

    /// Claims the punishment for a trigger; false while the last one is under `PUNISH_GRACE_MS` old,
    /// so a burst costs one strike, not one per message in flight.
    pub fn claim(&mut self, now: i64) -> bool {
        if self.punished_ms > 0 && now - self.punished_ms < PUNISH_GRACE_MS {
            return false;
        }
        self.punished_ms = now;
        true
    }
}

/// One message in a [`Crowd`].
#[derive(Debug)]
pub struct Post {
    pub user: u64,
    pub name: String,
    pub channel: u64,
    pub msg: u64,
}

#[derive(Debug, Default)]
struct Group {
    posts: Vec<(Post, i64)>,
    /// Shared heat of everyone who sent this text.
    heat: HeatState,
    tripped_ms: i64,
}

/// The same text from several members within seconds: a spam raid where every account stays under
/// its own heat. Their heat adds up in one shared pool, and when it trips every one of them is
/// punished.
#[derive(Debug, Default)]
pub struct Crowd {
    /// (guild, text hash) -> the members who sent that text lately.
    seen: HashMap<(u64, u64), Group>,
}

impl Crowd {
    const USERS: usize = 3;
    const WINDOW_MS: i64 = 30_000;
    /// Short texts ("gg", "hi") are said by many at once, so only the pool catches those; three
    /// members are enough for longer texts and links.
    const MIN_CHARS: usize = 30;

    /// Adds a post's heat (`b`) to its text's shared pool. When two or more members' combined heat
    /// reaches the max, three members sent a long text or link, or it tripped within the window,
    /// returns the group's posts not returned before (this one included).
    pub fn on_text(
        &mut self,
        guild: u64,
        post: Post,
        text: &str,
        b: &Breakdown,
        now: i64,
        cfg: &HeatCfg,
    ) -> Option<Vec<Post>> {
        use std::hash::{Hash, Hasher};
        let norm: String = crate::filters::cure(text)
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        let long =
            norm.chars().count() >= Self::MIN_CHARS || !crate::filters::urls(text).is_empty();
        if norm.is_empty() && !long {
            return None;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        norm.hash(&mut h);
        let fresh = |g: &mut Group| g.posts.retain(|&(_, at)| now - at < Self::WINDOW_MS);
        if self.seen.len() > 50_000 {
            self.seen.retain(|_, g| {
                fresh(g);
                !g.posts.is_empty() || now - g.tripped_ms < Self::WINDOW_MS
            });
        }
        let g = self.seen.entry((guild, h.finish())).or_default();
        fresh(g);
        g.posts.push((post, now));
        let pool = g.heat.apply(b, now, cfg);
        let mut users: Vec<u64> = g.posts.iter().map(|p| p.0.user).collect();
        users.sort_unstable();
        users.dedup();
        let tripped = (users.len() >= 2 && pool)
            || (long && users.len() >= Self::USERS)
            || (g.tripped_ms > 0 && now - g.tripped_ms < Self::WINDOW_MS);
        if !tripped {
            return None;
        }
        g.tripped_ms = now;
        g.heat = HeatState::default();
        Some(g.posts.drain(..).map(|p| p.0).collect())
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
            ..Default::default()
        };
        assert!((s.decayed(2000, &cfg) - 84.64).abs() < 1e-9);
        assert_eq!(s.decayed(-5, &cfg), 100.0);
    }

    #[test]
    fn rapid_messages_trigger_slow_ones_do_not() {
        let cfg = HeatCfg::default();
        let hist = UserHistory::default();
        let mut fast = HeatState::default();
        let hits: Vec<bool> = (0..7)
            .map(|i| fast.apply(&heat(&msg("hi"), &hist, &cfg), i * 100, &cfg))
            .collect();
        assert!(!hits[0] && hits[6]);
        let mut slow = HeatState::default();
        assert!((0..20).all(|i| !slow.apply(&heat(&msg("hi"), &hist, &cfg), i * 10_000, &cfg)));
    }

    #[test]
    fn spam_whitelist_skips_message_heat() {
        let cfg = HeatCfg::default();
        let mut m = msg("hi");
        m.suspicious_account = true;
        m.whitelist.spam = true;
        let mut s = HeatState::default();
        let hist = UserHistory::default();
        assert!((0..50).all(|i| !s.apply(&heat(&m, &hist, &cfg), i * 50, &cfg)));
    }

    #[test]
    fn five_emojis_are_not_spam() {
        let cfg = HeatCfg::default();
        let mut m = msg("🎉🎉🎉🎉🎉");
        m.emojis = 5;
        assert!(!HeatState::default().apply(&heat(&m, &UserHistory::default(), &cfg), 0, &cfg));
    }

    #[test]
    fn one_punishment_per_burst() {
        let mut s = HeatState::default();
        assert!(s.claim(1_000));
        assert!(!s.claim(1_500), "a message in flight is not a new offence");
        assert!(s.claim(1_000 + PUNISH_GRACE_MS));
    }

    #[test]
    fn posted_in_reads_snowflake_time() {
        let mut h = UserHistory::default();
        let at = |ms: i64| ((ms - 1_420_070_400_000) as u64) << 22;
        h.track(7, at(1_700_000_000_000));
        assert!(h.posted_in(7, 1_699_999_999_000));
        assert!(!h.posted_in(7, 1_700_000_001_000));
        assert!(!h.posted_in(8, 0));
    }

    #[test]
    fn long_walls_compare_by_their_start() {
        let cfg = HeatCfg::default();
        let wall = "spam ".repeat(800);
        let mut h = UserHistory::default();
        (0..5).for_each(|_| h.remember(&wall));
        let t = std::time::Instant::now();
        assert!(heat(&msg(&wall), &h, &cfg)
            .parts
            .iter()
            .any(|p| p.0 == "similarity"));
        assert!(t.elapsed().as_millis() < 500);
    }

    /// What the bot does per message: quiet-channel bonus once per user, then heat, then history.
    fn send(
        users: &mut std::collections::HashMap<u64, (HeatState, UserHistory)>,
        chan: &mut ChannelActivity,
        (user, text, now): (u64, &str, i64),
        cfg: &HeatCfg,
    ) -> bool {
        let (s, h) = users.entry(user).or_default();
        let mut m = msg(text);
        m.channel_inactive = chan.on_message(now, 10, 1) && !h.posted_in(1, now - 3_600_000);
        let b = heat(&m, h, cfg);
        h.remember(text);
        h.track(1, ((now - 1_420_070_400_000) as u64) << 22);
        s.apply(&b, now, cfg)
    }

    #[test]
    fn spammers_trip_while_chatters_in_the_same_channel_do_not() {
        let cfg = HeatCfg::default();
        let (mut users, mut chan) = (Default::default(), ChannelActivity::default());
        let t0 = 1_700_000_000_000;
        let mut tripped = std::collections::HashSet::new();
        for i in 0..30 {
            let now = t0 + i * 200;
            for spammer in [1, 2] {
                if send(&mut users, &mut chan, (spammer, "BUY NOW", now), &cfg) {
                    tripped.insert(spammer);
                }
            }
            // Chatters post every 2 s, the same short line as each other.
            if i % 10 == 0 {
                for chatter in [3, 4, 5] {
                    if send(&mut users, &mut chan, (chatter, "gg", now), &cfg) {
                        tripped.insert(chatter);
                    }
                }
            }
        }
        assert_eq!(tripped, [1, 2].into());
    }

    #[test]
    fn gg_twice_after_a_quiet_hour_is_not_spam() {
        let cfg = HeatCfg::default();
        let (mut users, mut chan) = (Default::default(), ChannelActivity::default());
        let t0 = 1_700_000_000_000;
        send(&mut users, &mut chan, (9, "hi", t0), &cfg);
        let t = t0 + 2 * 3_600_000;
        assert!(!send(&mut users, &mut chan, (1, "gg", t), &cfg));
        assert!(!send(&mut users, &mut chan, (1, "gg", t + 1000), &cfg));
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
            (Some(6.0), Some(4.0), Some(1.5))
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

    #[test]
    fn crowd_pools_heat_and_returns_every_member() {
        let cfg = HeatCfg::default();
        let mut c = Crowd::default();
        let post = |user, msg| Post {
            user,
            name: String::new(),
            channel: 1,
            msg,
        };
        let some = |v: f64| Breakdown {
            parts: vec![("base", v)],
        };
        let spam = "join my server for free nitro giveaway now!!";
        let on = |c: &mut Crowd, g, u, text, v, now| {
            c.on_text(g, post(u, now as u64), text, &some(v), now, &cfg)
                .map(|p| p.iter().map(|p| p.user).collect::<Vec<_>>())
        };
        // One member alone never trips the pool, however hot.
        assert_eq!(on(&mut c, 1, 10, spam, 60.0, 0), None);
        assert_eq!(on(&mut c, 1, 10, spam, 60.0, 1), None);
        // A second member's heat adds to the pool: both members (all their posts) come back.
        assert_eq!(
            on(
                &mut c,
                1,
                11,
                "JOIN my server for FREE nitro giveaway now",
                1.0,
                2
            ),
            Some(vec![10, 10, 11])
        );
        // Joining in right after is flagged too, alone.
        assert_eq!(on(&mut c, 1, 12, spam, 0.0, 3), Some(vec![12]));
        assert_eq!(
            on(&mut c, 2, 13, spam, 0.0, 4),
            None,
            "other guilds are separate"
        );
        // Three members trip it without heat.
        assert_eq!(on(&mut c, 1, 20, spam, 0.0, 100_000), None);
        assert_eq!(on(&mut c, 1, 21, spam, 0.0, 100_001), None);
        assert_eq!(
            on(&mut c, 1, 22, spam, 0.0, 100_002),
            Some(vec![20, 21, 22])
        );
        // Short text only through the pool: three members saying "gg" is chat, their heat is not.
        for u in 30..33 {
            assert_eq!(on(&mut c, 1, u, "gg", 15.0, 200_000 + u as i64), None);
        }
        assert_eq!(
            on(&mut c, 1, 33, "gg", 60.0, 200_040),
            Some(vec![30, 31, 32, 33])
        );
        assert_eq!(on(&mut c, 1, 34, "", 90.0, 250_000), None, "no text");
        assert_eq!(on(&mut c, 1, 35, "", 90.0, 250_001), None);
        assert_eq!(on(&mut c, 1, 40, "https://x.example", 0.0, 300_000), None);
        assert_eq!(on(&mut c, 1, 41, "https://x.example", 0.0, 300_001), None);
        assert!(on(&mut c, 1, 42, "https://x.example", 0.0, 300_002).is_some());
    }

    fn hot(total: f64) -> Breakdown {
        Breakdown {
            parts: vec![("message", total)],
        }
    }

    /// Index of the message that started a slowmode, if one did.
    fn flood(c: &mut ChannelHeat, users: u64, every_ms: i64, from: i64, n: i64) -> Option<i64> {
        let (cfg, h) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        let b = heat(
            &msg("hello there, how is everyone doing"),
            &UserHistory::default(),
            &h,
        );
        (0..n).find(|&i| c.on_message(from + i * every_ms, i as u64 % users, &b, &cfg, &h))
    }

    #[test]
    fn one_member_never_fills_the_pool_alone() {
        let (cfg, h) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        let mut c = ChannelHeat::default();
        // Fast messages, and one message far over max heat: still one member's worth.
        assert!((0..200).all(|i| !c.on_message(i * 50, 7, &hot(100.0), &cfg, &h)));
        assert!(!c.on_message(10_000, 7, &hot(10_000.0), &cfg, &h));
        // Two members: 200%, under the 300% trigger.
        assert!(!c.on_message(10_000, 8, &hot(10_000.0), &cfg, &h));
        // A third heavy member at the same moment reaches 300%.
        assert!(c.on_message(10_000, 9, &hot(10_000.0), &cfg, &h));
    }

    #[test]
    fn cold_shares_are_pruned_at_most_once_a_second() {
        let (cfg, h) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        let mut c = ChannelHeat::default();
        for u in 0..ChannelHeat::KEEP as u64 {
            c.on_message(0, u, &hot(0.5), &cfg, &h);
        }
        // 0.5 is under 1% of max heat: all cold, dropped on the next message.
        c.on_message(1_000, 9_999, &hot(50.0), &cfg, &h);
        assert_eq!(c.shares.len(), 1);
    }

    #[test]
    fn a_new_max_heat_starts_the_pool_over() {
        let (cfg, h) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        let mut c = ChannelHeat::default();
        c.on_message(0, 1, &hot(100.0), &cfg, &h);
        let low = HeatCfg {
            max: 30.0,
            ..HeatCfg::default()
        };
        // One member's heat from before must not count against the lower trigger (90).
        assert!(!c.on_message(1, 2, &hot(0.0), &cfg, &low));
        assert_eq!(c.heat.value, 0.0);
    }

    #[test]
    fn a_share_refills_as_it_decays() {
        let (cfg, h) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        let mut c = ChannelHeat::default();
        c.on_message(0, 1, &hot(100.0), &cfg, &h);
        // 10 s later the share decayed to about 43, so the next message may add about 57.
        c.on_message(10_000, 1, &hot(100.0), &cfg, &h);
        assert!(
            (c.heat.value - 100.0).abs() < 1e-6,
            "pool is the sum of the shares"
        );
    }

    #[test]
    fn four_members_flooding_start_it_once() {
        let mut c = ChannelHeat::default();
        // Four members, a message every 250 ms between them.
        flood(&mut c, 4, 250, 1, 40).expect("a flood starts it");
        assert_eq!(flood(&mut c, 4, 250, 20_000, 200), None, "once per flip");
    }

    #[test]
    fn normal_chat_stays_under_the_trigger() {
        let mut c = ChannelHeat::default();
        // Six members, one message every two seconds, for ten minutes.
        assert_eq!(flood(&mut c, 6, 2_000, 1, 300), None);
    }

    #[test]
    fn calm_after_the_pool_stays_low_and_flap_guard() {
        let (cfg, h) = (AutoSlowmodeCfg::default(), HeatCfg::default());
        let mut c = ChannelHeat::default();
        let t = flood(&mut c, 4, 250, 1, 40).unwrap() * 250 + 1;
        assert!(!c.is_calm(t, &cfg, &h), "still hot");
        // From ~600 the pool needs about 18 s to fall under 150, then 120 s of calm.
        assert!(!c.is_calm(t + 100_000, &cfg, &h));
        assert!(c.is_calm(t + 140_000, &cfg, &h));
        c.release(t + 140_000);
        let back = t + 150_000;
        assert_eq!(flood(&mut c, 4, 250, back, 400), None, "flap guard");
        assert!(flood(&mut c, 4, 250, t + 140_000 + 180_000, 40).is_some());
        // A fresh channel (also after a restart) is calm.
        assert!(ChannelHeat::default().is_calm(1_700_000_000_000, &cfg, &h));
    }

    #[test]
    fn flap_guard_counts_from_the_real_off() {
        let mut c = ChannelHeat::default();
        let t = flood(&mut c, 4, 250, 1, 40).unwrap() * 250 + 1;
        // A second start that found the slowmode already on leaves the claim alone: no new trip.
        assert_eq!(flood(&mut c, 4, 250, t, 2_000), None, "still claimed");
        // The real off, much later, starts the guard (min on 60 s + calm 120 s).
        let off = t + 1_000_000;
        c.release(off);
        assert_eq!(flood(&mut c, 4, 250, off + 1_000, 400), None, "flap guard");
        assert!(flood(&mut c, 4, 250, off + 180_000, 40).is_some());
        // A second release (the off job after a failed start released it) moves the guard too.
        let mut c = ChannelHeat::default();
        c.release(1_000);
        c.release(500_000);
        assert_eq!(flood(&mut c, 4, 250, 501_000, 400), None);
    }

    #[test]
    fn heat_max_and_decay_are_clamped() {
        let bad = |max, decay_per_sec| {
            HeatCfg {
                max,
                decay_per_sec,
                ..Default::default()
            }
            .clamped()
        };
        let c = bad(0.0, 10.0);
        assert_eq!((c.max, c.decay_per_sec), (1.0, 0.99));
        let c = bad(-5.0, 0.0);
        assert_eq!((c.max, c.decay_per_sec), (1.0, 0.001));
        let c = bad(f64::NAN, f64::INFINITY);
        assert_eq!((c.max, c.decay_per_sec), (100.0, 0.08));
        let d = HeatCfg::default().clamped();
        assert_eq!((d.max, d.decay_per_sec), (100.0, 0.08));
        // Clamped values keep the heat finite and a channel able to calm down.
        let h = bad(0.0, 1.0);
        let s = HeatState {
            value: 50.0,
            ..Default::default()
        };
        assert!(s.decayed(10_000, &h).is_finite());
        assert!(fall_ms(600.0, 150.0, bad(100.0, 0.0).decay_per_sec) < i64::MAX);
    }

    #[test]
    fn slowmode_values_are_clamped() {
        let c = AutoSlowmodeCfg {
            enabled: true,
            channel_pct: 0,
            slowmode_secs: u32::MAX,
            min_on_secs: 0,
            calm_secs: 1_000_000,
        }
        .clamped();
        assert_eq!(
            (
                c.enabled,
                c.channel_pct,
                c.slowmode_secs,
                c.min_on_secs,
                c.calm_secs
            ),
            (true, 200, 21_600, 10, 3_600)
        );
        let d = AutoSlowmodeCfg::default();
        assert_eq!(d.clone().clamped().channel_pct, d.channel_pct);
    }
}
