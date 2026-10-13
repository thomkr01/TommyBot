//! Join gate, join-raid detection and verification mode selection.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

pub fn account_created_ms(user_id: u64) -> i64 {
    (user_id >> 22) as i64 + DISCORD_EPOCH_MS
}

/// Ordered by severity; the gate applies the harshest action any filter asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Action {
    Log,
    /// Milliseconds. For the age filter this is replaced by the remaining time.
    Timeout(i64),
    Kick,
    Ban,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    NoAvatar,
    AccountAge,
    UnauthorizedBot,
    UnverifiedBot,
    InviteInName,
    Suspicious,
    UsernameFilter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub no_avatar: Option<Action>,
    pub min_age: Option<(i64, Action)>,
    pub unauthorized_bot: Option<Action>,
    pub unverified_bot: Option<Action>,
    pub invite_in_name: Option<Action>,
    pub suspicious: Option<Action>,
    pub username_filter: Option<Action>,
    /// Terms checked against the sanitized name; `*` is a wildcard, no `*` means "contains".
    pub username_terms: Vec<String>,
    /// Join Guard style: act only when every enabled filter matches, with the harshest action.
    pub require_all: bool,
}

fn yes() -> bool {
    true
}

impl Default for GateCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            no_avatar: None,
            min_age: None,
            unauthorized_bot: None,
            unverified_bot: None,
            invite_in_name: None,
            suspicious: None,
            username_filter: None,
            username_terms: vec![],
            require_all: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Joiner<'a> {
    pub id: u64,
    /// Already passed through confusable/Unicode sanitizing (e.g. `decancer`) by the caller.
    pub name: &'a str,
    pub has_avatar: bool,
    pub is_bot: bool,
    pub bot_verified: bool,
    /// For bots: whoever added it is Owner/EO/TA or above the bot.
    pub added_by_authorized: bool,
}

/// `*`-wildcard match, case-insensitive. A term without `*` matches as a substring.
pub fn term_matches(term: &str, name: &str) -> bool {
    let (term, name) = (term.to_lowercase(), name.to_lowercase());
    if !term.contains('*') {
        return name.contains(&term);
    }
    let parts: Vec<&str> = term.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

fn has_invite(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "discord.gg/",
        "discord.com/invite/",
        "discordapp.com/invite/",
    ]
    .iter()
    .any(|p| n.contains(p))
}

/// Suspicious = default avatar, young account or username-filter match (configurable OR).
pub fn is_suspicious(j: &Joiner, cfg: &GateCfg, min_age_ms: i64, now: i64) -> bool {
    !j.has_avatar
        || now - account_created_ms(j.id) < min_age_ms
        || cfg.username_terms.iter().any(|t| term_matches(t, j.name))
}

/// Filters that matched, with the action each wants.
pub fn gate(j: &Joiner, cfg: &GateCfg, now: i64) -> Vec<(Filter, Action)> {
    if !cfg.enabled {
        return vec![];
    }
    let age = now - account_created_ms(j.id);
    let mut enabled = 0;
    let mut hits = vec![];
    let mut check = |on: Option<Action>, f: Filter, hit: bool| {
        if let Some(a) = on {
            enabled += 1;
            if hit {
                hits.push((f, a));
            }
        }
    };
    check(cfg.no_avatar, Filter::NoAvatar, !j.has_avatar);
    check(
        cfg.min_age.map(|m| m.1),
        Filter::AccountAge,
        cfg.min_age.is_some_and(|(min, _)| age < min.max(5_000)),
    );
    check(
        cfg.unauthorized_bot,
        Filter::UnauthorizedBot,
        j.is_bot && !j.added_by_authorized,
    );
    check(
        cfg.unverified_bot,
        Filter::UnverifiedBot,
        j.is_bot && !j.bot_verified,
    );
    check(cfg.invite_in_name, Filter::InviteInName, has_invite(j.name));
    let min = cfg.min_age.map_or(0, |m| m.0);
    check(
        cfg.suspicious,
        Filter::Suspicious,
        is_suspicious(j, cfg, min, now),
    );
    check(
        cfg.username_filter,
        Filter::UsernameFilter,
        cfg.username_terms.iter().any(|t| term_matches(t, j.name)),
    );
    // Age timeouts only last for the remaining period.
    for h in &mut hits {
        if let (Filter::AccountAge, Action::Timeout(_)) = *h {
            h.1 = Action::Timeout(cfg.min_age.map_or(0, |m| m.0) - age);
        }
    }
    if cfg.require_all && hits.len() < enabled {
        hits.clear();
    }
    hits
}

