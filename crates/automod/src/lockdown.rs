//! Role-level lockdown: few API calls, exact restore from a snapshot (spec 4.5).
//! `plan` turns the guild's role list into a set of ops; the bot executes them and stores the snapshot.
use crate::window::Window;
use serde::{Deserialize, Serialize};

pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
/// Perms stripped by role lock: admin, kick, ban, manage channels/guild/messages/roles/webhooks/
/// expressions/nicknames/events/threads, mention everyone, moderate members, view audit log.
pub const DANGEROUS: u64 = (1 << 3)
    | (1 << 1)
    | (1 << 2)
    | (1 << 4)
    | (1 << 5)
    | (1 << 7)
    | (1 << 13)
    | (1 << 17)
    | (1 << 27)
    | (1 << 28)
    | (1 << 29)
    | (1 << 30)
    | (1 << 33)
    | (1 << 34)
    | (1 << 40);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinLock {
    Kick,
    Ban,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockKind {
    /// Main roles lose Send Messages.
    Server,
    /// Main roles lose View Channel too; a public `#server-locked` channel is created.
    Blind,
    Channel(u64),
    /// Every role below the bot loses dangerous perms.
    Role,
    Join(JoinLock),
    /// Server + role + join (ban).
    All,
    Mixed {
        channels: bool,
        roles: bool,
        joins: bool,
    },
}

impl LockKind {
    pub fn covers(&self) -> (bool, bool, Option<JoinLock>) {
        match self {
            LockKind::Server | LockKind::Blind => (true, false, None),
            LockKind::Role => (false, true, None),
            LockKind::Join(j) => (false, false, Some(*j)),
            LockKind::All => (true, true, Some(JoinLock::Ban)),
            LockKind::Mixed {
                channels,
                roles,
                joins,
            } => (*channels, *roles, joins.then_some(JoinLock::Ban)),
            LockKind::Channel(_) => (false, false, None),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct LockdownPrefs {
    pub channels: bool,
    pub roles: bool,
    pub joins: bool,
    pub updates: bool,
}

impl Default for LockdownPrefs {
    fn default() -> Self {
        Self {
            channels: true,
            roles: false,
            joins: false,
            updates: false,
        }
    }
}

impl LockdownPrefs {
    pub fn kind(&self) -> LockKind {
        if self.channels || self.roles || self.joins {
            LockKind::Mixed {
                channels: self.channels,
                roles: self.roles,
                joins: self.joins,
            }
        } else {
            LockKind::Server
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RoleInfo {
    pub id: u64,
    pub perms: u64,
    pub position: i32,
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    SetRolePerms {
        role: u64,
        perms: u64,
    },
    /// Deny Send Messages for `role` on `channel` (channel lock).
    DenyChannelSend {
        channel: u64,
        role: u64,
    },
    CreateLockedChannel,
    EnableJoinLock(JoinLock),
    PauseInvites,
}

/// What `plan` produced: ops to run now and the exact state to restore on unlock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub kind: Option<LockKind>,
    /// (role, perms before the lock)
    pub role_perms: Vec<(u64, u64)>,
    pub channel_denies: Vec<(u64, u64)>,
    pub join_lock: bool,
    pub invites_paused: bool,
}

pub struct Plan {
    pub ops: Vec<Op>,
    pub snapshot: Snapshot,
}

/// `main_roles` empty means @everyone (whose role id equals the guild id) is the main role.
pub fn plan(
    kind: &LockKind,
    guild_id: u64,
    main_roles: &[u64],
    roles: &[RoleInfo],
    bot_position: i32,
) -> Plan {
    let mut p = Plan {
        ops: vec![],
        snapshot: Snapshot {
            kind: Some(kind.clone()),
            ..Default::default()
        },
    };
    let main: Vec<u64> = if main_roles.is_empty() {
        vec![guild_id]
    } else {
        main_roles.to_vec()
    };
    let strip = |p: &mut Plan, r: &RoleInfo, mask: u64| {
        let perms = r.perms & !mask;
        if perms != r.perms && !p.snapshot.role_perms.iter().any(|(id, _)| *id == r.id) {
            p.snapshot.role_perms.push((r.id, r.perms));
            p.ops.push(Op::SetRolePerms { role: r.id, perms });
        }
    };
    let (channels, role_lock, join) = kind.covers();
    if channels {
        let blind = matches!(kind, LockKind::Blind);
        let mask = if blind {
            SEND_MESSAGES | VIEW_CHANNEL
        } else {
            SEND_MESSAGES
        };
        roles
            .iter()
            .filter(|r| main.contains(&r.id))
            .for_each(|r| strip(&mut p, r, mask));
        if blind {
            p.ops.push(Op::CreateLockedChannel);
        }
    }
    if let LockKind::Channel(c) = kind {
        for &role in &main {
            p.snapshot.channel_denies.push((*c, role));
            p.ops.push(Op::DenyChannelSend { channel: *c, role });
        }
    }
    if role_lock {
        // Bot-managed roles and anything at/above the bot are left alone.
        roles
            .iter()
            .filter(|r| r.position < bot_position && !r.managed)
            .for_each(|r| strip(&mut p, r, DANGEROUS));
    }
    if let Some(j) = join {
        p.snapshot.join_lock = true;
        p.ops.push(Op::EnableJoinLock(j));
        if j == JoinLock::Kick {
            p.snapshot.invites_paused = true;
            p.ops.push(Op::PauseInvites);
        }
    }
    p
}

pub fn kind_from(name: &str, channel: Option<u64>) -> Option<LockKind> {
    Some(match name {
        // Wick's codes: c channel, sc channels (server), k kick, b ban, r roles, s/all everything.
        "server" | "sc" => LockKind::Server,
        "blind" => LockKind::Blind,
        "role" | "roles" | "r" => LockKind::Role,
        "all" | "s" => LockKind::All,
        "kick" | "k" => LockKind::Join(JoinLock::Kick),
        "ban" | "b" => LockKind::Join(JoinLock::Ban),
        "channel" | "c" => LockKind::Channel(channel?),
        _ => return None,
    })
}

/// A role-type permission overwrite on a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Overwrite {
    pub channel: u64,
    pub role: u64,
    pub allow: u64,
    pub deny: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Fix {
    SetRolePerms { role: u64, perms: u64 },
    SetOverwrite(Overwrite),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub id: String,
    pub text: String,
    pub fix: Option<Fix>,
}

pub fn tshoot(
    guild_id: u64,
    main_roles: &[u64],
    roles: &[RoleInfo],
    overwrites: &[Overwrite],
    bot_position: i32,
) -> Vec<Problem> {
    let main: Vec<u64> = if main_roles.is_empty() {
        vec![guild_id]
    } else {
        main_roles.to_vec()
    };
    let mut out = vec![];
    for r in roles
        .iter()
        .filter(|r| main.contains(&r.id) && r.id != guild_id)
    {
        if r.position >= bot_position {
            out.push(Problem {
                id: format!("main_above_bot_{}", r.id),
                text: format!(
                    "Main role <@&{}> is not below the bot, so it cannot be locked.",
                    r.id
                ),
                fix: None,
            });
        }
    }
    for r in roles.iter().filter(|r| !main.contains(&r.id) && !r.managed) {
        if r.perms & (1 << 3) != 0 {
            out.push(Problem {
                id: format!("admin_{}", r.id),
                text: format!(
                    "Role <@&{}> has Administrator and ignores every lock.",
                    r.id
                ),
                fix: None,
            });
        } else if r.perms & SEND_MESSAGES != 0 {
            let fixable = r.position < bot_position;
            out.push(Problem {
                id: format!("bypass_{}", r.id),
                text: format!("Role <@&{}> grants Send Messages, so members with it can still talk during a lock.", r.id),
                fix: fixable.then_some(Fix::SetRolePerms {
                    role: r.id,
                    perms: r.perms & !SEND_MESSAGES,
                }),
            });
        }
    }
    // An explicit allow on a main role beats the stripped role perm, so the lock would not hold there.
    for o in overwrites
        .iter()
        .filter(|o| main.contains(&o.role) && o.allow & SEND_MESSAGES != 0)
    {
        out.push(Problem {
            id: format!("overwrite_{}_{}", o.channel, o.role),
            text: format!(
                "Channel <#{}> explicitly allows Send Messages for <@&{}>, so a lock does not apply there.",
                o.channel, o.role
            ),
            fix: Some(Fix::SetOverwrite(Overwrite {
                channel: o.channel,
                role: o.role,
                allow: o.allow & !SEND_MESSAGES,
                deny: o.deny,
            })),
        });
    }
    out
}

/// Ops that put every saved perm back exactly; join lock / invites are lifted by the caller via the flags.
pub fn unlock_ops(s: &Snapshot) -> Vec<Op> {
    s.role_perms
        .iter()
        .map(|&(role, perms)| Op::SetRolePerms { role, perms })
        .collect()
}

/// Auto lockdown: N mentions from non-whitelisted members within T locks the server.
#[derive(Debug, Default)]
pub struct MentionWatch(Window);

impl MentionWatch {
    pub fn on_mentions(&mut self, mentions: usize, now: i64, limit: usize, span_ms: i64) -> bool {
        let mut n = 0;
        for _ in 0..mentions {
            n = self.0.push(now, span_ms);
        }
        n >= limit
    }
}

/// Heat panic: N raiders trigger within T, then for `duration_ms` any raider message is a timeout.
#[derive(Debug, Default)]
pub struct Panic {
    window: Window,
    pub active_until: i64,
}

impl Panic {
    /// Call when a raider triggers the automod; returns true if panic just started.
    pub fn on_raider_trigger(
        &mut self,
        now: i64,
        limit: usize,
        span_ms: i64,
        duration_ms: i64,
    ) -> bool {
        if self.window.push(now, span_ms) >= limit && !self.active(now) {
            self.active_until = now + duration_ms;
            return true;
        }
        false
    }

    pub fn active(&self, now: i64) -> bool {
        now < self.active_until
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1;
    fn role(id: u64, perms: u64, position: i32) -> RoleInfo {
        RoleInfo {
            id,
            perms,
            position,
            managed: false,
        }
    }
    fn roles() -> Vec<RoleInfo> {
        vec![
            role(G, VIEW_CHANNEL | SEND_MESSAGES, 0),
            role(10, VIEW_CHANNEL | SEND_MESSAGES, 1),
            role(11, (1 << 2) | VIEW_CHANNEL, 2), // can ban
            role(12, 1 << 3, 9),                  // admin, above bot
        ]
    }

    #[test]
    fn server_lock_is_one_patch_per_main_role_and_restores_exactly() {
        let p = plan(&LockKind::Server, G, &[10], &roles(), 5);
        assert_eq!(
            p.ops,
            vec![Op::SetRolePerms {
                role: 10,
                perms: VIEW_CHANNEL
            }]
        );
        assert_eq!(
            unlock_ops(&p.snapshot),
            vec![Op::SetRolePerms {
                role: 10,
                perms: VIEW_CHANNEL | SEND_MESSAGES
            }]
        );
    }

    #[test]
    fn no_main_roles_falls_back_to_everyone_and_blind_hides_channels() {
        let p = plan(&LockKind::Blind, G, &[], &roles(), 5);
        assert_eq!(
            p.ops,
            vec![
                Op::SetRolePerms { role: G, perms: 0 },
                Op::CreateLockedChannel
            ]
        );
    }

    #[test]
    fn role_lock_skips_roles_at_or_above_bot() {
        let p = plan(&LockKind::Role, G, &[], &roles(), 5);
        assert_eq!(
            p.ops,
            vec![Op::SetRolePerms {
                role: 11,
                perms: VIEW_CHANNEL
            }]
        );
        assert_eq!(p.snapshot.role_perms, vec![(11, (1 << 2) | VIEW_CHANNEL)]);
    }

    #[test]
    fn all_combines_without_double_snapshotting() {
        let p = plan(&LockKind::All, G, &[11], &roles(), 5);
        // role 11 is both a main role and dangerous: snapshotted once, with the original perms
        assert_eq!(
            p.snapshot
                .role_perms
                .iter()
                .filter(|(id, _)| *id == 11)
                .count(),
            1
        );
        assert!(p.ops.contains(&Op::EnableJoinLock(JoinLock::Ban)));
    }

    #[test]
    fn kick_join_lock_pauses_invites_and_channel_lock_denies_main() {
        let p = plan(&LockKind::Join(JoinLock::Kick), G, &[], &[], 5);
        assert_eq!(
            p.ops,
            vec![Op::EnableJoinLock(JoinLock::Kick), Op::PauseInvites]
        );
        let p = plan(&LockKind::Channel(77), G, &[10, 11], &[], 5);
        assert_eq!(p.ops.len(), 2);
    }

    #[test]
    fn tshoot_flags_bypass_roles_and_unlockable_main_roles() {
        let ow = [Overwrite {
            channel: 7,
            role: 10,
            allow: SEND_MESSAGES | VIEW_CHANNEL,
            deny: 0,
        }];
        let p = tshoot(G, &[10], &roles(), &ow, 5);
        let ids: Vec<&str> = p.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["bypass_1", "admin_12", "overwrite_7_10"]);
        assert_eq!(
            p[0].fix,
            Some(Fix::SetRolePerms {
                role: 1,
                perms: VIEW_CHANNEL
            })
        );
        assert_eq!(
            p[2].fix,
            Some(Fix::SetOverwrite(Overwrite {
                channel: 7,
                role: 10,
                allow: VIEW_CHANNEL,
                deny: 0
            }))
        );
        assert!(tshoot(G, &[12], &roles(), &[], 5)[0]
            .id
            .starts_with("main_above_bot"));
        assert_eq!(kind_from("channel", None), None);
        assert_eq!(kind_from("c", Some(7)), Some(LockKind::Channel(7)));
        assert_eq!(kind_from("sc", None), Some(LockKind::Server));
        assert_eq!(kind_from("r", None), Some(LockKind::Role));
        assert_eq!(kind_from("s", None), Some(LockKind::All));
        assert_eq!(kind_from("b", None), Some(LockKind::Join(JoinLock::Ban)));
        assert_eq!(
            kind_from("kick", None),
            Some(LockKind::Join(JoinLock::Kick))
        );
    }

    #[test]
    fn mixed_lock_covers_what_the_settings_switch_on() {
        let prefs = LockdownPrefs {
            channels: true,
            roles: true,
            joins: false,
            updates: false,
        };
        let p = plan(&prefs.kind(), G, &[10], &roles(), 5);
        assert!(p.ops.contains(&Op::SetRolePerms {
            role: 10,
            perms: VIEW_CHANNEL
        }));
        assert!(p
            .ops
            .iter()
            .any(|o| matches!(o, Op::SetRolePerms { role: 11, .. })));
        assert!(!p.snapshot.join_lock);
        assert_eq!(
            LockKind::Mixed {
                channels: false,
                roles: false,
                joins: true
            }
            .covers()
            .2,
            Some(JoinLock::Ban)
        );
        let none = LockdownPrefs {
            channels: false,
            ..prefs
        };
        assert_eq!(
            LockdownPrefs {
                roles: false,
                ..none
            }
            .kind(),
            LockKind::Server
        );
    }

    #[test]
    fn mention_watch_and_panic() {
        let mut w = MentionWatch::default();
        assert!(!w.on_mentions(49, 0, 50, 30_000));
        assert!(w.on_mentions(1, 10_000, 50, 30_000));
        let mut w = MentionWatch::default();
        w.on_mentions(30, 0, 50, 30_000);
        assert!(!w.on_mentions(30, 40_000, 50, 30_000)); // old ones aged out

        let mut p = Panic::default();
        assert!(!p.on_raider_trigger(0, 3, 10_000, 600_000));
        assert!(!p.on_raider_trigger(1000, 3, 10_000, 600_000));
        assert!(p.on_raider_trigger(2000, 3, 10_000, 600_000));
        assert!(p.active(5000) && !p.active(700_000));
    }
}
