//! Voice join raid: many different members joining voice in a short time. Once it triggers,
//! everyone in the window and everyone who joins voice until it ends is flagged, except staff,
//! bots and whitelisted members.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::join::Action;

/// Allowed ranges (dashboard, import, MCP, prefix panel); the detector clamps to them on read.
pub const VOICE_COUNT: (i64, i64) = (2, 100);
pub const VOICE_SPAN_MS: (i64, i64) = (1_000, 300_000);
pub const VOICE_ACTIVE_MS: (i64, i64) = (60_000, 3_600_000);
/// What a plain "Timeout" means here. The server's default timeout can be 28 days, too long for
/// members whose only flag is joining voice during a raid.
pub const VOICE_TIMEOUT_MS: i64 = 600_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceRaidCfg {
    pub enabled: bool,
    /// Different members joining voice within `span_ms` that start a raid.
    pub count: usize,
    pub span_ms: i64,
    /// How long joins stay flagged once triggered.
    pub active_ms: i64,
    /// What happens to flagged members after they are disconnected.
    #[serde(deserialize_with = "crate::join::no_rename")]
    pub action: Action,
}

impl Default for VoiceRaidCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            count: 8,
            span_ms: 10_000,
            active_ms: 5 * 60_000,
            action: Action::Timeout(0),
        }
    }
}

impl VoiceRaidCfg {
    /// The action to apply: the default timeout is [`VOICE_TIMEOUT_MS`].
    pub fn action(&self) -> Action {
        match self.action {
            Action::Timeout(0) => Action::Timeout(VOICE_TIMEOUT_MS),
            a => a,
        }
    }

    fn count(&self) -> usize {
        (self.count as i64).clamp(VOICE_COUNT.0, VOICE_COUNT.1) as usize
    }

    fn span(&self) -> i64 {
        self.span_ms.clamp(VOICE_SPAN_MS.0, VOICE_SPAN_MS.1)
    }

    pub fn active(&self) -> i64 {
        self.active_ms.clamp(VOICE_ACTIVE_MS.0, VOICE_ACTIVE_MS.1)
    }
}

/// The window never needs more members than the highest trigger count.
const MAX_WINDOW: usize = VOICE_COUNT.1 as usize;

#[derive(Debug, Default)]
pub struct VoiceRaidDetector {
    /// Each member's last voice join in the window (id, ms, name), oldest first, one per member.
    joins: VecDeque<(u64, i64, String)>,
    active_until: i64,
    /// When the current (or last) raid started.
    pub started: i64,
    /// Report code of the current raid (`voice-` + a join raid code).
    pub code: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct VoiceOutcome {
    /// This join started the raid (claimed once per active period under the caller's lock).
    pub triggered: bool,
    /// Members to disconnect and punish, with their names.
    pub flagged: Vec<(u64, String)>,
}

impl VoiceRaidDetector {
    /// A raid that was still running at restart.
    pub fn restored(started_ms: i64, until_ms: i64, code: String) -> Self {
        Self {
            started: started_ms,
            active_until: until_ms,
            code,
            ..Self::default()
        }
    }

    pub fn active(&self, now: i64) -> bool {
        now < self.active_until
    }

    pub fn active_until(&self) -> i64 {
        self.active_until
    }

    /// Nothing left that a later join could need: not active and the window has run out.
    pub fn idle(&self, now: i64) -> bool {
        !self.active(now)
            && self
                .joins
                .back()
                .is_none_or(|j| now - j.1 > VOICE_SPAN_MS.1)
    }

