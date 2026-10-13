//! Typed per-guild config sections shared by the bot (reads them) and the dashboard (validates writes).
use serde::Deserialize;
use std::collections::HashMap;
use tommybot_antinuke::{Action as NukeAction, Limit};
use tommybot_automod::{heat::HeatCfg, strike::StrikeCfg};
use twilight_model::id::{
    marker::{ChannelMarker, RoleMarker, UserMarker, WebhookMarker},
    Id,
};

#[derive(Deserialize)]
#[serde(default)]
pub struct AntinukeCfg {
    /// Master switch for the dashboard overview; on unless a saved config turns it off.
    pub enabled: bool,
    pub limits: HashMap<NukeAction, Limit>,
    /// Quarantined members are moved to this role (none: all roles removed).
    pub quarantine_role: Option<Id<RoleMarker>>,
}

impl Default for AntinukeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            limits: HashMap::new(),
            quarantine_role: None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct AutomodCfg {
    pub enabled: bool,
    pub heat: HeatCfg,
    pub strike: StrikeCfg,
    /// (mentions, window ms): that many mentions in the window locks the server.
    pub mention_lock: Option<(usize, i64)>,
    pub filters: FiltersCfg,
}

/// What a tripped filter does; the heat weights stay in [`HeatCfg`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterAction {
    #[default]
    Delete,
    Warn,
    Timeout,
    Kick,
    Ban,
}

/// One automod filter. `action` None leaves the decision to the strike system.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FilterCfg {
    pub enabled: bool,
    pub action: Option<FilterAction>,
    /// Timeout or ban length; ignored by the other actions.
    pub duration_secs: Option<i64>,
}

impl Default for FilterCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            action: None,
            duration_secs: None,
        }
    }
}

/// Per-filter switches (Wick's Filters page). Filters default on, as the global weights always applied.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct FiltersCfg {
    pub regular: FilterCfg,
    pub similar: FilterCfg,
    pub emojis: FilterCfg,
    pub newlines: FilterCfg,
    pub chars_upper: FilterCfg,
    pub chars_lower: FilterCfg,
    pub zalgo: FilterCfg,
    pub suspicion: FilterCfg,
    pub inactive_channel: FilterCfg,
    pub invites: FilterCfg,
    /// Phishing and scam sites.
    pub bad_sites: FilterCfg,
    /// Fake Nitro and Steam links.
    pub scam_links: FilterCfg,
    pub nsfw_links: FilterCfg,
    /// Follow link shorteners before checking.
    pub nsfw_crawl_shorteners: bool,
    pub words: WordsCfg,
    pub links: FilterCfg,
    /// Domains for the link blacklist.
    pub blacklisted_domains: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WordsCfg {
    pub filter: FilterCfg,
    /// Matched as whole words.
    pub strict: Vec<String>,
    /// Matched anywhere in a word.
    pub wildcard: Vec<String>,
    pub premade_profane: bool,
    pub premade_vulgar: bool,
    pub premade_racist: bool,
    /// Keep the words in a native Discord AutoMod rule instead.
    pub use_discord_automod: bool,
}

/// Users, roles, channels and webhooks that automod and anti-nuke leave alone.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WhitelistCfg {
    pub users: Vec<Id<UserMarker>>,
    pub roles: Vec<Id<RoleMarker>>,
    pub channels: Vec<Id<ChannelMarker>>,
    pub categories: Vec<Id<ChannelMarker>>,
    pub webhooks: Vec<Id<WebhookMarker>>,
}

pub const MAX_APPEAL_FIELDS: usize = 6;

#[derive(Clone, Debug, Deserialize)]
pub struct AppealField {
    pub label: String,
    /// Multi-line answer.
    #[serde(default)]
    pub long: bool,
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AppealsCfg {
    pub enabled: bool,
    /// At most [`MAX_APPEAL_FIELDS`]; the bot ignores the rest.
    pub fields: Vec<AppealField>,
    /// Where new appeals are posted for staff.
    pub channel: Option<Id<ChannelMarker>>,
}

impl Default for AppealsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            fields: vec![AppealField {
                label: "Appeal Message".into(),
                long: true,
                required: true,
            }],
            channel: None,
        }
    }
}

