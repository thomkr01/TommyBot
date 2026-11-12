//! Anti-nuke, quarantine hold and `/revert` planning (spec sections 2.5 and 4.1-4.2).
//!
//! Pure logic only: no Discord or DB calls, so every rule is unit-testable. The bot crate feeds
//! audit-log events in and executes the returned verdicts / inverse ops. Ids are raw `u64`
//! snowflakes and times are passed in, to avoid depending on core or a clock.

mod backup;
mod incident;
pub mod security;
pub use backup::*;
pub use incident::*;

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
    Timeout,
    ChannelUpdate,
    /// Renames only.
    RoleUpdate,
    EmojiDelete,
    StickerDelete,
    AutoModRuleDelete,
    AutoModRuleUpdate,
    Unban,
    IntegrationCreate,
    IntegrationDelete,
    EmojiCreate,
    StickerCreate,
    SoundboardDelete,
    ScheduledEventCreate,
    /// Name or icon changes only.
    GuildUpdate,
    VoiceDisconnect,
    VoiceMove,
}

impl Action {
    pub const ALL: [Action; 27] = [
        Action::ChannelCreate,
        Action::ChannelDelete,
        Action::RoleCreate,
        Action::RoleDelete,
        Action::Ban,
        Action::Kick,
        Action::WebhookCreate,
        Action::WebhookDelete,
        Action::Prune,
        Action::BotAdd,
        Action::Timeout,
        Action::ChannelUpdate,
        Action::RoleUpdate,
        Action::EmojiDelete,
        Action::StickerDelete,
        Action::AutoModRuleDelete,
        Action::AutoModRuleUpdate,
        Action::Unban,
        Action::IntegrationCreate,
        Action::IntegrationDelete,
        Action::EmojiCreate,
        Action::StickerCreate,
        Action::SoundboardDelete,
        Action::ScheduledEventCreate,
        Action::GuildUpdate,
        Action::VoiceDisconnect,
        Action::VoiceMove,
    ];

    /// Defaults (per minute, per hour, heat %); None: watched only when configured.
    pub fn default_limit(self) -> Option<(u32, u32, u32)> {
        Some(match self {
            Action::Ban | Action::Kick => (5, 15, 20),
            Action::RoleCreate => (5, 15, 10),
            Action::RoleDelete => (3, 10, 25),
            Action::ChannelCreate => (4, 12, 16),
            Action::ChannelDelete => (3, 8, 25),
            Action::WebhookCreate => (3, 10, 15),
            Action::WebhookDelete => (3, 8, 10),
            Action::Prune => (1, 1, 0),
            Action::BotAdd => return None,
            Action::Timeout | Action::Unban => (5, 15, 15),
            Action::ChannelUpdate | Action::RoleUpdate => (5, 20, 10),
            Action::EmojiDelete | Action::StickerDelete => (5, 15, 10),
            Action::AutoModRuleDelete => (2, 5, 25),
            Action::AutoModRuleUpdate => (3, 10, 15),
            Action::IntegrationCreate | Action::IntegrationDelete | Action::GuildUpdate => {
                (2, 5, 25)
            }
            Action::EmojiCreate => (5, 15, 10),
            Action::StickerCreate => (3, 10, 10),
            Action::SoundboardDelete => (3, 10, 15),
            Action::ScheduledEventCreate => (3, 10, 10),
            Action::VoiceDisconnect | Action::VoiceMove => (10, 30, 5),
        })
    }

    /// Watched only once the server configures it, so busy mods are not quarantined by surprise.
    pub fn opt_in(self) -> bool {
        matches!(
            self,
            Action::Timeout
                | Action::ChannelUpdate
                | Action::RoleUpdate
                | Action::EmojiDelete
                | Action::StickerDelete
                | Action::AutoModRuleDelete
                | Action::AutoModRuleUpdate
                | Action::Unban
                | Action::IntegrationDelete
                | Action::EmojiCreate
                | Action::StickerCreate
                | Action::SoundboardDelete
                | Action::ScheduledEventCreate
                | Action::GuildUpdate
                | Action::VoiceDisconnect
                | Action::VoiceMove
        )
    }

