//! Typed per-guild config sections shared by the bot (reads them) and the dashboard (validates writes).
use serde::Deserialize;
use std::collections::HashMap;
use tommybot_antinuke::{Action as NukeAction, Limit};
use tommybot_automod::{heat::HeatCfg, strike::StrikeCfg};
use twilight_model::id::{marker::RoleMarker, Id};

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct AntinukeCfg {
    pub limits: HashMap<NukeAction, Limit>,
    /// Quarantined members are moved to this role (none: all roles removed).
    pub quarantine_role: Option<Id<RoleMarker>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct AutomodCfg {
    pub enabled: bool,
    pub heat: HeatCfg,
    pub strike: StrikeCfg,
    /// (mentions, window ms): that many mentions in the window locks the server.
    pub mention_lock: Option<(usize, i64)>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct LockdownCfg {
    /// Roles that lose Send Messages in a server lock; empty means @everyone.
    pub main_roles: Vec<Id<RoleMarker>>,
}

/// Panic mode: trips when `executors` distinct accounts are quarantined within `window_secs`.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct PanicCfg {
    /// 0 turns panic mode off.
    pub executors: usize,
    pub window_secs: u64,
    pub duration_secs: u64,
    pub lock_mod_commands: bool,
    pub warned_roles: Vec<Id<RoleMarker>>,
    pub unlock_when_ending: bool,
}

impl Default for PanicCfg {
    fn default() -> Self {
        Self {
            executors: 0,
            window_secs: 600,
            duration_secs: 600,
            lock_mod_commands: false,
            warned_roles: vec![],
            unlock_when_ending: false,
        }
    }
}
