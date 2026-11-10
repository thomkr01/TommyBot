//! Nuke cases, panic mode and webhook cleanup (spec 4.2). Pure logic; the bot executes the plans.

use crate::Inverse;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Six base36 chars (like `7rh9t7`) from caller-supplied entropy (e.g. now-nanos ^ guild id).
pub fn case_id(seed: u64) -> String {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (0..6)
        .map(|_| {
            let d = (z % 36) as u32;
            z /= 36;
            char::from_digit(d, 36).unwrap()
        })
        .collect()
}

/// Everything one nuke did, stored as jsonb in `nuke_incidents`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NukeCase {
    pub id: String,
    pub quarantined: Vec<u64>,
    pub reverted: Vec<Inverse>,
    /// Undone actions per culprit, for the panic report.
    #[serde(default)]
    pub fragments: HashMap<u64, u32>,
    /// Culprits the bot could not quarantine (or kick), counted as ❎ in the panic report.
    #[serde(default)]
    pub failed: Vec<u64>,
}

impl NukeCase {
    pub fn new(seed: u64) -> Self {
        Self {
            id: case_id(seed),
            quarantined: vec![],
            reverted: vec![],
            fragments: HashMap::new(),
            failed: vec![],
        }
    }

    /// `contained` is false when the culprit could not be quarantined; a later success moves them over.
    pub fn add(&mut self, executor: u64, ops: impl IntoIterator<Item = Inverse>, contained: bool) {
        if contained {
            self.failed.retain(|u| *u != executor);
            if !self.quarantined.contains(&executor) {
                self.quarantined.push(executor);
            }
        } else if !self.quarantined.contains(&executor) && !self.failed.contains(&executor) {
            self.failed.push(executor);
        }
        let before = self.reverted.len();
        self.reverted.extend(ops);
        *self.fragments.entry(executor).or_default() += (self.reverted.len() - before) as u32;
    }
}

/// Panic mode trips when `executors` distinct accounts were quarantined within `window`:
/// a coordinated nuke that per-user limits alone won't stop.
pub struct PanicTracker {
    pub window: Duration,
    pub executors: usize,
    hits: HashMap<u64, VecDeque<(Instant, u64)>>,
}

impl PanicTracker {
    pub fn new(window: Duration, executors: usize) -> Self {
        Self {
            window,
            executors,
            hits: HashMap::new(),
        }
    }

    /// Distinct accounts counted for this guild inside the window, before the next `record`.
    pub fn involved(&self, guild: u64, now: Instant) -> HashSet<u64> {
        let live = |t: &Instant| now.duration_since(*t) < self.window;
        self.hits
            .get(&guild)
            .into_iter()
            .flatten()
            .filter(|(t, _)| live(t))
            .map(|(_, u)| *u)
            .collect()
    }