pub fn harshest(hits: &[(Filter, Action)]) -> Option<Action> {
    hits.iter().map(|h| h.1).max()
}

// ---- Join raid ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StringOp {
    Exact,
    Exclude,
    Include,
    EndsWith,
}

/// Parse `=exact`, `!exclude`, `'include`, `$endswith`; a bare string is an include.
pub fn parse_pattern(p: &str) -> (StringOp, String) {
    let op = match p.chars().next() {
        Some('=') => StringOp::Exact,
        Some('!') => StringOp::Exclude,
        Some('\'') => StringOp::Include,
        Some('$') => StringOp::EndsWith,
        _ => return (StringOp::Include, p.to_lowercase()),
    };
    (op, p[1..].to_lowercase())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaidCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub count: usize,
    pub span_ms: i64,
    /// Count only suspicious accounts toward the static trigger.
    pub suspicious_only: bool,
    /// How long the raid state stays active once triggered.
    pub active_ms: i64,
    pub string_patterns: Vec<String>,
    pub flag_younger_than_ms: Option<i64>,
    pub flag_no_avatar: bool,
    /// Minimum accounts in one creation-time bucket to call it a bot farm; None disables.
    pub id_flag_min: Option<usize>,
    /// What happens to flagged raiders.
    #[serde(default = "default_raid_action")]
    pub action: Action,
    /// Also lock the server (join lock) when a raid is detected.
    #[serde(default)]
    pub lock: Option<crate::lockdown::LockKind>,
}

fn default_raid_action() -> Action {
    Action::Kick
}

impl Default for RaidCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            count: 10,
            span_ms: 10 * 60_000,
            suspicious_only: false,
            active_ms: 10 * 60_000,
            string_patterns: vec![],
            flag_younger_than_ms: None,
            flag_no_avatar: false,
            id_flag_min: None,
            action: Action::Kick,
            lock: None,
        }
    }
}

#[derive(Debug, Clone)]
struct JoinRec {
    id: u64,
    at: i64,
    name: String,
    has_avatar: bool,
    suspicious: bool,
}

#[derive(Debug, Default)]
pub struct RaidDetector {
    joins: Vec<JoinRec>,
    active_until: i64,
}

#[derive(Debug, Default, PartialEq)]
pub struct RaidOutcome {
    /// This join started the raid.
    pub triggered: bool,
    /// Accounts to punish now: on trigger every flagged joiner in the window, afterwards just this joiner.
    pub flagged: Vec<u64>,
}

impl RaidDetector {
    pub fn active(&self, now: i64) -> bool {
        now < self.active_until
    }

    pub fn on_join(
        &mut self,
        j: &Joiner,
        suspicious: bool,
        cfg: &RaidCfg,
        now: i64,
    ) -> RaidOutcome {
        self.joins.retain(|r| now - r.at <= cfg.span_ms);
        self.joins.push(JoinRec {
            id: j.id,
            at: now,
            name: j.name.to_lowercase(),
            has_avatar: j.has_avatar,
            suspicious,
        });
        let counted = self
            .joins
            .iter()
            .filter(|r| r.suspicious || !cfg.suspicious_only)
            .count();
        let was_active = self.active(now);
        if !was_active && counted >= cfg.count {
            self.active_until = now + cfg.active_ms;
            return RaidOutcome {
                triggered: true,
                flagged: self.flagged(cfg, now),
            };
        }
        if was_active {
            let me = self.flagged(cfg, now).contains(&j.id);
            return RaidOutcome {
                triggered: false,
                flagged: if me { vec![j.id] } else { vec![] },
            };
        }
        RaidOutcome::default()
    }

