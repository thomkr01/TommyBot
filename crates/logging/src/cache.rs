use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};
use twilight_model::id::{
    marker::{GuildMarker, MessageMarker, UserMarker},
    Id,
};

/// Discord does not send deleted or old message content, so we keep recent messages ourselves.
pub const TTL: Duration = Duration::from_secs(30 * 60);
/// Per guild. Oldest entries go first once this is reached.
pub const CAPACITY: usize = 10_000;

#[derive(Clone, Debug)]
pub struct CachedMessage {
    pub author: Id<UserMarker>,
    pub content: String,
    inserted: Instant,
}

impl CachedMessage {
    pub fn new(author: Id<UserMarker>, content: String, now: Instant) -> Self {
        Self {
            author,
            content,
            inserted: now,
        }
    }
}

#[derive(Default)]
struct GuildMessages {
    map: HashMap<Id<MessageMarker>, CachedMessage>,
    /// Insertion order, used for eviction. An id can appear more than once after an edit.
    order: VecDeque<(Instant, Id<MessageMarker>)>,
}

#[derive(Default)]
pub struct MessageCache {
    guilds: HashMap<Id<GuildMarker>, GuildMessages>,
}

impl MessageCache {
    pub fn insert(
        &mut self,
        guild: Id<GuildMarker>,
        id: Id<MessageMarker>,
        msg: CachedMessage,
        now: Instant,
    ) {
        let g = self.guilds.entry(guild).or_default();
        g.order.push_back((msg.inserted, id));
        g.map.insert(id, msg);
        g.prune(now);
    }

    /// Expired entries are treated as missing, even before they are pruned.
    pub fn get(
        &self,
        guild: Id<GuildMarker>,
        id: Id<MessageMarker>,
        now: Instant,
    ) -> Option<&CachedMessage> {
        self.guilds
            .get(&guild)?
            .map
            .get(&id)
            .filter(|m| now.duration_since(m.inserted) < TTL)
    }

    pub fn remove(
        &mut self,
        guild: Id<GuildMarker>,
        id: Id<MessageMarker>,
    ) -> Option<CachedMessage> {
        self.guilds.get_mut(&guild)?.map.remove(&id)
    }
}

impl GuildMessages {
    fn prune(&mut self, now: Instant) {
        while let Some(&(at, id)) = self.order.front() {
            let expired = now.duration_since(at) >= TTL;
            if !expired && self.order.len() <= CAPACITY {
                break;
            }
            self.order.pop_front();
            // Skip stale order entries left behind by an edit: the live copy has a newer timestamp.
            if self.map.get(&id).is_some_and(|m| m.inserted == at) {
                self.map.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(now: Instant) -> CachedMessage {
        CachedMessage::new(Id::new(9), "hi".into(), now)
    }

    #[test]
    fn expires_after_ttl() {
        let t0 = Instant::now();
        let mut c = MessageCache::default();
        c.insert(Id::new(1), Id::new(2), msg(t0), t0);
        assert!(c
            .get(Id::new(1), Id::new(2), t0 + TTL - Duration::from_secs(1))
            .is_some());
        assert!(c.get(Id::new(1), Id::new(2), t0 + TTL).is_none());
    }

    #[test]
    fn evicts_oldest_over_capacity() {
        let t0 = Instant::now();
        let mut c = MessageCache::default();
        for i in 0..=CAPACITY as u64 {
            c.insert(Id::new(1), Id::new(i + 1), msg(t0), t0);
        }
        assert!(c.get(Id::new(1), Id::new(1), t0).is_none());
        assert!(c
            .get(Id::new(1), Id::new(CAPACITY as u64 + 1), t0)
            .is_some());
    }

    #[test]
    fn edit_keeps_newest_copy() {
        let t0 = Instant::now();
        let mut c = MessageCache::default();
        c.insert(Id::new(1), Id::new(2), msg(t0), t0);
        let t1 = t0 + TTL - Duration::from_secs(1);
        c.insert(Id::new(1), Id::new(2), msg(t1), t1);
        // The first order entry expires now, but the edited copy must survive it.
        let t2 = t0 + TTL + Duration::from_secs(1);
        c.insert(Id::new(1), Id::new(3), msg(t2), t2);
        assert!(c.get(Id::new(1), Id::new(2), t2).is_some());
    }
}
