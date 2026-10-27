//! Strikes after a heat trigger, with Wick's reoffend multiplier (1m, 1m, 11m cap, 22m, 44m).
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrikeCfg {
    /// "Use the auto timeouts": off records strikes but times nobody out.
    pub enabled: bool,
    /// Strike number that counts as the cap.
    pub cap: u32,
    pub strike_timeout_ms: i64,
    pub cap_timeout_ms: i64,
    /// Reoffending within this long after a cap timeout ends doubles the last duration.
    pub reoffend_window_ms: i64,
    /// Strikes below the cap are forgotten after this long without a trigger.
    pub strike_expiry_ms: i64,
    pub reset_heat_on_timeout: bool,
    /// Grow each cap timeout after the first by `multiplier_pct`.
    pub multiply: bool,
    pub multiplier_pct: u32,
}

impl Default for StrikeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            cap: 3,
            strike_timeout_ms: 60_000,
            cap_timeout_ms: 11 * 60_000,
            reoffend_window_ms: 60 * 60_000,
            strike_expiry_ms: 60 * 60_000,
            reset_heat_on_timeout: false,
            multiply: true,
            multiplier_pct: 200,
        }
    }
}

/// Persisted per (guild, user) in `store` so restarts lose nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StrikeState {
    pub strikes: u32,
    pub last_strike_ms: i64,
    /// Duration of the last cap timeout; 0 if none yet.
    pub last_cap_duration_ms: i64,
    pub cap_end_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrikeAction {
    pub strike: u32,
    pub timeout_ms: i64,
    /// At the cap: also purge the user's recent messages.
    pub purge: bool,
    pub reset_heat: bool,
}

pub fn strike(s: &mut StrikeState, cfg: &StrikeCfg, now: i64) -> StrikeAction {
    let expired = if s.last_cap_duration_ms > 0 {
        now > s.cap_end_ms + cfg.reoffend_window_ms
    } else {
        now - s.last_strike_ms > cfg.strike_expiry_ms
    };
    if expired {
        *s = StrikeState::default();
    }
    s.strikes += 1;
    s.last_strike_ms = now;
    let capped = s.strikes >= cfg.cap;
    let timeout_ms = if capped {
        s.last_cap_duration_ms = if s.last_cap_duration_ms > 0 && cfg.multiply {
            s.last_cap_duration_ms * i64::from(cfg.multiplier_pct) / 100
        } else {
            cfg.cap_timeout_ms
        };
        s.cap_end_ms = now + s.last_cap_duration_ms;
        s.last_cap_duration_ms
    } else {
        cfg.strike_timeout_ms
    };
    StrikeAction {
        strike: s.strikes,
        timeout_ms,
        purge: capped,
        reset_heat: cfg.reset_heat_on_timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: i64 = 60_000;

    #[test]
    fn wick_example_sequence() {
        let cfg = StrikeCfg::default();
        let mut s = StrikeState::default();
        let mut now = 0;
        let mut seq = vec![];
        for _ in 0..5 {
            let a = strike(&mut s, &cfg, now);
            seq.push((a.timeout_ms / M, a.purge));
            now += a.timeout_ms + 5 * M; // reoffend shortly after the timeout ends
        }
        assert_eq!(
            seq,
            [(1, false), (1, false), (11, true), (22, true), (44, true)]
        );
        let flat = StrikeCfg {
            multiply: false,
            ..Default::default()
        };
        let mut s = StrikeState::default();
        let caps: Vec<_> = (0..4)
            .map(|i| strike(&mut s, &flat, i).timeout_ms / M)
            .collect();
        assert_eq!(caps, [1, 1, 11, 11]);
    }

    #[test]
    fn old_strikes_and_old_caps_are_forgotten() {
        let cfg = StrikeCfg::default();
        let mut s = StrikeState::default();
        strike(&mut s, &cfg, 0);
        assert_eq!(strike(&mut s, &cfg, 2 * 60 * M).strike, 1);
        let mut s = StrikeState::default();
        for i in 0..3 {
            strike(&mut s, &cfg, i);
        }
        let later = s.cap_end_ms + cfg.reoffend_window_ms + 1;
        let a = strike(&mut s, &cfg, later);
        assert_eq!((a.strike, a.timeout_ms), (1, cfg.strike_timeout_ms));
    }
}
