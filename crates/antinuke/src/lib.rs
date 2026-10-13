//! Anti-nuke, quarantine hold and `/revert` planning (spec sections 2.5 and 4.1-4.2).
//!
//! Pure logic only: no Discord or DB calls, so every rule is unit-testable. The bot crate feeds
//! audit-log events in and executes the returned verdicts / inverse ops. Ids are raw `u64`
//! snowflakes and times are passed in, to avoid depending on core or a clock.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Audit-log actions that are counted per executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    ChannelCreate,
    ChannelDelete,
    RoleCreate,
    RoleDelete,
    Ban,
    Kick,
    WebhookCreate,
    WebhookDelete,
    Prune,
    BotAdd,
}

/// Reaching either limit trips (spec: "Reaching either quarantines them").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    pub per_minute: u32,
    pub per_hour: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub minute: u32,
    pub hour: u32,
}

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

type Key = (u64, u64, Action); // (guild, executor, action)

/// Sliding-window event counter. Pruned on push, so memory is bounded by the hourly limit.
#[derive(Default)]
pub struct Windows(HashMap<Key, VecDeque<Instant>>);

impl Windows {
    // shortcut: plain HashMap needs &mut; wrap in a sharded DashMap when wiring into the bot.
    pub fn record(&mut self, guild: u64, user: u64, action: Action, now: Instant) -> Counts {
        let q = self.0.entry((guild, user, action)).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= HOUR) {
            q.pop_front();
        }
        q.push_back(now);
        let minute = q.iter().filter(|t| now.duration_since(**t) < MINUTE).count();
        Counts { minute: minute as u32, hour: q.len() as u32 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Exempt or not watched: nothing counted.
    Ignore,
    Allow(Counts),
    /// Limit reached: quarantine, revert, open a nuke case, alert owners.
    Quarantine(Counts),
}

pub struct AntiNuke {
    pub limits: HashMap<Action, Limit>,
    windows: Windows,
}

impl AntiNuke {
    pub fn new(limits: HashMap<Action, Limit>) -> Self {
        Self { limits, windows: Windows::default() }
    }

