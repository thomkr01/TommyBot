//! Role-level lockdown: few API calls, exact restore from a snapshot (spec 4.5).
//! `plan` turns the guild's role list into a set of ops; the bot executes them and stores the snapshot.
use crate::window::Window;
use serde::{Deserialize, Serialize};

pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
/// Perms stripped by role lock: admin, kick, ban, manage channels/guild/messages/roles/webhooks/
/// expressions/nicknames/events/threads, mention everyone, moderate members, view audit log.
pub const DANGEROUS: u64 = (1 << 3) | (1 << 1) | (1 << 2) | (1 << 4) | (1 << 5) | (1 << 7) | (1 << 13)
    | (1 << 17) | (1 << 27) | (1 << 28) | (1 << 29) | (1 << 30) | (1 << 33) | (1 << 34) | (1 << 40);

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
    SetRolePerms { role: u64, perms: u64 },
    /// Deny Send Messages for `role` on `channel` (channel lock).
    DenyChannelSend { channel: u64, role: u64 },
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
pub fn plan(kind: &LockKind, guild_id: u64, main_roles: &[u64], roles: &[RoleInfo], bot_position: i32) -> Plan {
    let mut p = Plan { ops: vec![], snapshot: Snapshot { kind: Some(kind.clone()), ..Default::default() } };
    let main: Vec<u64> = if main_roles.is_empty() { vec![guild_id] } else { main_roles.to_vec() };
    let strip = |p: &mut Plan, r: &RoleInfo, mask: u64| {
        let perms = r.perms & !mask;
        if perms != r.perms && !p.snapshot.role_perms.iter().any(|(id, _)| *id == r.id) {
            p.snapshot.role_perms.push((r.id, r.perms));
            p.ops.push(Op::SetRolePerms { role: r.id, perms });
        }
    };
    let (server, blind) = (matches!(kind, LockKind::Server | LockKind::All), matches!(kind, LockKind::Blind));
    if server || blind {
        let mask = if blind { SEND_MESSAGES | VIEW_CHANNEL } else { SEND_MESSAGES };
        roles.iter().filter(|r| main.contains(&r.id)).for_each(|r| strip(&mut p, r, mask));
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
    if matches!(kind, LockKind::Role | LockKind::All) {
        // Bot-managed roles and anything at/above the bot are left alone.
        roles.iter().filter(|r| r.position < bot_position && !r.managed).for_each(|r| strip(&mut p, r, DANGEROUS));
    }
    let join = match kind {
        LockKind::Join(j) => Some(*j),
        LockKind::All => Some(JoinLock::Ban),
        _ => None,
    };
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

/// Ops that put every saved perm back exactly; join lock / invites are lifted by the caller via the flags.
pub fn unlock_ops(s: &Snapshot) -> Vec<Op> {
    s.role_perms.iter().map(|&(role, perms)| Op::SetRolePerms { role, perms }).collect()
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
    pub fn on_raider_trigger(&mut self, now: i64, limit: usize, span_ms: i64, duration_ms: i64) -> bool {
        if self.window.push(now, span_ms) >= limit && !self.active(now) {
            self.active_until = now + duration_ms;
            return true;
        }
        false
    }

    pub fn active(&self, now: i64) -> bool {
        now < self.active_until
    }

    /// Raiders messaging during panic get timed out; regular members keep the normal rules.
    pub fn punish(&self, now: i64, is_raider: bool) -> bool {
        is_raider && self.active(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1;
    fn role(id: u64, perms: u64, position: i32) -> RoleInfo {
        RoleInfo { id, perms, position, managed: false }
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
        assert_eq!(p.ops, vec![Op::SetRolePerms { role: 10, perms: VIEW_CHANNEL }]);
        assert_eq!(unlock_ops(&p.snapshot), vec![Op::SetRolePerms { role: 10, perms: VIEW_CHANNEL | SEND_MESSAGES }]);
    }

    #[test]
    fn no_main_roles_falls_back_to_everyone_and_blind_hides_channels() {
        let p = plan(&LockKind::Blind, G, &[], &roles(), 5);
        assert_eq!(p.ops, vec![Op::SetRolePerms { role: G, perms: 0 }, Op::CreateLockedChannel]);
    }

    #[test]
    fn role_lock_skips_roles_at_or_above_bot() {
        let p = plan(&LockKind::Role, G, &[], &roles(), 5);
        assert_eq!(p.ops, vec![Op::SetRolePerms { role: 11, perms: VIEW_CHANNEL }]);
        assert_eq!(p.snapshot.role_perms, vec![(11, (1 << 2) | VIEW_CHANNEL)]);
    }

    #[test]
    fn all_combines_without_double_snapshotting() {
        let p = plan(&LockKind::All, G, &[11], &roles(), 5);
        // role 11 is both a main role and dangerous: snapshotted once, with the original perms
        assert_eq!(p.snapshot.role_perms.iter().filter(|(id, _)| *id == 11).count(), 1);
        assert!(p.ops.contains(&Op::EnableJoinLock(JoinLock::Ban)));
    }

    #[test]
    fn kick_join_lock_pauses_invites_and_channel_lock_denies_main() {
        let p = plan(&LockKind::Join(JoinLock::Kick), G, &[], &[], 5);
        assert_eq!(p.ops, vec![Op::EnableJoinLock(JoinLock::Kick), Op::PauseInvites]);
        let p = plan(&LockKind::Channel(77), G, &[10, 11], &[], 5);
        assert_eq!(p.ops.len(), 2);
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
        assert!(p.punish(5000, true) && !p.punish(5000, false) && !p.punish(700_000, true));
    }
}