/// Wick's Miscellaneous page.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct MiscCfg {
    pub prefix: String,
    pub use_slash: bool,
    /// Short embeds instead of full ones.
    pub compact_replies: bool,
    /// Delete the bot's own replies after this many seconds.
    pub delete_replies_secs: Option<u32>,
    pub enforce_quarantine_perms: bool,
    /// Ignore commands from members without a permit.
    pub ignore_no_permit: bool,
    pub modlogs_channel: Option<Id<ChannelMarker>>,
    pub confirm: bool,
    pub always_dm: bool,
    /// Delete the staff member's prefix command message.
    pub delete_invocation: bool,
    pub hide_staff_in_dms: bool,
    pub default_timeout_secs: u64,
    pub ban_purge_days: u8,
    pub show_reason_on_auto: bool,
    /// Delete the member's last 100 messages when automod punishes them.
    pub purge_last_100: bool,
    /// Only owners, trusted admins and permit holders count as staff.
    pub staff_only_permitted: bool,
    pub warn_base_points: i32,
    /// Apply `warn_actions` once a member's warn points reach them.
    pub auto_warn_actions: bool,
    pub warn_actions: Vec<crate::warn::Threshold>,
}

impl Default for MiscCfg {
    fn default() -> Self {
        Self {
            prefix: "w!".into(),
            use_slash: true,
            compact_replies: false,
            delete_replies_secs: None,
            enforce_quarantine_perms: false,
            ignore_no_permit: false,
            modlogs_channel: None,
            confirm: false,
            always_dm: false,
            delete_invocation: false,
            hide_staff_in_dms: false,
            default_timeout_secs: 600,
            ban_purge_days: 0,
            show_reason_on_auto: true,
            purge_last_100: false,
            staff_only_permitted: true,
            warn_base_points: 1,
            auto_warn_actions: true,
            warn_actions: vec![],
        }
    }
}

impl MiscCfg {
    /// Discord allows timeouts up to 28 days.
    pub fn timeout_secs(&self) -> u64 {
        self.default_timeout_secs.clamp(1, 28 * 86_400)
    }

    /// Discord allows purging up to 7 days of messages on a ban.
    pub fn purge_secs(&self) -> u32 {
        u32::from(self.ban_purge_days.min(7)) * 86_400
    }
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
    /// Also trip on automod heat, not only on anti-nuke quarantines.
    pub heat_algorithm: bool,
    /// Distinct accounts that must hit the heat limit within `window_secs` to trip it.
    pub heat_executors: usize,
    /// Categories panic mode leaves unlocked.
    pub whitelisted_categories: Vec<Id<ChannelMarker>>,
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
            heat_algorithm: false,
            heat_executors: 3,
            whitelisted_categories: vec![],
        }
    }
}

/// Server backups: the 3-hour auto backup can be switched off per guild.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct BackupsCfg {
    pub auto: bool,
}

impl Default for BackupsCfg {
    fn default() -> Self {
        Self { auto: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn antinuke_is_enabled_unless_saved_off() {
        assert!(AntinukeCfg::default().enabled);
        let on: AntinukeCfg = serde_json::from_value(json!({"quarantine_role": "7"})).unwrap();
        assert!(on.enabled);
        let off: AntinukeCfg = serde_json::from_value(json!({"enabled": false})).unwrap();
        assert!(!off.enabled);
    }

    #[test]
    fn backups_are_automatic_unless_saved_off() {
        assert!(BackupsCfg::default().auto);
        let off: BackupsCfg = serde_json::from_value(json!({"auto": false})).unwrap();
        assert!(!off.auto);
    }

    #[test]
    fn misc_defaults_fill_missing_fields_and_clamp() {
        let m: MiscCfg = serde_json::from_value(
            json!({"default_timeout_secs": 99_999_999, "ban_purge_days": 30}),
        )
        .unwrap();
        assert_eq!(m.prefix, "w!");
        assert!(m.use_slash && m.auto_warn_actions);
        assert_eq!(m.timeout_secs(), 28 * 86_400);
        assert_eq!(m.purge_secs(), 7 * 86_400);
    }

    #[test]
    fn filters_default_on_and_accept_an_action() {
        let a: AutomodCfg = serde_json::from_value(
            json!({"filters": {"zalgo": {"enabled": false}, "invites": {"action": "timeout", "duration_secs": 600}}}),
        )
        .unwrap();
        assert!(a.filters.regular.enabled && !a.filters.zalgo.enabled);
        assert_eq!(a.filters.invites.action, Some(FilterAction::Timeout));
    }

    #[test]
    fn appeals_start_with_one_message_field() {
        let a = AppealsCfg::default();
        assert_eq!(a.fields.len(), 1);
        assert!(a.fields.len() <= MAX_APPEAL_FIELDS);
        let w: WhitelistCfg =
            serde_json::from_value(json!({"users": [1], "webhooks": [2]})).unwrap();
        assert_eq!(w.users.len() + w.webhooks.len(), 2);
    }
}
