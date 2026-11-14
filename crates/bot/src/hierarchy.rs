//! Per-guild owner, roles, channel parents and the bot's top role, so a hierarchy check costs one call.
use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use twilight_model::{
    guild::Role,
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::app::App;

const TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct GuildInfo {
    pub owner: Id<UserMarker>,
    pub roles: HashMap<Id<RoleMarker>, i64>,
    pub role_perms: HashMap<Id<RoleMarker>, u64>,
    /// Bot, booster and integration roles: Discord hands these out itself.
    pub managed: HashSet<Id<RoleMarker>>,
    /// Linked roles and server subscription or purchase roles: Discord gives them, so they carry
    /// no permit (Q5). Onboarding roles are not marked by Discord and are refused at save only.
    pub unpermittable: HashSet<Id<RoleMarker>>,
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

impl GuildInfo {
    /// [`tommybot_core::roles::unfit`] for a role of this guild.
    pub fn unfit(&self, gid: Id<GuildMarker>, role: Id<RoleMarker>) -> Option<&'static str> {
        let rank = self.roles.get(&role).copied().unwrap_or(0);
        let managed = self.managed.contains(&role);
        tommybot_core::roles::unfit(gid.get(), role.get(), managed, rank, self.bot_top)
    }
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
            unpermittable: all
                .iter()
                .filter(|r| {
                    r.tags.as_ref().is_some_and(|t| {
                        t.guild_connections
                            || t.available_for_purchase
                            || t.subscription_listing_id.is_some()
                    })
                })
                .map(|r| r.id)
                .collect(),
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
        crate::verify::given::invalidated(gid, None);
    }

    /// [`App::invalidate_guild`] for an event about one role or channel: TommyBot's own marked
    /// edit of it only softens the verification gate.
    pub fn invalidate_object(&self, gid: Id<GuildMarker>, object: u64) {
        self.guilds.lock().unwrap().remove(&gid);
        crate::verify::given::invalidated(gid, Some(object));
    }

    /// Tests: a cached server with this owner, so no Discord call is needed for it.
    #[cfg(test)]
    pub fn seed_guild(&self, gid: Id<GuildMarker>, owner: Id<UserMarker>) {
        let info = GuildInfo {
            owner,
            roles: HashMap::new(),
            role_perms: HashMap::new(),
            managed: HashSet::new(),
            unpermittable: HashSet::new(),
            parents: HashMap::new(),
            channels: HashSet::new(),
            nsfw: HashSet::new(),
            bot_top: 0,
            fetched: Instant::now(),
        };
        self.guilds.lock().unwrap().insert(gid, Arc::new(info));
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

    /// A channel's category as a gateway event shows it (D4). A snapshot (`GuildCreate`, a new
    /// channel) sets the baseline; an update to another category is a move.
    pub fn note_category(&self, channel: u64, parent: Option<u64>, snapshot: bool) {
        let mut m = self.category_moves.lock().unwrap();
        // shortcut: past the cap everything is dropped; a later update then counts as a move.
        if m.len() > 200_000 {
            m.clear();
        }
        let moved = moved(m.get(&channel), parent, snapshot, Instant::now());
        m.insert(channel, (parent, moved));
    }

    /// The channel came from outside `cats` (or from where nobody saw) in the last hour.
    pub fn moved_in(&self, channel: u64, cats: &[Id<ChannelMarker>]) -> bool {
        self.category_moves
            .lock()
            .unwrap()
            .get(&channel)
            .and_then(|e| e.1)
            .is_some_and(|(t, from)| {
                t.elapsed() < MOVE_WINDOW
                    && !from.is_some_and(|f| cats.iter().any(|c| c.get() == f))
            })
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

/// How long a move into a category counts against it.
const MOVE_WINDOW: Duration = Duration::from_secs(3600);

/// The channel's first move in the last hour and the category it left: kept while the category
/// stays or the move is recent, none for a snapshot, now for another move (from an unseen place
/// with nothing seen before).
fn moved(
    prev: Option<&(Option<u64>, CategoryMove)>,
    parent: Option<u64>,
    snapshot: bool,
    now: Instant,
) -> CategoryMove {
    match prev {
        Some((old, m)) if *old == parent => *m,
        _ if snapshot => None,
        Some((_, Some((t, from)))) if now.duration_since(*t) < MOVE_WINDOW => Some((*t, *from)),
        Some((old, _)) => Some((now, *old)),
        None => Some((now, None)),
    }
}

/// When a channel moved category and the category it left.
pub(crate) type CategoryMove = Option<(Instant, Option<u64>)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_into_another_category_is_remembered() {
        let t = Instant::now();
        assert_eq!(moved(None, Some(5), true, t), None, "new channel");
        assert_eq!(
            moved(Some(&(Some(4), None)), Some(5), false, t),
            Some((t, Some(4)))
        );
        assert_eq!(
            moved(Some(&(Some(5), None)), Some(5), false, t),
            None,
            "a rename"
        );
        assert_eq!(
            moved(None, Some(5), false, t),
            Some((t, None)),
            "no baseline"
        );
        // Outside, then ticket category 4, then 5: the first move, from outside, still counts.
        let first = Some((t, Some(1)));
        let later = t + Duration::from_secs(5);
        assert_eq!(moved(Some(&(Some(4), first)), Some(5), false, later), first);
        let old = t + MOVE_WINDOW;
        assert_eq!(
            moved(Some(&(Some(4), first)), Some(5), false, old),
            Some((old, Some(4)))
        );
    }

    #[test]
    fn top_position_picks_highest_known_role() {
        let roles = HashMap::from([(Id::new(1), 3), (Id::new(2), 7)]);
        assert_eq!(top_position(&roles, &[Id::new(1), Id::new(2)]), 7);
        assert_eq!(top_position(&roles, &[Id::new(9)]), 0); // unknown role
        assert_eq!(top_position(&roles, &[]), 0);
    }
}