    /// Accounts in the window that any enabled algorithm flags.
    fn flagged(&self, cfg: &RaidCfg, now: i64) -> Vec<u64> {
        let farm = cfg.id_flag_min.map(|min| self.farm_ids(min));
        self.joins
            .iter()
            .filter(|r| {
                string_flag(&r.name, &cfg.string_patterns)
                    || cfg
                        .flag_younger_than_ms
                        .is_some_and(|y| now - account_created_ms(r.id) < y)
                    || (cfg.flag_no_avatar && !r.has_avatar)
                    || farm.as_ref().is_some_and(|f| f.contains(&r.id))
            })
            .map(|r| r.id)
            .collect()
    }

    /// ID flag: bucket joiners by account creation time (days; hours when one day holds over half the window).
    fn farm_ids(&self, min: usize) -> Vec<u64> {
        let bucket_by = |size: i64| {
            let mut m: HashMap<i64, Vec<u64>> = HashMap::new();
            for r in &self.joins {
                m.entry(account_created_ms(r.id) / size)
                    .or_default()
                    .push(r.id);
            }
            m
        };
        let mut buckets = bucket_by(DAY_MS);
        if buckets.values().any(|v| v.len() * 2 > self.joins.len()) {
            buckets = bucket_by(HOUR_MS);
        }
        buckets
            .into_values()
            .filter(|v| v.len() >= min)
            .flatten()
            .collect()
    }
}

/// Include/Exact/EndsWith patterns flag; any matching Exclude pattern vetoes.
fn string_flag(name: &str, patterns: &[String]) -> bool {
    let mut flagged = false;
    for p in patterns {
        let (op, s) = parse_pattern(p);
        let hit = match op {
            StringOp::Exact => name == s,
            StringOp::Include | StringOp::Exclude => name.contains(&s),
            StringOp::EndsWith => name.ends_with(&s),
        };
        if op == StringOp::Exclude {
            if hit {
                return false;
            }
        } else {
            flagged |= hit;
        }
    }
    flagged
}