    /// `exempt` is computed by the caller: Owner, EO, TA, anti-nuke-whitelisted permit, or a
    /// whitelisted category for channel create/delete.
    pub fn on_action(&mut self, guild: u64, executor: u64, action: Action, exempt: bool, now: Instant) -> Verdict {
        let Some(limit) = self.limits.get(&action).copied() else { return Verdict::Ignore };
        if exempt {
            return Verdict::Ignore;
        }
        let c = self.windows.record(guild, executor, action, now);
        if c.minute >= limit.per_minute || c.hour >= limit.per_hour {
            Verdict::Quarantine(c)
        } else {
            Verdict::Allow(c)
        }
    }
}

// Discord permission bits that spec 2.1 calls dangerous.
pub const DANGEROUS_PERMS: u64 = (1 << 1)  // kick
    | (1 << 2)   // ban
    | (1 << 3)   // administrator
    | (1 << 4)   // manage channels
    | (1 << 5)   // manage guild
    | (1 << 13)  // manage messages
    | (1 << 28)  // manage roles
    | (1 << 29)  // manage webhooks
    | (1 << 40); // moderate members

/// Tripwire: a role or overwrite *gained* a dangerous permission (instant, no counting).
pub fn gained_dangerous(old: u64, new: u64) -> bool {
    new & !old & DANGEROUS_PERMS != 0
}

/// Tripwire: member given a role carrying dangerous permissions (strict member role addition).
pub fn role_grant_is_dangerous(role_perms: u64) -> bool {
    role_perms & DANGEROUS_PERMS != 0
}

/// Quarantine hold (4.1): someone else changed a quarantined member's roles.
/// Returns true when the executor must be quarantined and the change reverted.
pub fn hold_violation(executor: u64, target: u64, target_quarantined: bool, executor_qwhitelisted: bool) -> bool {
    target_quarantined && executor != target && !executor_qwhitelisted
}

/// Inverse operation stored as jsonb in `action_log.inverse`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Inverse {
    Unban { user: u64 },
    DeleteChannel { id: u64 },
    DeleteRole { id: u64 },
    DeleteWebhook { id: u64 },
    RestoreRoles { user: u64, roles: Vec<u64> },
    Unquarantine { user: u64 },
    /// Discord can't undo kicks: reported for re-invite.
    Reinvite { user: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub actor: u64,
    pub at: i64, // unix seconds
    pub inverse: Inverse,
    pub reverted: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RevertPlan {
    /// Newest first.
    pub ops: Vec<Inverse>,
    pub reinvite: Vec<u64>,
}

/// `/revert actor since`: inverses of everything `actor` did at or after `since`, newest first.
pub fn plan_revert(log: &[LogEntry], actor: u64, since: i64) -> RevertPlan {
    let mut sel: Vec<&LogEntry> = log.iter().filter(|e| e.actor == actor && e.at >= since && !e.reverted).collect();
    sel.sort_by_key(|e| std::cmp::Reverse(e.at));
    let mut plan = RevertPlan::default();
    for e in sel {
        match &e.inverse {
            Inverse::Reinvite { user } => plan.reinvite.push(*user),
            op => plan.ops.push(op.clone()),
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(pm: u32, ph: u32) -> AntiNuke {
        AntiNuke::new(HashMap::from([(Action::Ban, Limit { per_minute: pm, per_hour: ph })]))
    }

    #[test]
    fn minute_limit_trips_at_reach() {
        let (mut a, t0) = (engine(3, 100), Instant::now());
        assert!(matches!(a.on_action(1, 2, Action::Ban, false, t0), Verdict::Allow(_)));
        assert!(matches!(a.on_action(1, 2, Action::Ban, false, t0), Verdict::Allow(_)));
        assert_eq!(a.on_action(1, 2, Action::Ban, false, t0), Verdict::Quarantine(Counts { minute: 3, hour: 3 }));
    }

    #[test]
    fn minute_window_slides_but_hour_keeps_counting() {
        let (mut a, t0) = (engine(3, 4), Instant::now());
        for i in 0..3 {
            // spaced 40s apart: never 3 inside one minute
            let v = a.on_action(1, 2, Action::Ban, false, t0 + Duration::from_secs(40 * i));
            assert!(matches!(v, Verdict::Allow(_)), "{v:?}");
        }
        let v = a.on_action(1, 2, Action::Ban, false, t0 + Duration::from_secs(130));
        assert_eq!(v, Verdict::Quarantine(Counts { minute: 2, hour: 4 }));
    }

    #[test]
    fn old_events_expire_after_an_hour() {
        let (mut a, t0) = (engine(10, 2), Instant::now());
        a.on_action(1, 2, Action::Ban, false, t0);
        let v = a.on_action(1, 2, Action::Ban, false, t0 + HOUR);
        assert_eq!(v, Verdict::Allow(Counts { minute: 1, hour: 1 }));
    }

    #[test]
    fn exempt_unwatched_and_other_users_are_independent() {
        let (mut a, t0) = (engine(1, 1), Instant::now());
        assert_eq!(a.on_action(1, 2, Action::Ban, true, t0), Verdict::Ignore);
        assert_eq!(a.on_action(1, 2, Action::Kick, false, t0), Verdict::Ignore);
        assert!(matches!(a.on_action(1, 3, Action::Ban, false, t0), Verdict::Quarantine(_)));
        assert!(matches!(a.on_action(1, 4, Action::Ban, false, t0), Verdict::Quarantine(_)));
    }

    #[test]
    fn tripwires() {
        assert!(gained_dangerous(0, 1 << 3));
        assert!(!gained_dangerous(1 << 3, 1 << 3)); // already had it
        assert!(!gained_dangerous(0, 1 << 10)); // view channel: harmless
        assert!(role_grant_is_dangerous(1 << 28));
        assert!(!role_grant_is_dangerous(1 << 10));
    }

    #[test]
    fn quarantine_hold() {
        assert!(hold_violation(2, 3, true, false));
        assert!(!hold_violation(2, 3, true, true)); // whitelisted auto-role bot
        assert!(!hold_violation(2, 3, false, false));
        assert!(!hold_violation(3, 3, true, false)); // self change
    }

    #[test]
    fn revert_is_newest_first_scoped_and_skips_done() {
        let e = |actor, at, inverse, reverted| LogEntry { actor, at, inverse, reverted };
        let log = [
            e(9, 100, Inverse::Unban { user: 1 }, false),
            e(9, 300, Inverse::DeleteChannel { id: 2 }, false),
            e(9, 200, Inverse::Reinvite { user: 3 }, false),
            e(9, 250, Inverse::DeleteRole { id: 4 }, true), // already reverted
            e(8, 300, Inverse::DeleteRole { id: 5 }, false), // other actor
            e(9, 10, Inverse::Unban { user: 6 }, false), // before since
        ];
        let p = plan_revert(&log, 9, 50);
        assert_eq!(p.ops, vec![Inverse::DeleteChannel { id: 2 }, Inverse::Unban { user: 1 }]);
        assert_eq!(p.reinvite, vec![3]);
    }

    #[test]
    fn inverse_json_roundtrip() {
        let i = Inverse::RestoreRoles { user: 1, roles: vec![2, 3] };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(s, r#"{"op":"restore_roles","user":1,"roles":[2,3]}"#);
        assert_eq!(serde_json::from_str::<Inverse>(&s).unwrap(), i);
    }
}