    /// Call on every quarantine verdict. True means start panic mode (resets the guild).
    pub fn record(&mut self, guild: u64, executor: u64, now: Instant) -> bool {
        let q = self.hits.entry(guild).or_default();
        q.retain(|(t, _)| now.duration_since(*t) < self.window);
        q.push_back((now, executor));
        let tripped = q.iter().map(|(_, u)| u).collect::<HashSet<_>>().len() >= self.executors;
        if tripped {
            self.hits.remove(&guild);
        }
        tripped
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicStep {
    LockServer,
    LockModCommands,
    PingWarnedRoles,
    QuarantineInvolved,
    LoadLatestBackup,
    UnlockServer,
}

pub fn panic_plan(lock_server: bool, lock_mod_commands: bool) -> Vec<PanicStep> {
    use PanicStep::*;
    let mut v = vec![];
    if lock_server {
        v.push(LockServer);
    }
    if lock_mod_commands {
        v.push(LockModCommands);
    }
    v.extend([PingWarnedRoles, QuarantineInvolved, LoadLatestBackup]);
    v
}

/// Run when panic ends; unlocking is opt-in ("unlock when ending").
pub fn panic_end_plan(unlock_when_ending: bool) -> Vec<PanicStep> {
    if unlock_when_ending {
        vec![PanicStep::UnlockServer]
    } else {
        vec![]
    }
}

/// What a stopped nuke runs, in order. Panic mode is not an input: the revert always runs, and
/// the anti-nuke report is sent before panic mode can start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NukeStep {
    Contain,
    Revert,
    Report,
    Panic,
}

pub fn nuke_plan(auto_quarantine: bool) -> Vec<NukeStep> {
    use NukeStep::*;
    if auto_quarantine {
        vec![Contain, Revert, Report, Panic]
    } else {
        vec![Revert, Report]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Webhook {
    pub id: u64,
    pub owner: u64,
    pub created_at: i64, // unix seconds
}

/// Recently created webhooks to delete when heat triggers, minus protected owners
/// (EO/TA, immune users, popular bots) and whitelisted webhooks.
pub fn webhooks_to_delete(
    hooks: &[Webhook],
    since: i64,
    owner_protected: impl Fn(u64) -> bool,
    hook_whitelisted: impl Fn(u64) -> bool,
) -> Vec<u64> {
    hooks
        .iter()
        .filter(|h| h.created_at >= since && !owner_protected(h.owner) && !hook_whitelisted(h.id))
        .map(|h| h.id)
        .collect()
}

/// `cached` is `Some((owner_whitelisted, user_created))` when the webhook is cached; otherwise
/// only the webhook whitelist counts. User-created webhooks never inherit the owner's whitelist
/// because their tokens are exposed.
pub fn webhook_whitelisted(cached: Option<(bool, bool)>, hook_whitelisted: bool) -> bool {
    hook_whitelisted || cached.is_some_and(|(owner, user_created)| owner && !user_created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_id_is_six_base36_and_varies() {
        let a = case_id(1);
        assert_eq!(a.len(), 6);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
        assert_ne!(a, case_id(2));
        assert_eq!(a, case_id(1));
    }

    #[test]
    fn case_dedups_executors_and_collects_ops() {
        let mut c = NukeCase::new(7);
        c.add(1, [Inverse::DeleteChannel { id: 5 }], true);
        c.add(1, [Inverse::Unban { user: 9 }], true);
        c.add(2, [], true);
        assert_eq!(c.quarantined, vec![1, 2]);
        assert_eq!(c.reverted.len(), 2);
    }

    #[test]
    fn failed_quarantines_are_kept_apart_until_one_succeeds() {
        let mut c = NukeCase::new(7);
        c.add(3, [Inverse::DeleteChannel { id: 5 }], false);
        c.add(3, [], false);
        assert_eq!((c.quarantined.clone(), c.failed.clone()), (vec![], vec![3]));
        assert_eq!(c.fragments[&3], 1);
        c.add(3, [], true);
        assert_eq!((c.quarantined.clone(), c.failed.clone()), (vec![3], vec![]));
        c.add(3, [], false); // already quarantined: stays a success
        assert!(c.failed.is_empty());
    }

    #[test]
    fn nuke_reverts_without_panic_and_reports_before_panic() {
        use NukeStep::*;
        assert_eq!(nuke_plan(false), vec![Revert, Report]);
        assert_eq!(nuke_plan(true), vec![Contain, Revert, Report, Panic]);
    }

    #[test]
    fn panic_needs_distinct_executors_inside_window() {
        let (mut p, t0) = (
            PanicTracker::new(Duration::from_secs(60), 3),
            Instant::now(),
        );
        assert!(!p.record(1, 10, t0));
        assert!(!p.record(1, 10, t0)); // same account twice is not coordinated
        assert!(!p.record(1, 11, t0));
        assert!(!p.record(2, 12, t0)); // other guild is separate
        assert!(p.record(1, 12, t0 + Duration::from_secs(30)));
        assert!(!p.record(1, 13, t0 + Duration::from_secs(31))); // reset after tripping
    }

    #[test]
    fn panic_window_expires() {
        let (mut p, t0) = (
            PanicTracker::new(Duration::from_secs(60), 2),
            Instant::now(),
        );
        assert!(!p.record(1, 10, t0));
        assert!(!p.record(1, 11, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn panic_plans() {
        use PanicStep::*;
        assert_eq!(
            panic_plan(true, false),
            vec![
                LockServer,
                PingWarnedRoles,
                QuarantineInvolved,
                LoadLatestBackup
            ]
        );
        assert_eq!(panic_plan(true, true)[1], LockModCommands);
        assert_eq!(panic_plan(false, true)[0], LockModCommands);
        assert_eq!(panic_end_plan(true), vec![UnlockServer]);
        assert!(panic_end_plan(false).is_empty());
    }

    #[test]
    fn webhook_cleanup_and_whitelist() {
        let hooks = [
            Webhook {
                id: 1,
                owner: 100,
                created_at: 500,
            },
            Webhook {
                id: 2,
                owner: 200,
                created_at: 500,
            }, // protected owner
            Webhook {
                id: 3,
                owner: 100,
                created_at: 10,
            }, // too old
            Webhook {
                id: 4,
                owner: 100,
                created_at: 600,
            }, // whitelisted hook
        ];
        assert_eq!(
            webhooks_to_delete(&hooks, 100, |o| o == 200, |h| h == 4),
            vec![1]
        );

        assert!(webhook_whitelisted(Some((true, false)), false));
        assert!(!webhook_whitelisted(Some((true, true)), false)); // user-created: no inheritance
        assert!(!webhook_whitelisted(None, false)); // uncached: owner ignored
        assert!(webhook_whitelisted(None, true));
    }

    #[test]
    fn case_counts_fragments_per_culprit_and_panic_lists_involved() {
        let mut c = NukeCase::new(1);
        c.add(
            7,
            [
                Inverse::DeleteWebhook { id: 1 },
                Inverse::DeleteWebhook { id: 2 },
            ],
            true,
        );
        c.add(7, [Inverse::DeleteWebhook { id: 3 }], true);
        assert_eq!(c.fragments[&7], 3);
        let mut t = PanicTracker::new(Duration::from_secs(60), 3);
        let now = Instant::now();
        t.record(1, 10, now);
        t.record(1, 11, now);
        assert_eq!(t.involved(1, now), HashSet::from([10, 11]));
    }
}
