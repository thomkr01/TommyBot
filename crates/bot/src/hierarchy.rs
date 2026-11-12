//! Per-guild owner, roles, channel parents and the bot's top role, so a hierarchy check costs one call.
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use twilight_model::{
    guild::{Permissions, Role},
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::app::App;

const TTL: Duration = Duration::from_secs(300);

pub struct GuildInfo {
    pub owner: Id<UserMarker>,
    pub roles: HashMap<Id<RoleMarker>, i64>,
    pub role_perms: HashMap<Id<RoleMarker>, u64>,
    /// Bot, booster and integration roles: Discord hands these out itself.
    pub managed: HashSet<Id<RoleMarker>>,
    /// Channel -> its category (threads are not listed).
    pub parents: HashMap<Id<ChannelMarker>, Id<ChannelMarker>>,
    /// Every channel but threads.
    pub channels: HashSet<Id<ChannelMarker>>,
    /// Age-restricted channels (threads follow their parent).
    pub nsfw: HashSet<Id<ChannelMarker>>,
    pub bot_top: i64,
    pub(crate) fetched: Instant,
}

/// The roles with each position rewritten to its rank in Discord's order (ties broken by id), so
/// every compare and every position sent back follows the order Discord enforces.
pub fn ranked(gid: Id<GuildMarker>, mut roles: Vec<Role>) -> Vec<Role> {
    let rank =
        tommybot_core::roles::ranks(roles.iter().map(|r| (r.id.get(), r.position)), gid.get());
    for r in &mut roles {
        r.position = rank[&r.id.get()];
    }
    roles
}

/// Highest role position among `member_roles` (0 = @everyone only).
pub fn top_position(roles: &HashMap<Id<RoleMarker>, i64>, member_roles: &[Id<RoleMarker>]) -> i64 {
    member_roles
        .iter()
        .filter_map(|r| roles.get(r))
        .copied()
        .max()
        .unwrap_or(0)
}

impl App {
    /// Cached for 5 minutes; gateway role/channel/guild/bot-member events call [`App::invalidate_guild`].
    pub async fn guild_info(&self, gid: Id<GuildMarker>) -> Result<Arc<GuildInfo>> {
        if let Some(i) = self.guilds.lock().unwrap().get(&gid) {
            if i.fetched.elapsed() < TTL {
                return Ok(i.clone());
            }
        }
        let owner = self.http.guild(gid).await?.model().await?.owner_id;
        let all = ranked(gid, self.http.roles(gid).await?.models().await?);
        let roles: HashMap<_, _> = all.iter().map(|r| (r.id, r.position)).collect();
        let all_channels = self.http.guild_channels(gid).await?.models().await?;
        let parents = all_channels
            .iter()
            .filter_map(|c| Some((c.id, c.parent_id?)))
            .collect();
        let me = self.http.guild_member(gid, self.me).await?.model().await?;
        let info = Arc::new(GuildInfo {
            owner,
            bot_top: top_position(&roles, &me.roles),
            role_perms: all.iter().map(|r| (r.id, r.permissions.bits())).collect(),
            managed: all.iter().filter(|r| r.managed).map(|r| r.id).collect(),
            parents,
            channels: all_channels.iter().map(|c| c.id).collect(),
            nsfw: all_channels
                .iter()
                .filter(|c| c.nsfw == Some(true))
                .map(|c| c.id)
                .collect(),
            roles,
            fetched: Instant::now(),
        });
        self.guilds.lock().unwrap().insert(gid, info.clone());
        Ok(info)
    }

    pub fn invalidate_guild(&self, gid: Id<GuildMarker>) {
        self.guilds.lock().unwrap().remove(&gid);
    }

    /// Tests: a cached server with this owner, so no Discord call is needed for it.
    #[cfg(test)]
    pub fn seed_guild(&self, gid: Id<GuildMarker>, owner: Id<UserMarker>) {
        let info = GuildInfo {
            owner,
            roles: HashMap::new(),
            role_perms: HashMap::new(),
            managed: HashSet::new(),
            parents: HashMap::new(),
            channels: HashSet::new(),
            nsfw: HashSet::new(),
            bot_top: 0,
            fetched: Instant::now(),
        };
        self.guilds.lock().unwrap().insert(gid, Arc::new(info));
    }

    /// Members with dangerous permissions are spam-whitelisted unless only permits count as staff.
    pub async fn staff_by_perms(
        &self,
        cfg: &crate::app::GuildCfg,
        gid: Id<GuildMarker>,
        roles: &[Id<RoleMarker>],
    ) -> bool {
        const BITS: Permissions = Permissions::ADMINISTRATOR
            .union(Permissions::KICK_MEMBERS)
            .union(Permissions::BAN_MEMBERS)
            .union(Permissions::MANAGE_CHANNELS)
            .union(Permissions::MANAGE_GUILD)
            .union(Permissions::MANAGE_MESSAGES)
            .union(Permissions::MANAGE_ROLES)
            .union(Permissions::MANAGE_WEBHOOKS);
        // A failed lookup must not switch automod off.
        !cfg.misc.staff_only_permitted
            && self.guild_info(gid).await.is_ok_and(|info| {
                roles
                    .iter()
                    .any(|r| info.role_perms.get(r).is_some_and(|p| p & BITS.bits() != 0))
            })
    }

    /// The channel and category whitelists apply to: a thread's parent channel, and that
    /// channel's category.
    pub async fn channel_scope(
        &self,
        gid: Id<GuildMarker>,
        ch: Id<ChannelMarker>,
    ) -> Result<(Id<ChannelMarker>, Option<Id<ChannelMarker>>)> {
        let info = self.guild_info(gid).await?;
        let channel = if info.channels.contains(&ch) {
            ch
        } else {
            let known = self.thread_parents.lock().unwrap().get(&ch).copied();
            match known {
                Some(p) => p,
                None => {
                    let p = self.http.channel(ch).await?.model().await?.parent_id;
                    if let Some(p) = p {
                        self.note_thread(ch, p);
                    }
                    p.unwrap_or(ch)
                }
            }
        };
        Ok((channel, info.parents.get(&channel).copied()))
    }

    /// Remembers a thread's parent channel (from thread events or a fetch).
    pub fn note_thread(&self, thread: Id<ChannelMarker>, parent: Id<ChannelMarker>) {
        let mut m = self.thread_parents.lock().unwrap();
        // shortcut: past the cap everything is dropped and refetched once per thread.
        if m.len() > 50_000 {
            m.clear();
        }
        m.insert(thread, parent);
    }

    /// Remembers a created or deleted channel's category until its audit entry arrives.
    /// Channels without a category are noted too, so anti-nuke can tell "no category" from "not seen yet".
    pub fn note_parent(&self, c: &twilight_model::channel::Channel) {
        let mut m = self.channel_parents.lock().unwrap();
        // shortcut: entries whose audit entry never comes (anti-nuke off) pile up until this cap clears them all.
        if m.len() > 10_000 {
            m.clear();
        }
        m.insert(c.id.get(), c.parent_id.map(|p| p.get()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_position_picks_highest_known_role() {
        let roles = HashMap::from([(Id::new(1), 3), (Id::new(2), 7)]);
        assert_eq!(top_position(&roles, &[Id::new(1), Id::new(2)]), 7);
        assert_eq!(top_position(&roles, &[Id::new(9)]), 0); // unknown role
        assert_eq!(top_position(&roles, &[]), 0);
    }
}
