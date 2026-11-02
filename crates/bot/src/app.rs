//! Shared bot state and the per-guild config cache. The dashboard appends a `guild_config` row and
//! sends `NOTIFY config_changed <guild>`; we drop that guild's cached copy and reload lazily.
use crate::verify::VerificationCfg;
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::{postgres::PgListener, PgPool};
use std::collections::{HashMap, HashSet};
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
        AntinukeCfg, AppealsCfg, AutomodCfg, BackupsCfg, CatchCfg, LockdownCfg, MiscCfg, PanicCfg,
        TicketsCfg, WhitelistCfg,
    },
};
use tommybot_logging::{LogConfig, Logger};
use twilight_http::Client;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, UserMarker},
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
    pub tickets: TicketsCfg,
    pub catch: CatchCfg,
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

/// Strict parse of one section for writes from Discord: a bad value is refused, not defaulted.
pub fn check_section(name: &str, v: &Value) -> Result<(), String> {
    fn t<T: DeserializeOwned>(v: &Value) -> Result<(), String> {
        T::deserialize(v).map(drop).map_err(|e| e.to_string())
    }
    match name {
        "logging" => t::<LogConfig>(v),
        "antinuke" => t::<AntinukeCfg>(v),
        "automod" => t::<AutomodCfg>(v),
        "joingate" => t::<GateCfg>(v),
        "joinraid" => t::<RaidCfg>(v),
        "lockdown" => t::<LockdownCfg>(v).and(t::<LockdownPrefs>(v)),
        "panic" => t::<PanicCfg>(v),
        "backups" => t::<BackupsCfg>(v),
        "misc" => t::<MiscCfg>(v),
        "whitelist" => t::<WhitelistCfg>(v),
        "verification" => t::<VerificationCfg>(v),
        _ => Ok(()),
    }
}

impl GuildCfg {
    pub(crate) fn parse(cfg: &Value) -> Self {
        let lockdown: LockdownCfg = section(cfg, "lockdown");
        let mut verification: VerificationCfg = section(cfg, "verification");
        // Verified members get the Statics main roles unless verification names its own.
        if verification.roles.is_empty() {
            verification.roles = lockdown.main_roles.clone();
        }
        Self {
            logging: section(cfg, "logging"),
            antinuke: section(cfg, "antinuke"),
            automod: section(cfg, "automod"),
            joingate: section(cfg, "joingate"),
            joinraid: section(cfg, "joinraid"),
            lockdown,
            lockdown_prefs: section(cfg, "lockdown"),
            discord_automod: section(cfg, "discord_automod"),
            panic: section(cfg, "panic"),
            backups: section(cfg, "backups"),
            misc: section(cfg, "misc"),
            appeals: section(cfg, "appeals"),
            whitelist: section(cfg, "whitelist"),
            verification,
            tickets: section(cfg, "tickets"),
            catch: section(cfg, "catch"),
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
    /// Whether @everyone can read a log channel, and when that was checked.
    pub log_public:
        Mutex<HashMap<Id<twilight_model::id::marker::ChannelMarker>, (bool, std::time::Instant)>>,
    pub antinuke: Mutex<AntiNuke>,
    pub rate_limiter: Mutex<RateLimiter>,
    pub heat: Mutex<HashMap<(u64, u64), (HeatState, UserHistory)>>,
    pub crowd: Mutex<tommybot_automod::heat::Crowd>,
    /// Phishing and NSFW domain lists, refreshed in the background.
    pub sites: tommybot_automod::sites::Sites,
    pub raids: Mutex<HashMap<u64, RaidDetector>>,
    /// Pending image-captcha answers and their expiry (ms) per (guild, user); lost on restart, the member just clicks again.
    pub captchas: Mutex<HashMap<(u64, u64), (String, i64)>>,
    /// Invite uses per guild at the last join, to see which invite the next joiner used.
    pub invites: Mutex<HashMap<u64, HashMap<String, u64>>>,
    pub mentions: Mutex<HashMap<u64, MentionWatch>>,
    /// Mentions per (guild, user) for the mentions filter's hour cap.
    pub pings: Mutex<HashMap<(u64, u64), MentionWatch>>,
    /// Last message per channel, for the inactive channel filter.
    pub channels: Mutex<HashMap<u64, ChannelActivity>>,
    /// Name pair last punished by the post-join username check, so unrelated member updates do not repeat it.
    pub punished_names: Mutex<HashMap<(u64, u64), String>>,
    /// Last public command per (guild, member), in ms.
    pub cmd_cooldown: Mutex<HashMap<(u64, u64), i64>>,
    /// Anti-nuke heat per (guild, staff member) for panic mode's heat algorithm.
    pub nuke_heat: Mutex<HashMap<(u64, u64), HeatState>>,
    /// The open nuke case per guild and when it last grew; a long quiet gap starts a new case.
    pub nukes: Mutex<HashMap<u64, (NukeCase, Instant)>>,
    /// Anti-nuke quarantines and automod heat trips are counted apart, each with its own threshold.
    pub panic: Mutex<PanicTracker>,
    pub panic_heat: Mutex<PanicTracker>,
    pub panic_runs: Mutex<HashMap<u64, PanicRun>>,
    /// Heat panic: accounts that tripped it and when it ends (unix secs). Lost on restart, it is short.
    pub heat_raiders: Mutex<HashMap<u64, (HashSet<u64>, i64)>>,
    pub(crate) guilds: Mutex<HashMap<Id<GuildMarker>, Arc<crate::hierarchy::GuildInfo>>>,
    /// Category of each channel created or deleted since its audit entry, for anti-nuke's ticket categories.
    pub channel_parents: Mutex<HashMap<u64, u64>>,
    /// Recent reactions per (guild, member), for reaction spam.
    pub reactions: Mutex<HashMap<(u64, u64), tommybot_automod::window::Window>>,
    /// Thread -> parent channel; see [`App::channel_scope`].
    pub thread_parents: Mutex<HashMap<Id<ChannelMarker>, Id<ChannelMarker>>>,
}

/// Active panic mode for one guild. Its end is a `panic_end` job, so `panic::restore` rebuilds this at boot.
pub struct PanicRun {
    pub lock_mod_commands: bool,
}

/// Anyone may run the public commands, so each member gets one per this many ms.
const CMD_COOLDOWN_MS: i64 = 3_000;

impl App {
    /// True (and the clock restarts) when the member's last public command is old enough.
    pub fn cooled_down(&self, guild: u64, user: u64) -> bool {
        cool(
            &mut self.cmd_cooldown.lock().unwrap(),
            (guild, user),
            crate::automod::now_ms(),
        )
    }

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
            log_public: Default::default(),
            antinuke: Mutex::new(AntiNuke::new(HashMap::new())),
            rate_limiter: Default::default(),
            heat: Default::default(),
            sites: Default::default(),
            raids: Default::default(),
            captchas: Default::default(),
            invites: Default::default(),
            mentions: Default::default(),
            pings: Default::default(),
            channels: Default::default(),
            punished_names: Default::default(),
            cmd_cooldown: Default::default(),
            crowd: Default::default(),
            nuke_heat: Default::default(),
            nukes: Default::default(),
            panic: Mutex::new(PanicTracker::new(Duration::from_secs(600), 0)),
            panic_heat: Mutex::new(PanicTracker::new(Duration::from_secs(600), 0)),
            panic_runs: Default::default(),
            heat_raiders: Default::default(),
            guilds: Default::default(),
            channel_parents: Default::default(),
            thread_parents: Default::default(),
            reactions: Default::default(),
        }
    }

