//! Shared bot state and the per-guild config cache. The dashboard appends a `guild_config` row and
//! sends `NOTIFY config_changed <guild>`; we drop that guild's cached copy and reload lazily.
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::{postgres::PgListener, PgPool};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tommybot_antinuke::{Action as NukeAction, AntiNuke, Limit};
use tommybot_automod::{
    heat::{HeatCfg, HeatState, UserHistory},
    join::{GateCfg, RaidCfg, RaidDetector},
    lockdown::{JoinLock, MentionWatch},
    strike::StrikeCfg,
};
use tommybot_core::permits::{self, Level};
use tommybot_logging::{LogConfig, Logger};
use twilight_http::Client;
use twilight_model::id::{
    marker::{GuildMarker, RoleMarker, UserMarker},
    Id,
};

#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub struct AntinukeCfg {
    pub limits: HashMap<NukeAction, Limit>,
    /// Quarantined members are moved to this role (none: all roles removed).
    pub quarantine_role: Option<Id<RoleMarker>>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub struct AutomodCfg {
    pub enabled: bool,
    pub heat: HeatCfg,
    pub strike: StrikeCfg,
    /// (mentions, window ms): that many mentions in the window locks the server.
    pub mention_lock: Option<(usize, i64)>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub struct LockdownCfg {
    /// Roles that lose Send Messages in a server lock; empty means @everyone.
    pub main_roles: Vec<Id<RoleMarker>>,
}

#[derive(Default)]
pub struct GuildCfg {
    pub logging: LogConfig,
    pub antinuke: AntinukeCfg,
    pub automod: AutomodCfg,
    pub joingate: GateCfg,
    pub joinraid: Option<RaidCfg>,
    pub lockdown: LockdownCfg,
}

/// One dashboard section; a section that fails to parse falls back to its default.
fn section<T: DeserializeOwned + Default>(cfg: &Value, name: &str) -> T {
    match cfg.get(name).cloned().map(serde_json::from_value) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            tracing::warn!(section = name, %e, "bad config section, using defaults");
            T::default()
        }
        None => T::default(),
    }
}

impl GuildCfg {
    fn parse(cfg: &Value) -> Self {
        Self {
            logging: section(cfg, "logging"),
            antinuke: section(cfg, "antinuke"),
            automod: section(cfg, "automod"),
            joingate: section(cfg, "joingate"),
            joinraid: section(cfg, "joinraid"),
            lockdown: section(cfg, "lockdown"),
        }
    }
}

pub struct App {
    pub http: Client,
    pub pool: PgPool,
    pub me: Id<UserMarker>,
    cfgs: RwLock<HashMap<u64, Arc<GuildCfg>>>,
    pub logger: Mutex<Logger>,
    pub antinuke: Mutex<AntiNuke>,
    pub heat: Mutex<HashMap<(u64, u64), (HeatState, UserHistory)>>,
    pub raids: Mutex<HashMap<u64, RaidDetector>>,
    pub mentions: Mutex<HashMap<u64, MentionWatch>>,
    /// Active join locks. shortcut: lost on restart, persist via the lock snapshot when needed.
    pub join_locks: Mutex<HashMap<u64, JoinLock>>,
}

impl App {
    pub fn new(http: Client, pool: PgPool, me: Id<UserMarker>) -> Self {
        Self {
            http,
            pool,
            me,
            cfgs: Default::default(),
            logger: Default::default(),
            antinuke: Mutex::new(AntiNuke::new(HashMap::new())),
            heat: Default::default(),
            raids: Default::default(),
            mentions: Default::default(),
            join_locks: Default::default(),
        }
    }

    /// Permit level of a member; a user who is not in the server has none.
    pub async fn level(
        &self,
        guild: Id<GuildMarker>,
        owner: Id<UserMarker>,
        user: Id<UserMarker>,
    ) -> Result<Level> {
        let roles: Vec<i64> = match self.http.guild_member(guild, user).await {
            Ok(r) => r
                .model()
                .await?
                .roles
                .iter()
                .map(|r| r.get() as i64)
                .collect(),
            Err(_) => vec![],
        };
        permits::resolve(
            &self.pool,
            guild.get() as i64,
            owner.get() as i64,
            user.get() as i64,
            &roles,
        )
        .await
    }

    /// Owner, Extra Owner or Trusted Admin: untouchable by every bot system.
    pub async fn is_immune(&self, guild: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
        let owner = self.http.guild(guild).await?.model().await?.owner_id;
        Ok(self.level(guild, owner, user).await?.immune())
    }

    pub async fn cfg(&self, guild: u64) -> Result<Arc<GuildCfg>> {
        if let Some(c) = self.cfgs.read().unwrap().get(&guild) {
            return Ok(c.clone());
        }
        let row: Option<(Value,)> = sqlx::query_as(
            "SELECT config FROM guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(guild as i64)
        .fetch_optional(&self.pool)
        .await?;
        let cfg = Arc::new(row.map_or_else(GuildCfg::default, |(v,)| GuildCfg::parse(&v)));
        self.cfgs.write().unwrap().insert(guild, cfg.clone());
        Ok(cfg)
    }

    /// Reloads configs when the dashboard saves. Reconnects by itself; a gap only delays a reload.
    pub async fn listen_config(self: Arc<Self>) -> Result<()> {
        let mut l = PgListener::connect_with(&self.pool).await?;
        l.listen("config_changed").await?;
        loop {
            let n = l.recv().await?;
            if let Ok(g) = n.payload().parse::<u64>() {
                self.cfgs.write().unwrap().remove(&g);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sections_and_survives_bad_ones() {
        let c = GuildCfg::parse(&json!({
            "antinuke": {"limits": {"Ban": {"per_minute": 3, "per_hour": 10}}, "quarantine_role": "7"},
            "automod": {"enabled": true, "mention_lock": [5, 10000]},
            "logging": {"fallback": "42"},
            "joingate": "not an object",
            "lockdown": {"main_roles": ["9"]}
        }));
        assert_eq!(c.antinuke.limits[&NukeAction::Ban].per_minute, 3);
        assert_eq!(c.antinuke.quarantine_role, Some(Id::new(7)));
        assert!(c.automod.enabled && c.automod.mention_lock == Some((5, 10_000)));
        assert_eq!(c.logging.fallback, Some(Id::new(42)));
        assert_eq!(c.lockdown.main_roles, vec![Id::new(9)]);
        assert!(!GuildCfg::parse(&json!({})).automod.enabled);
    }
}
