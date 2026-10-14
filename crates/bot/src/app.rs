//! Shared bot state and the per-guild config cache. The dashboard appends a `guild_config` row and
//! sends `NOTIFY config_changed <guild>`; we drop that guild's cached copy and reload lazily.
use crate::verify::VerificationCfg;
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::{postgres::PgListener, PgPool};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tommybot_antinuke::{AntiNuke, NukeCase, PanicTracker};
use tommybot_automod::discord_api::{Api, DiscordAutomodCfg};
use tommybot_automod::{
    heat::{ChannelActivity, HeatState, UserHistory},
    join::{GateCfg, RaidCfg, RaidDetector},
    lockdown::{LockdownPrefs, MentionWatch},
};
use tommybot_core::{
    permits::{self, Level, RateLimiter},
    sections::{
        AntinukeCfg, AppealsCfg, AutomodCfg, BackupsCfg, LockdownCfg, MiscCfg, PanicCfg,
        WhitelistCfg,
    },
};
use tommybot_logging::{LogConfig, Logger};
use twilight_http::Client;
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

#[derive(Default)]
pub struct GuildCfg {
    pub logging: LogConfig,
    pub antinuke: AntinukeCfg,
    pub automod: AutomodCfg,
    pub joingate: GateCfg,
    pub joinraid: Option<RaidCfg>,
    pub lockdown: LockdownCfg,
    pub lockdown_prefs: LockdownPrefs,
    pub discord_automod: DiscordAutomodCfg,
    pub panic: PanicCfg,
    pub backups: BackupsCfg,
    pub misc: MiscCfg,
    pub appeals: AppealsCfg,
    pub whitelist: WhitelistCfg,
    pub verification: VerificationCfg,
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
            lockdown_prefs: section(cfg, "lockdown"),
            discord_automod: section(cfg, "discord_automod"),
            panic: section(cfg, "panic"),
            backups: section(cfg, "backups"),
            misc: section(cfg, "misc"),
            appeals: section(cfg, "appeals"),
            whitelist: section(cfg, "whitelist"),
            verification: section(cfg, "verification"),
        }
    }
}

pub struct App {
    pub http: Client,
    /// Own client and rate-limit bucket for backup restore and panic mode.
    pub restore_http: Client,
    pub pool: PgPool,
    pub api: Api,
    pub me: Id<UserMarker>,
    cfgs: RwLock<HashMap<u64, Arc<GuildCfg>>>,
    pub logger: Mutex<Logger>,
    pub antinuke: Mutex<AntiNuke>,
    pub rate_limiter: Mutex<RateLimiter>,
    pub heat: Mutex<HashMap<(u64, u64), (HeatState, UserHistory)>>,
    pub raids: Mutex<HashMap<u64, RaidDetector>>,
    /// Pending image-captcha answers per (guild, user); lost on restart, the member just clicks again.
    pub captchas: Mutex<HashMap<(u64, u64), String>>,
    pub mentions: Mutex<HashMap<u64, MentionWatch>>,
    /// Mentions per (guild, user) for the mentions filter's hour cap.
    pub pings: Mutex<HashMap<(u64, u64), MentionWatch>>,
    /// Last message per channel, for the inactive channel filter.
    pub channels: Mutex<HashMap<u64, ChannelActivity>>,
    /// Anti-nuke heat per (guild, staff member) for panic mode's heat algorithm.
    pub nuke_heat: Mutex<HashMap<(u64, u64), HeatState>>,
    /// The open nuke case per guild and when it last grew; a long quiet gap starts a new case.
    pub nukes: Mutex<HashMap<u64, (NukeCase, Instant)>>,
    /// Anti-nuke quarantines and automod heat trips are counted apart, each with its own threshold.
    pub panic: Mutex<PanicTracker>,
    pub panic_heat: Mutex<PanicTracker>,
    pub panic_runs: Mutex<HashMap<u64, PanicRun>>,
    pub(crate) guilds: Mutex<HashMap<Id<GuildMarker>, Arc<crate::hierarchy::GuildInfo>>>,
}

/// Active panic mode for one guild. Its end is a `panic_end` job, so `panic::restore` rebuilds this at boot.
pub struct PanicRun {
    pub lock_mod_commands: bool,
}

impl App {
    pub fn new(
        http: Client,
        restore_http: Client,
        pool: PgPool,
        api: Api,
        me: Id<UserMarker>,
    ) -> Self {
        Self {
            http,
            restore_http,
            pool,
            api,
            me,
            cfgs: Default::default(),
            logger: Mutex::new(Logger::new(me)),
            antinuke: Mutex::new(AntiNuke::new(HashMap::new())),
            rate_limiter: Default::default(),
            heat: Default::default(),
            raids: Default::default(),
            captchas: Default::default(),
            mentions: Default::default(),
            pings: Default::default(),
            channels: Default::default(),
            nuke_heat: Default::default(),
            nukes: Default::default(),
            panic: Mutex::new(PanicTracker::new(Duration::from_secs(600), 0)),
            panic_heat: Mutex::new(PanicTracker::new(Duration::from_secs(600), 0)),
            panic_runs: Default::default(),
            guilds: Default::default(),
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

    pub fn mod_commands_locked(&self, guild: u64) -> bool {
        self.panic_runs
            .lock()
            .unwrap()
            .get(&guild)
            .is_some_and(|r| r.lock_mod_commands)
    }

    /// Owner, Extra Owner or Trusted Admin: untouchable by every bot system.
    pub async fn is_immune(&self, guild: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
        let owner = self.guild_info(guild).await?.owner;
        Ok(self.level(guild, owner, user).await?.immune())
    }

    /// Exempt from one system via the permit whitelist (immune levels are exempt from all).
    pub async fn whitelisted(
        &self,
        guild: Id<GuildMarker>,
        user: Id<UserMarker>,
        flag: &str,
    ) -> Result<bool> {
        let owner = self.guild_info(guild).await?.owner;
        Ok(self.level(guild, owner, user).await?.whitelisted(flag))
    }

    /// Records a command use against the permit's limit; `false` means the actor breached it.
    pub fn command_allowed(&self, actor: Id<UserMarker>, level: &Level, action: &str) -> bool {
        permits::command_allowed(
            &mut self.rate_limiter.lock().unwrap(),
            actor.get() as i64,
            level,
            action,
            std::time::Instant::now(),
        )
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
                tokio::spawn(crate::automod::sync_discord_rule(self.clone(), g));
            }
        }
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tommybot_antinuke::Action as NukeAction;

    #[test]
    fn parses_sections_and_survives_bad_ones() {
        let c = GuildCfg::parse(&json!({
            "antinuke": {"limits": {"Ban": {"per_minute": 3, "per_hour": 10}}, "quarantine_role": "7"},
            "automod": {"enabled": true, "auto_lockdown": {"mention_count": 5}},
            "logging": {"fallback": "42"},
            "joingate": "not an object",
            "lockdown": {"main_roles": ["9"]}
        }));
        assert_eq!(c.antinuke.limit(NukeAction::Ban).unwrap().0.per_minute, 3);
        assert_eq!(c.antinuke.quarantine_role, Some(Id::new(7)));
        assert!(c.automod.enabled && c.automod.auto_lockdown.mention_count == 5);
        assert_eq!(c.logging.fallback, Some(Id::new(42)));
        assert_eq!(c.lockdown.main_roles, vec![Id::new(9)]);
        assert!(!GuildCfg::parse(&json!({})).automod.enabled);
    }
}