    /// Permit level of a member; a user who is not in the server has none.
    pub async fn level(
        &self,
        guild: Id<GuildMarker>,
        owner: Id<UserMarker>,
        user: Id<UserMarker>,
    ) -> Result<Level> {
        let roles: Vec<i64> = crate::http_err::member(&self.http, guild, user)
            .await?
            .map(|m| m.roles.iter().map(|r| r.get() as i64).collect())
            .unwrap_or_default();
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

    /// Drops a guild's cached config after a write from Discord, before the NOTIFY round trip.
    pub fn forget_cfg(&self, guild: u64) {
        self.cfgs.write().unwrap().remove(&guild);
    }

    /// Reloads configs when the dashboard saves. Reconnects by itself; a gap only delays a reload.
    pub async fn listen_config(self: Arc<Self>) -> Result<()> {
        let mut l = PgListener::connect_with(&self.pool).await?;
        l.listen("config_changed").await?;
        // A restarted listener missed what was saved while it was down.
        self.cfgs.write().unwrap().clear();
        loop {
            match l.try_recv().await? {
                Some(n) => {
                    if let Ok(g) = n.payload().parse::<u64>() {
                        self.cfgs.write().unwrap().remove(&g);
                        tokio::spawn(crate::automod::sync_discord_rule(self.clone(), g));
                    }
                }
                // The connection dropped and reconnects on the next call: changes saved meanwhile
                // were never announced, so every cached config is read again.
                None => self.cfgs.write().unwrap().clear(),
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

fn cool(last: &mut HashMap<(u64, u64), i64>, key: (u64, u64), now: i64) -> bool {
    if last.len() > 10_000 {
        last.retain(|_, t| now - *t < CMD_COOLDOWN_MS);
    }
    if last.get(&key).is_some_and(|t| now - t < CMD_COOLDOWN_MS) {
        return false;
    }
    last.insert(key, now);
    true
}

#[cfg(test)]
mod cooldown_tests {
    #[test]
    fn one_public_command_per_window() {
        let mut m = Default::default();
        assert!(super::cool(&mut m, (1, 2), 0));
        assert!(!super::cool(&mut m, (1, 2), 2_999));
        assert!(super::cool(&mut m, (1, 3), 10));
        assert!(super::cool(&mut m, (1, 2), 3_000));
    }
}
