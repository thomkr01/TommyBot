//! Warn points (spec 4.9): thresholds map accumulated points to a punishment; harshest crossed wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Threshold {
    pub points: i32,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Timeout { secs: i64 },
    Kick,
    Ban,
}

impl Action {
    fn severity(&self) -> u8 {
        match self {
            Action::Timeout { .. } => 0,
            Action::Kick => 1,
            Action::Ban => 2,
        }
    }
}

/// Returns the punishment when `before` -> `after` crosses one or more thresholds.
pub fn crossed(thresholds: &[Threshold], before: i32, after: i32) -> Option<Threshold> {
    thresholds
        .iter()
        .filter(|t| t.points > before && t.points <= after)
        .max_by_key(|t| (t.action.severity(), t.points))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harshest_crossed_wins() {
        let ts = [
            Threshold {
                points: 3,
                action: Action::Timeout { secs: 600 },
            },
            Threshold {
                points: 6,
                action: Action::Kick,
            },
            Threshold {
                points: 9,
                action: Action::Ban,
            },
        ];
        assert_eq!(crossed(&ts, 0, 2), None);
        assert_eq!(
            crossed(&ts, 0, 3).unwrap().action,
            Action::Timeout { secs: 600 }
        );
        assert_eq!(crossed(&ts, 2, 7).unwrap().action, Action::Kick);
        assert_eq!(crossed(&ts, 7, 7), None);
    }
}
