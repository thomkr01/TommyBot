//! Per-guild owner, role positions and the bot's top role, so a hierarchy check costs one call.
use anyhow::Result;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use twilight_model::id::{
    marker::{GuildMarker, RoleMarker, UserMarker},
    Id,
};

use crate::app::App;

const TTL: Duration = Duration::from_secs(300);

pub struct GuildInfo {
    pub owner: Id<UserMarker>,
    pub roles: HashMap<Id<RoleMarker>, i64>,
    pub bot_top: i64,
    fetched: Instant,
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
    /// Cached for 5 minutes; gateway role/guild/bot-member events call [`App::invalidate_guild`].
    pub async fn guild_info(&self, gid: Id<GuildMarker>) -> Result<Arc<GuildInfo>> {
        if let Some(i) = self.guilds.lock().unwrap().get(&gid) {
            if i.fetched.elapsed() < TTL {
                return Ok(i.clone());
            }
        }
        let owner = self.http.guild(gid).await?.model().await?.owner_id;
        let roles: HashMap<_, _> = self
            .http
            .roles(gid)
            .await?
            .models()
            .await?
            .into_iter()
            .map(|r| (r.id, r.position))
            .collect();
        let me = self.http.guild_member(gid, self.me).await?.model().await?;
        let info = Arc::new(GuildInfo {
            owner,
            bot_top: top_position(&roles, &me.roles),
            roles,
            fetched: Instant::now(),
        });
        self.guilds.lock().unwrap().insert(gid, info.clone());
        Ok(info)
    }

    pub fn invalidate_guild(&self, gid: Id<GuildMarker>) {
        self.guilds.lock().unwrap().remove(&gid);
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