    /// A member joined voice (no channel before). An exempt member (staff, bot, whitelisted)
    /// neither counts nor is flagged. Reaching the count again while active only extends it.
    pub fn on_join(
        &mut self,
        user: u64,
        name: &str,
        exempt: bool,
        cfg: &VoiceRaidCfg,
        now: i64,
    ) -> VoiceOutcome {
        if exempt {
            return VoiceOutcome::default();
        }
        let span = cfg.span();
        while self.joins.front().is_some_and(|j| now - j.1 > span) {
            self.joins.pop_front();
        }
        self.joins.retain(|j| j.0 != user);
        self.joins.push_back((user, now, name.to_string()));
        if self.joins.len() > MAX_WINDOW {
            self.joins.pop_front();
        }
        let was_active = self.active(now);
        if self.joins.len() >= cfg.count() {
            self.active_until = now + cfg.active();
            if !was_active {
                self.started = now;
                return VoiceOutcome {
                    triggered: true,
                    flagged: self.joins.iter().map(|j| (j.0, j.2.clone())).collect(),
                };
            }
        }
        VoiceOutcome {
            triggered: false,
            flagged: if was_active {
                vec![(user, name.to_string())]
            } else {
                vec![]
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VoiceRaidCfg {
        VoiceRaidCfg {
            enabled: true,
            count: 3,
            span_ms: 10_000,
            active_ms: 60_000,
            action: Action::Timeout(0),
        }
    }

    fn ids(o: &VoiceOutcome) -> Vec<u64> {
        o.flagged.iter().map(|f| f.0).collect()
    }

    #[test]
    fn triggers_at_count_of_different_members_and_flags_the_window() {
        let mut d = VoiceRaidDetector::default();
        assert_eq!(d.on_join(1, "a", false, &cfg(), 0), VoiceOutcome::default());
        assert_eq!(d.on_join(2, "b", false, &cfg(), 1), VoiceOutcome::default());
        let o = d.on_join(3, "c", false, &cfg(), 2);
        assert!(o.triggered);
        assert_eq!(ids(&o), [1, 2, 3]);
        assert_eq!(o.flagged[2].1, "c");
        assert_eq!(d.started, 2);
        // Every later join is flagged, without a second trigger.
        let o = d.on_join(4, "d", false, &cfg(), 3);
        assert!(!o.triggered);
        assert_eq!(ids(&o), [4]);
    }

    #[test]
    fn one_member_hopping_in_and_out_never_triggers() {
        let mut d = VoiceRaidDetector::default();
        for t in 0..8 {
            assert_eq!(d.on_join(1, "a", false, &cfg(), t), VoiceOutcome::default());
        }
        assert!(!d.active(8));
    }

    #[test]
    fn the_window_slides() {
        let mut d = VoiceRaidDetector::default();
        d.on_join(1, "a", false, &cfg(), 0);
        d.on_join(2, "b", false, &cfg(), 5_000);
        // Member 1 is out of the 10 s window.
        assert_eq!(
            d.on_join(3, "c", false, &cfg(), 10_001),
            VoiceOutcome::default()
        );
        assert!(d.on_join(4, "d", false, &cfg(), 12_000).triggered);
    }

    #[test]
    fn the_active_period_ends() {
        let mut d = VoiceRaidDetector::default();
        for (u, t) in [(1, 0), (2, 1), (3, 2)] {
            d.on_join(u, "x", false, &cfg(), t);
        }
        assert!(d.active(60_001));
        assert!(!d.active(60_002));
        assert_eq!(
            d.on_join(9, "z", false, &cfg(), 70_000),
            VoiceOutcome::default()
        );
        assert!(!d.idle(70_000));
        assert!(d.idle(70_000 + VOICE_SPAN_MS.1 + 1));
    }

    #[test]
    fn a_second_trigger_inside_the_active_period_only_extends_it() {
        let mut d = VoiceRaidDetector::default();
        for (u, t) in [(1, 0), (2, 1), (3, 2)] {
            d.on_join(u, "x", false, &cfg(), t);
        }
        assert_eq!(d.active_until(), 60_002);
        // Three new members inside the active period: the count is reached again.
        for (u, t) in [(4, 50_000), (5, 50_001)] {
            assert!(!d.on_join(u, "x", false, &cfg(), t).triggered);
        }
        let o = d.on_join(6, "x", false, &cfg(), 50_002);
        assert!(!o.triggered);
        assert_eq!(ids(&o), [6]);
        assert_eq!(d.active_until(), 110_002);
        assert_eq!(d.started, 2);
    }

    #[test]
    fn exempt_members_neither_count_nor_get_flagged() {
        let mut d = VoiceRaidDetector::default();
        for u in 10..20 {
            assert_eq!(
                d.on_join(u, "staff", true, &cfg(), 0),
                VoiceOutcome::default()
            );
        }
        assert!(!d.active(0));
        for (u, t) in [(1, 1), (2, 2), (3, 3)] {
            d.on_join(u, "x", false, &cfg(), t);
        }
        assert!(d.active(4));
        assert_eq!(
            d.on_join(10, "staff", true, &cfg(), 5),
            VoiceOutcome::default()
        );
    }

    #[test]
    fn out_of_range_settings_are_clamped() {
        let wild = VoiceRaidCfg {
            count: 0,
            span_ms: -5,
            active_ms: i64::MAX,
            ..cfg()
        };
        let mut d = VoiceRaidDetector::default();
        // Count 0 still needs two members.
        assert!(!d.on_join(1, "a", false, &wild, 0).triggered);
        assert!(d.on_join(2, "b", false, &wild, 500).triggered);
        assert_eq!(d.active_until(), 500 + VOICE_ACTIVE_MS.1);
        // The window stays bounded.
        let mut d = VoiceRaidDetector::default();
        let big = VoiceRaidCfg {
            count: 1000,
            ..cfg()
        };
        for u in 0..500 {
            d.on_join(u, "x", false, &big, 0);
        }
        assert!(d.joins.len() <= MAX_WINDOW);
    }

    #[test]
    fn defaults_and_old_configs() {
        let c = VoiceRaidCfg::default();
        assert!(!c.enabled);
        assert_eq!((c.count, c.span_ms, c.active_ms), (8, 10_000, 300_000));
        assert_eq!(c.action(), Action::Timeout(VOICE_TIMEOUT_MS));
        // Old saved Join Raid settings (no `voice` key) still parse, with the card off.
        let old: crate::join::RaidCfg = serde_json::from_str(
            r#"{"enabled":true,"count":3,"span_ms":1,"suspicious_only":false,"active_ms":1,"string_patterns":[],"flag_younger_than_ms":null,"flag_no_avatar":false,"action":"Kick","lock":null,"warned_roles":[]}"#,
        )
        .unwrap();
        assert!(old.enabled && !old.voice.enabled);
        assert_eq!(old.voice.count, 8);
        // What the dashboard sends.
        let v: VoiceRaidCfg =
            serde_json::from_str(r#"{"enabled":true,"count":5,"action":"Kick"}"#).unwrap();
        assert_eq!((v.count, v.span_ms, v.action()), (5, 10_000, Action::Kick));
        assert!(serde_json::from_str::<VoiceRaidCfg>(r#"{"action":"Rename"}"#).is_err());
        // The defaults write back as the dashboard select's value.
        assert_eq!(
            serde_json::to_value(VoiceRaidCfg::default()).unwrap()["action"],
            "Timeout"
        );
    }
}