    /// Discord keeps nothing to put back, so the anti-nuke report says so instead of a revert.
    pub fn cannot_undo(self) -> bool {
        matches!(
            self,
            Action::IntegrationDelete
                | Action::SoundboardDelete
                | Action::VoiceDisconnect
                | Action::VoiceMove
        )
    }

    /// "Kicks & Bans" card: kicks share the ban limit and counter.
    pub fn card(self) -> Action {
        if self == Action::Kick {
            Action::Ban
        } else {
            self
        }
    }
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
    windows: HashMap<Key, VecDeque<Instant>>,
}

impl AntiNuke {
    pub fn new(limits: HashMap<Action, Limit>) -> Self {
        Self {
            limits,
            windows: HashMap::new(),
        }
    }

    /// `exempt` is computed by the caller: Owner, EO, TA, anti-nuke-whitelisted permit, or a
    /// whitelisted category for channel create/delete.
    pub fn on_action(
        &mut self,
        guild: u64,
        executor: u64,
        action: Action,
        exempt: bool,
        now: Instant,
    ) -> Verdict {
        self.on_actions(guild, executor, action, exempt, now, 1)
    }

    /// `n` actions at once (a merged voice audit entry grew by `n`). `n` is capped at the hour
    /// limit, which already trips, so a huge count cannot make a huge loop or window.
    pub fn on_actions(
        &mut self,
        guild: u64,
        executor: u64,
        action: Action,
        exempt: bool,
        now: Instant,
        n: u32,
    ) -> Verdict {
        let action = action.card();
        let Some(limit) = self.limits.get(&action).copied() else {
            return Verdict::Ignore;
        };
        if exempt {
            return Verdict::Ignore;
        }
        let q = self.windows.entry((guild, executor, action)).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= HOUR) {
            q.pop_front();
        }
        q.extend(std::iter::repeat_n(now, n.min(limit.per_hour) as usize));
        let minute = q
            .iter()
            .filter(|t| now.duration_since(**t) < MINUTE)
            .count();
        let c = Counts {
            minute: minute as u32,
            hour: q.len() as u32,
        };
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
    has_dangerous(new & !old)
}

/// Also the tripwire for giving a member a role that carries dangerous permissions.
pub fn has_dangerous(perms: u64) -> bool {
    perms & DANGEROUS_PERMS != 0
}

/// Quarantine hold (4.1): someone else changed a quarantined member's roles.
/// Returns true when the executor must be quarantined and the change reverted.
pub fn hold_violation(
    executor: u64,
    target: u64,
    target_quarantined: bool,
    executor_qwhitelisted: bool,
) -> bool {
    target_quarantined && executor != target && !executor_qwhitelisted
}

/// Inverse operation stored as jsonb in `action_log.inverse`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Inverse {
    Unban {
        user: u64,
    },
    DeleteChannel {
        id: u64,
    },
    DeleteRole {
        id: u64,
    },
    DeleteWebhook {
        id: u64,
    },
    RestoreRoles {
        user: u64,
        roles: Vec<u64>,
    },
    Unquarantine {
        user: u64,
    },
    /// Undoes an accepted ban appeal.
    Ban {
        user: u64,
    },
    /// Undoes an accepted timeout appeal; `until` is unix seconds.
    Timeout {
        user: u64,
        until: i64,
    },
    /// Discord can't undo kicks: reported for re-invite.
    Reinvite {
        user: u64,
    },
    /// Gives a role back the permissions anti-nuke cleared (a nuking bot's managed role).
    RolePerms {
        id: u64,
        perms: u64,
    },
    /// Removes an added app (Discord also kicks its bot and deletes its webhooks).
    DeleteIntegration {
        id: u64,
    },
    DeleteEmoji {
        id: u64,
    },
    DeleteSticker {
        id: u64,
    },
    DeleteScheduledEvent {
        id: u64,
    },
    GuildName {
        name: String,
    },
    /// `None`: the server had no icon.
    GuildIcon {
        hash: Option<String>,
    },
}

