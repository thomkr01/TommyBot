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
    /// Per action; actions left out use [`NukeAction::default_limit`].
    pub limits: HashMap<NukeAction, LimitCfg>,
    /// Quarantined members are moved to this role (none: all roles removed).
    pub quarantine_role: Option<Id<RoleMarker>>,
    /// Watch member prunes.
    pub prune: bool,
    /// Quarantine whoever changes the vanity URL (bots cannot set it back).
    pub vanity_guard: bool,
    /// Off: a reached limit is only logged and reverted.
    pub auto_quarantine: bool,
    /// Punish giving dangerous permissions to any role.
    pub strict_mode: bool,
    /// Punish giving members a role with dangerous permissions.
    pub strict_member_roles: bool,
    /// Keep dangerous permissions off @everyone and the main roles.
    pub monitor_public_roles: bool,
    /// Keep dangerous channel overwrites off @everyone and the main roles.
    pub monitor_channel_perms: bool,
    /// May change a quarantined member's roles.
    pub quarantine_whitelist: Vec<Id<UserMarker>>,
}

impl Default for AntinukeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            limits: HashMap::new(),
            quarantine_role: None,
            prune: false,
            vanity_guard: false,
            auto_quarantine: true,
            strict_mode: false,
            strict_member_roles: false,
            monitor_public_roles: false,
            monitor_channel_perms: false,
            quarantine_whitelist: vec![],
        }
    }
}

/// One anti-nuke filter card; unset numbers take the action's defaults.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct LimitCfg {
    pub enabled: bool,
    pub per_minute: Option<u32>,
    pub per_hour: Option<u32>,
    /// Heat % per action towards panic mode's heat algorithm.
    pub heat: Option<u32>,
}

impl Default for LimitCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            per_minute: None,
            per_hour: None,
            heat: None,
        }
    }
}

impl AntinukeCfg {
    /// The watched limit and heat % of `a`, None when it is not watched.
    pub fn limit(&self, a: NukeAction) -> Option<(Limit, u32)> {
        if a == NukeAction::Prune && !self.prune {
            return None;
        }
        let c = self.limits.get(&a).copied().unwrap_or_default();
        let d = a.default_limit();
        let limit = Limit {
            per_minute: c.per_minute.or(d.map(|d| d.0))?,
            per_hour: c.per_hour.or(d.map(|d| d.1))?,
        };
        let heat = c.heat.or(d.map(|d| d.2)).unwrap_or(0);
        c.enabled.then_some((limit, heat))
    }

    pub fn watched(&self) -> HashMap<NukeAction, Limit> {
        NukeAction::ALL
            .into_iter()
            .filter_map(|a| self.limit(a).map(|(l, _)| (a, l)))
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(default)]
pub struct AutomodCfg {
    pub enabled: bool,
    /// Off: only the link and word filters run.
    pub anti_spam: bool,
    /// Off: messages from webhooks are left alone.
    pub monitor_webhooks: bool,
    pub heat: HeatCfg,
    pub strike: StrikeCfg,
    pub auto_lockdown: AutoLockdownCfg,
    pub filters: FiltersCfg,
}

impl Default for AutomodCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            anti_spam: true,
            monitor_webhooks: true,
            heat: HeatCfg::default(),
            strike: StrikeCfg::default(),
            auto_lockdown: AutoLockdownCfg::default(),
            filters: FiltersCfg::default(),
        }
    }
}

/// Locks the server on mention spam, a strike count or (with `joinraid.lock`) a join raid.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AutoLockdownCfg {
    pub enabled: bool,
    pub mentions: bool,
    /// That many mentions in the window lock the server.
    pub mention_count: usize,
    pub mention_window_ms: i64,
    /// Lock when a member reaches this strike number.
    pub strikes: Option<u32>,
}

impl Default for AutoLockdownCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            mentions: true,
            mention_count: 50,
            mention_window_ms: 3_000,
            strikes: None,
        }
    }
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
#[derive(Clone, Debug, Deserialize)]
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
    /// Messages after a quiet spell that count as the inactive-channel burst.
    pub inactive_messages: u32,
    /// Hours without a message before a channel counts as inactive.
    pub inactive_hours: u32,
    /// Mentions tab: `mentions` holds the action, the rest only switch their heat.
    pub mentions: FilterCfg,
    pub mention_users: FilterCfg,
    pub mention_roles: FilterCfg,
    pub mention_everyone: FilterCfg,
    /// Mentions a member may send per hour before the mentions filter trips.
    pub mention_hour_cap: usize,
    /// Attachments tab, same split as mentions.
    pub attachments: FilterCfg,
    pub embeds: FilterCfg,
    pub images: FilterCfg,
    pub files: FilterCfg,
    pub attach_links: FilterCfg,
    pub stickers: FilterCfg,
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