// ---- Verification ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyMode {
    Button,
    Captcha,
    SimpleCaptcha,
    Dm,
    Web,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyTarget {
    Everyone,
    SuspiciousOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailAction {
    Nothing,
    Kick,
    Ban,
    Quarantine,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyCfg {
    pub mode: VerifyMode,
    pub target: VerifyTarget,
    pub time_limit_ms: i64,
    pub on_fail: FailAction,
}

impl Default for VerifyCfg {
    fn default() -> Self {
        Self {
            mode: VerifyMode::Captcha,
            target: VerifyTarget::Everyone,
            time_limit_ms: 120_000,
            on_fail: FailAction::Nothing,
        }
    }
}

/// Button mode is upgraded to captcha while a raid is active.
pub fn effective_mode(cfg: &VerifyCfg, raid_active: bool) -> VerifyMode {
    if raid_active && cfg.mode == VerifyMode::Button {
        VerifyMode::Captcha
    } else {
        cfg.mode
    }
}

/// Members who already hold a main role count as verified.
pub fn must_verify(cfg: &VerifyCfg, suspicious: bool, has_main_role: bool) -> bool {
    !has_main_role && (cfg.target == VerifyTarget::Everyone || suspicious)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    /// Snowflake for an account created `age_ms` before NOW (+ `salt` as worker bits).
    fn id_aged(age_ms: i64, salt: u64) -> u64 {
        (((NOW - age_ms - DISCORD_EPOCH_MS) as u64) << 22) | salt
    }

    fn joiner(id: u64, name: &str, has_avatar: bool) -> Joiner<'_> {
        Joiner {
            id,
            name,
            has_avatar,
            ..Default::default()
        }
    }

    #[test]
    fn account_age_from_snowflake() {
        assert_eq!(account_created_ms(id_aged(5 * DAY_MS, 7)), NOW - 5 * DAY_MS);
    }

    #[test]
    fn wildcard_terms() {
        assert!(term_matches("free*nitro", "FREE gift NITRO"));
        assert!(term_matches("spam", "xxSPAMxx"));
        assert!(!term_matches("a*b*c", "acb"));
        assert!(!term_matches("ab*ba", "aba"));
    }

    #[test]
    fn gate_picks_harshest_and_age_timeout_is_remaining() {
        let cfg = GateCfg {
            no_avatar: Some(Action::Log),
            min_age: Some((DAY_MS, Action::Timeout(0))),
            username_filter: Some(Action::Ban),
            username_terms: vec!["nitro*".into()],
            ..Default::default()
        };
        let j = joiner(id_aged(DAY_MS / 2, 0), "clean", true);
        let hits = gate(&j, &cfg, NOW);
        assert_eq!(
            hits,
            vec![(Filter::AccountAge, Action::Timeout(DAY_MS / 2))]
        );
        let j = joiner(id_aged(DAY_MS / 2, 0), "nitro giveaway", false);
        assert_eq!(harshest(&gate(&j, &cfg, NOW)), Some(Action::Ban));
    }

    #[test]
    fn disabled_gate_matches_nothing() {
        let cfg = GateCfg {
            enabled: false,
            no_avatar: Some(Action::Ban),
            ..Default::default()
        };
        assert!(gate(&joiner(id_aged(1000, 0), "a", false), &cfg, NOW).is_empty());
        assert!(
            serde_json::from_str::<GateCfg>("{\"username_terms\":[],\"require_all\":false}")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn require_all_needs_every_filter() {
        let mut cfg = GateCfg {
            no_avatar: Some(Action::Kick),
            min_age: Some((DAY_MS, Action::Kick)),
            require_all: true,
            ..Default::default()
        };
        assert!(gate(&joiner(id_aged(10 * DAY_MS, 0), "a", false), &cfg, NOW).is_empty());
        assert_eq!(
            gate(&joiner(id_aged(1000, 0), "a", false), &cfg, NOW).len(),
            2
        );
        cfg.require_all = false;
        assert_eq!(
            gate(&joiner(id_aged(10 * DAY_MS, 0), "a", false), &cfg, NOW).len(),
            1
        );
    }

    #[test]
    fn bots_and_invite_names() {
        let cfg = GateCfg {
            unauthorized_bot: Some(Action::Kick),
            invite_in_name: Some(Action::Ban),
            ..Default::default()
        };
        let bot = Joiner {
            id: id_aged(DAY_MS, 0),
            name: "bot",
            is_bot: true,
            has_avatar: true,
            ..Default::default()
        };
        assert_eq!(harshest(&gate(&bot, &cfg, NOW)), Some(Action::Kick));
        let j = joiner(id_aged(DAY_MS, 0), "join discord.gg/abc", true);
        assert_eq!(harshest(&gate(&j, &cfg, NOW)), Some(Action::Ban));
    }

    #[test]
    fn pattern_operators() {
        let pats: Vec<String> = ["=bob", "$_bot", "'raid", "!safe"]
            .map(String::from)
            .to_vec();
        assert!(string_flag("bob", &pats));
        assert!(!string_flag("bobby", &pats));
        assert!(string_flag("user_bot", &pats));
        assert!(string_flag("xraidx", &pats));
        assert!(!string_flag("safe raid", &pats));
    }

    fn raid_cfg() -> RaidCfg {
        RaidCfg {
            count: 3,
            span_ms: 60_000,
            active_ms: 600_000,
            ..Default::default()
        }
    }

    #[test]
    fn static_trigger_flags_whole_window() {
        let cfg = RaidCfg {
            flag_no_avatar: true,
            ..raid_cfg()
        };
        let mut d = RaidDetector::default();
        assert_eq!(
            d.on_join(&joiner(1, "a", false), false, &cfg, NOW),
            RaidOutcome::default()
        );
        d.on_join(&joiner(2, "b", true), false, &cfg, NOW + 1000);
        let out = d.on_join(&joiner(3, "c", false), false, &cfg, NOW + 2000);
        assert!(out.triggered);
        assert_eq!(out.flagged, vec![1, 3]);
        assert!(d.active(NOW + 3000));
        // while active only the new joiner is judged
        let out = d.on_join(&joiner(4, "d", false), false, &cfg, NOW + 4000);
        assert_eq!((out.triggered, out.flagged), (false, vec![4]));
        assert!(!d.active(NOW + 700_000));
    }

    #[test]
    fn suspicious_only_counting() {
        let cfg = RaidCfg {
            suspicious_only: true,
            ..raid_cfg()
        };
        let mut d = RaidDetector::default();
        for i in 0..5 {
            assert!(
                !d.on_join(&joiner(i, "a", true), false, &cfg, NOW + i as i64)
                    .triggered
            );
        }
        d.on_join(&joiner(10, "a", true), true, &cfg, NOW + 10);
        d.on_join(&joiner(11, "a", true), true, &cfg, NOW + 11);
        assert!(
            d.on_join(&joiner(12, "a", true), true, &cfg, NOW + 12)
                .triggered
        );
    }

    #[test]
    fn id_flag_catches_bot_farm_and_narrows_buckets() {
        let cfg = RaidCfg {
            id_flag_min: Some(3),
            ..raid_cfg()
        };
        let mut d = RaidDetector::default();
        // three accounts made within an hour on one old day, one unrelated account
        let farm = [
            id_aged(50 * DAY_MS, 1),
            id_aged(50 * DAY_MS - 60_000, 2),
            id_aged(50 * DAY_MS - 120_000, 3),
        ];
        let other = id_aged(200 * DAY_MS, 4);
        for (i, id) in farm.iter().chain([&other]).enumerate() {
            let out = d.on_join(&joiner(*id, "x", true), false, &cfg, NOW + i as i64);
            if out.triggered {
                let mut f = out.flagged.clone();
                f.sort();
                let mut want = farm.to_vec();
                want.sort();
                assert_eq!(f, want);
                return;
            }
        }
        panic!("raid never triggered");
    }

    #[test]
    fn raid_action_defaults_to_kick() {
        let mut v = serde_json::to_value(RaidCfg::default()).unwrap();
        v.as_object_mut().unwrap().remove("action");
        assert_eq!(
            serde_json::from_value::<RaidCfg>(v.clone()).unwrap().action,
            Action::Kick
        );
        v["action"] = serde_json::json!("Ban");
        assert_eq!(
            serde_json::from_value::<RaidCfg>(v).unwrap().action,
            Action::Ban
        );
    }

    #[test]
    fn verification_helpers() {
        let mut cfg = VerifyCfg {
            mode: VerifyMode::Button,
            ..Default::default()
        };
        assert_eq!(effective_mode(&cfg, true), VerifyMode::Captcha);
        assert_eq!(effective_mode(&cfg, false), VerifyMode::Button);
        cfg.mode = VerifyMode::Web;
        assert_eq!(effective_mode(&cfg, true), VerifyMode::Web);
        assert!(!must_verify(&cfg, true, true));
        cfg.target = VerifyTarget::SuspiciousOnly;
        assert!(!must_verify(&cfg, false, false));
        assert!(must_verify(&cfg, true, false));
    }
}