/// The CDN address of a server icon, or None for anything that is not a Discord icon hash
/// (`a_` for animated, then 32 lowercase hex). The only place an icon URL is built, so a stored
/// value can never point a download at another host or path.
pub fn icon_url(guild: u64, hash: &str) -> Option<String> {
    let (hex, ext) = match hash.strip_prefix("a_") {
        Some(h) => (h, "gif"),
        None => (hash, "png"),
    };
    (hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| format!("https://cdn.discordapp.com/icons/{guild}/{hash}.{ext}?size=1024"))
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
    let mut sel: Vec<&LogEntry> = log
        .iter()
        .filter(|e| e.actor == actor && e.at >= since && !e.reverted)
        .collect();
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
    #[test]
    fn appeal_inverses_match_the_json_the_dashboard_stores() {
        let ban: Inverse = serde_json::from_str(r#"{"op":"ban","user":7}"#).unwrap();
        assert_eq!(ban, Inverse::Ban { user: 7 });
        let t: Inverse = serde_json::from_str(r#"{"op":"timeout","user":7,"until":99}"#).unwrap();
        assert_eq!(t, Inverse::Timeout { user: 7, until: 99 });
    }

    use super::*;

    fn engine(pm: u32, ph: u32) -> AntiNuke {
        AntiNuke::new(HashMap::from([(
            Action::Ban,
            Limit {
                per_minute: pm,
                per_hour: ph,
            },
        )]))
    }

    #[test]
    fn minute_limit_trips_at_reach() {
        let (mut a, t0) = (engine(3, 100), Instant::now());
        assert!(matches!(
            a.on_action(1, 2, Action::Ban, false, t0),
            Verdict::Allow(_)
        ));
        assert!(matches!(
            a.on_action(1, 2, Action::Ban, false, t0),
            Verdict::Allow(_)
        ));
        assert_eq!(
            a.on_action(1, 2, Action::Ban, false, t0),
            Verdict::Quarantine(Counts { minute: 3, hour: 3 })
        );
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
    fn kicks_and_bans_share_one_counter() {
        let (mut a, t0) = (engine(2, 10), Instant::now());
        assert!(matches!(
            a.on_action(1, 2, Action::Kick, false, t0),
            Verdict::Allow(_)
        ));
        assert!(matches!(
            a.on_action(1, 2, Action::Ban, false, t0),
            Verdict::Quarantine(Counts { minute: 2, .. })
        ));
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
        assert_eq!(
            a.on_action(1, 2, Action::RoleCreate, false, t0),
            Verdict::Ignore
        );
        assert!(matches!(
            a.on_action(1, 3, Action::Ban, false, t0),
            Verdict::Quarantine(_)
        ));
        assert!(matches!(
            a.on_action(1, 4, Action::Ban, false, t0),
            Verdict::Quarantine(_)
        ));
    }

    #[test]
    fn on_actions_counts_n_capped_at_the_hour_limit() {
        let (mut a, t0) = (engine(10, 30), Instant::now());
        assert_eq!(
            a.on_actions(1, 2, Action::Ban, false, t0, 4),
            Verdict::Allow(Counts { minute: 4, hour: 4 })
        );
        assert_eq!(
            a.on_actions(1, 2, Action::Ban, false, t0, 6),
            Verdict::Quarantine(Counts {
                minute: 10,
                hour: 10
            })
        );
        let (mut a, t0) = (engine(10, 30), Instant::now());
        assert_eq!(
            a.on_actions(1, 2, Action::Ban, false, t0, u32::MAX),
            Verdict::Quarantine(Counts {
                minute: 30,
                hour: 30
            })
        );
        assert_eq!(
            a.on_actions(1, 2, Action::Ban, true, t0, 50),
            Verdict::Ignore
        );
    }

    #[test]
    fn tripwires() {
        assert!(gained_dangerous(0, 1 << 3));
        assert!(!gained_dangerous(1 << 3, 1 << 3)); // already had it
        assert!(!gained_dangerous(0, 1 << 10)); // view channel: harmless
        assert!(has_dangerous(1 << 28));
        assert!(!has_dangerous(1 << 10));
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
        let e = |actor, at, inverse, reverted| LogEntry {
            actor,
            at,
            inverse,
            reverted,
        };
        let log = [
            e(9, 100, Inverse::Unban { user: 1 }, false),
            e(9, 300, Inverse::DeleteChannel { id: 2 }, false),
            e(9, 200, Inverse::Reinvite { user: 3 }, false),
            e(9, 250, Inverse::DeleteRole { id: 4 }, true), // already reverted
            e(8, 300, Inverse::DeleteRole { id: 5 }, false), // other actor
            e(9, 10, Inverse::Unban { user: 6 }, false),    // before since
        ];
        let p = plan_revert(&log, 9, 50);
        assert_eq!(
            p.ops,
            vec![Inverse::DeleteChannel { id: 2 }, Inverse::Unban { user: 1 }]
        );
        assert_eq!(p.reinvite, vec![3]);
    }

    #[test]
    fn inverse_json_roundtrip() {
        let i = Inverse::RestoreRoles {
            user: 1,
            roles: vec![2, 3],
        };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(s, r#"{"op":"restore_roles","user":1,"roles":[2,3]}"#);
        assert_eq!(serde_json::from_str::<Inverse>(&s).unwrap(), i);
        let i = Inverse::RolePerms { id: 4, perms: 8 };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(s, r#"{"op":"role_perms","id":4,"perms":8}"#);
        assert_eq!(serde_json::from_str::<Inverse>(&s).unwrap(), i);
        for (i, json) in [
            (
                Inverse::DeleteIntegration { id: 1 },
                r#"{"op":"delete_integration","id":1}"#,
            ),
            (
                Inverse::DeleteEmoji { id: 2 },
                r#"{"op":"delete_emoji","id":2}"#,
            ),
            (
                Inverse::DeleteSticker { id: 3 },
                r#"{"op":"delete_sticker","id":3}"#,
            ),
            (
                Inverse::DeleteScheduledEvent { id: 4 },
                r#"{"op":"delete_scheduled_event","id":4}"#,
            ),
            (
                Inverse::GuildName { name: "Old".into() },
                r#"{"op":"guild_name","name":"Old"}"#,
            ),
            (
                Inverse::GuildIcon { hash: None },
                r#"{"op":"guild_icon","hash":null}"#,
            ),
            (
                Inverse::GuildIcon {
                    hash: Some("ab".into()),
                },
                r#"{"op":"guild_icon","hash":"ab"}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&i).unwrap(), json);
            assert_eq!(serde_json::from_str::<Inverse>(json).unwrap(), i);
        }
    }

    #[test]
    fn two_renames_revert_to_the_first_name() {
        let e = |at, name: &str| LogEntry {
            actor: 9,
            at,
            inverse: Inverse::GuildName { name: name.into() },
            reverted: false,
        };
        // Same second: the caller passes rows newest first, and the order holds for ties.
        let p = plan_revert(&[e(100, "second"), e(100, "original")], 9, 0);
        let last = p.ops.last().unwrap();
        assert_eq!(
            last,
            &Inverse::GuildName {
                name: "original".into()
            }
        );
        let p = plan_revert(&[e(100, "original"), e(200, "second")], 9, 0);
        assert_eq!(
            p.ops.last().unwrap(),
            &Inverse::GuildName {
                name: "original".into()
            }
        );
    }

    #[test]
    fn icon_url_takes_only_discord_hashes() {
        let h = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            icon_url(5, h).unwrap(),
            format!("https://cdn.discordapp.com/icons/5/{h}.png?size=1024")
        );
        assert!(icon_url(5, &format!("a_{h}"))
            .unwrap()
            .ends_with(".gif?size=1024"));
        for bad in [
            "",
            "../../evil",
            &h.to_uppercase(),
            &h[1..],
            &format!("{h}0"),
            &format!("{}?x=1", &h[..28]),
            &format!("{}/../", &h[..27]),
            &format!("b_{h}"),
        ] {
            assert_eq!(icon_url(5, bad), None, "{bad}");
        }
    }

    #[test]
    fn new_cards_are_opt_in_except_app_additions() {
        assert!(!Action::IntegrationCreate.opt_in());
        for a in [
            Action::IntegrationDelete,
            Action::EmojiCreate,
            Action::StickerCreate,
            Action::SoundboardDelete,
            Action::ScheduledEventCreate,
            Action::GuildUpdate,
            Action::VoiceDisconnect,
            Action::VoiceMove,
        ] {
            assert!(a.opt_in(), "{a:?}");
        }
        // Every card's minute limit is inside the dashboard range (2-50), Prune aside.
        for a in Action::ALL {
            if let Some((m, h, _)) = a.default_limit().filter(|_| a != Action::Prune) {
                assert!((2..=50).contains(&m) && h >= m, "{a:?}");
            }
        }
    }
}
