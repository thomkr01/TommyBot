use std::{cmp::Reverse, collections::HashMap};

/// Each role's place in Discord's role order, as a dense position with @everyone at 0. Discord
/// breaks a position tie by id, the older (lower) id ranking higher. Every role on a server whose
/// roles were never reordered sits at position 1, so comparing raw positions there is wrong.
pub fn ranks(roles: impl IntoIterator<Item = (u64, i64)>, everyone: u64) -> HashMap<u64, i64> {
    let mut v: Vec<_> = roles.into_iter().collect();
    v.sort_by_key(|&(id, pos)| (id != everyone, pos, Reverse(id)));
    v.into_iter()
        .enumerate()
        .map(|(i, (id, _))| (id, i as i64))
        .collect()
}

/// Why `role` cannot be the quarantine role or a main role, which the bot edits and hands out:
/// @everyone, a managed (bot, booster, integration) role, or one not below the bot's top role.
/// `rank` and `bot_top` are [`ranks`] positions.
pub fn unfit(
    guild: u64,
    role: u64,
    managed: bool,
    rank: i64,
    bot_top: i64,
) -> Option<&'static str> {
    if role == guild {
        Some("it is @everyone (leave the main roles empty to use @everyone)")
    } else if managed {
        Some("Discord manages it (a bot, booster or integration role)")
    } else if rank >= bot_top {
        Some("it is not below TommyBot's highest role")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyone_managed_and_high_roles_are_unfit() {
        assert!(unfit(1, 1, false, 0, 5).is_some());
        assert!(unfit(1, 2, true, 2, 5).is_some());
        assert!(unfit(1, 2, false, 5, 5).is_some());
        assert_eq!(unfit(1, 2, false, 4, 5), None);
    }

    #[test]
    fn ties_rank_the_older_id_higher() {
        let r = ranks([(9, 1), (3, 1), (5, 1), (1, 0)], 1);
        assert_eq!((r[&1], r[&9], r[&5], r[&3]), (0, 1, 2, 3));
    }

    #[test]
    fn untied_positions_keep_their_order() {
        let r = ranks([(1, 0), (7, 2), (8, 1), (2, 3)], 1);
        assert_eq!((r[&1], r[&8], r[&7], r[&2]), (0, 1, 2, 3));
    }
}