impl Default for FiltersCfg {
    fn default() -> Self {
        let on = FilterCfg::default;
        Self {
            regular: on(),
            similar: on(),
            emojis: on(),
            newlines: on(),
            chars_upper: on(),
            chars_lower: on(),
            zalgo: on(),
            suspicion: on(),
            inactive_channel: on(),
            inactive_messages: 10,
            inactive_hours: 1,
            mentions: on(),
            mention_users: on(),
            mention_roles: on(),
            mention_everyone: on(),
            mention_hour_cap: 20,
            attachments: on(),
            embeds: on(),
            images: on(),
            files: on(),
            attach_links: on(),
            stickers: on(),
            invites: on(),
            bad_sites: on(),
            scam_links: on(),
            nsfw_links: on(),
            nsfw_crawl_shorteners: false,
            words: WordsCfg::default(),
            links: on(),
            blacklisted_domains: vec![],
        }
    }
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

/// Automod whitelists, one per tab. The top-level lists are the Spam tab, which skips automod entirely.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WhitelistCfg {
    #[serde(flatten)]
    pub spam: WhitelistList,
    pub invites: WhitelistList,
    /// @user and @role mentions.
    pub mentions: WhitelistList,
    /// @everyone and @here.
    pub everyone: WhitelistList,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WhitelistList {
    pub users: Vec<Id<UserMarker>>,
    pub roles: Vec<Id<RoleMarker>>,
    pub channels: Vec<Id<ChannelMarker>>,
    pub categories: Vec<Id<ChannelMarker>>,
    pub webhooks: Vec<Id<WebhookMarker>>,
}

impl WhitelistList {
    /// A message whose author, member roles, channel, category or webhook is listed.
    pub fn covers(
        &self,
        user: Id<UserMarker>,
        roles: &[Id<RoleMarker>],
        channel: Id<ChannelMarker>,
        category: Option<Id<ChannelMarker>>,
        webhook: Option<Id<WebhookMarker>>,
    ) -> bool {
        self.users.contains(&user)
            || roles.iter().any(|r| self.roles.contains(r))
            || self.channels.contains(&channel)
            || category.is_some_and(|c| self.categories.contains(&c))
            || webhook.is_some_and(|w| self.webhooks.contains(&w))
    }
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
    /// Fraction of a staff member's anti-nuke heat lost per second (0.01 = 1%/s).
    pub heat_decay: f64,
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
            heat_decay: 0.01,
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
    }

    #[test]
    fn whitelist_tabs_keep_the_old_lists_as_spam() {
        let w: WhitelistCfg = serde_json::from_value(
            json!({"users": ["1"], "webhooks": ["2"], "invites": {"channels": ["3"]}}),
        )
        .unwrap();
        let (u, c) = (Id::new(1), Id::new(3));
        assert!(w.spam.covers(u, &[], Id::new(9), None, None));
        assert!(w.invites.covers(Id::new(5), &[], c, None, None));
        assert!(!w.mentions.covers(u, &[], c, None, Some(Id::new(2))));
        assert!(w.spam.covers(Id::new(5), &[], c, None, Some(Id::new(2))));
    }

    #[test]
    fn antinuke_limits_fall_back_to_wick_defaults() {
        let c: AntinukeCfg = serde_json::from_value(json!({"limits": {
            "Ban": {"per_minute": 2}, "RoleCreate": {"enabled": false}, "BotAdd": {"per_minute": 1, "per_hour": 2}
        }}))
        .unwrap();
        let ban = c.limit(NukeAction::Ban).unwrap();
        assert_eq!((ban.0.per_minute, ban.0.per_hour, ban.1), (2, 15, 20));
        assert!(c.limit(NukeAction::RoleCreate).is_none());
        assert!(
            c.limit(NukeAction::Prune).is_none(),
            "prunes need the toggle"
        );
        assert_eq!(c.watched().len(), 8);
    }
}
