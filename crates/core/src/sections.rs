//! Typed per-guild config sections shared by the bot (reads them) and the dashboard (validates writes).
use serde::Deserialize;
use std::collections::HashMap;
use tommybot_antinuke::{Action as NukeAction, Limit};
use tommybot_automod::{
    heat::{AutoSlowmodeCfg, HeatCfg},
    strike::StrikeCfg,
};
use twilight_model::id::{
    marker::{ChannelMarker, RoleMarker, UserMarker, WebhookMarker},
    Id,
};

#[derive(Deserialize)]
#[serde(default)]
pub struct AntinukeCfg {
    /// Master switch; off until a saved config turns it on.
    pub enabled: bool,
    /// Per action; actions left out use [`NukeAction::default_limit`]. An action this version
    /// does not know is skipped, not the whole section (D48).
    #[serde(deserialize_with = "tommybot_logging::lenient::map")]
    pub limits: HashMap<NukeAction, LimitCfg>,
    /// Quarantined members are moved to this role (none: all roles removed).
    pub quarantine_role: Option<Id<RoleMarker>>,
    /// Watch member prunes.
    pub prune: bool,
    /// Punish anyone who touches a quarantined member (roles, nickname, timeout, kick, ban).
    pub quarantine_hold: bool,
    /// Quarantine whoever changes the vanity URL and put the old code back.
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
    /// Channels created or deleted in these categories never count (ticket bots).
    pub ticket_categories: Vec<Id<ChannelMarker>>,
}

impl Default for AntinukeCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            limits: HashMap::new(),
            quarantine_role: None,
            prune: false,
            quarantine_hold: true,
            vanity_guard: false,
            auto_quarantine: true,
            strict_mode: false,
            strict_member_roles: false,
            monitor_public_roles: false,
            monitor_channel_perms: false,
            quarantine_whitelist: vec![],
            ticket_categories: vec![],
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

impl LimitCfg {
    /// Minute cap, hour cap and heat % of card `a`, unset ones taking the defaults. Values saved
    /// before the limits existed are pulled inside them, and a saved heat of 0 means the default.
    pub fn values(&self, a: NukeAction) -> Option<(u32, u32, u32)> {
        use crate::limits::{NUKE_HEAT, NUKE_HOUR, NUKE_MINUTE};
        let fit = |v: Option<u32>, (lo, hi): (i64, i64)| v.map(|v| v.clamp(lo as u32, hi as u32));
        let d = a.default_limit();
        Some((
            fit(self.per_minute, NUKE_MINUTE).or(d.map(|d| d.0))?,
            fit(self.per_hour, NUKE_HOUR).or(d.map(|d| d.1))?,
            fit(self.heat.filter(|h| *h > 0), NUKE_HEAT)
                .or(d.map(|d| d.2))
                .unwrap_or(0),
        ))
    }
}

impl AntinukeCfg {
    /// The watched limit and heat % of `a`, None when it is not watched.
    pub fn limit(&self, a: NukeAction) -> Option<(Limit, u32)> {
        if (a == NukeAction::Prune && !self.prune)
            || (a.opt_in() && !self.limits.contains_key(&a.card()))
        {
            return None;
        }
        let c = self.limits.get(&a.card()).copied().unwrap_or_default();
        let (per_minute, per_hour, heat) = c.values(a)?;
        c.enabled.then_some((
            Limit {
                per_minute,
                per_hour,
            },
            heat,
        ))
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
    #[serde(deserialize_with = "heat_cfg")]
    pub heat: HeatCfg,
    pub strike: StrikeCfg,
    pub auto_lockdown: AutoLockdownCfg,
    #[serde(deserialize_with = "slowmode_cfg")]
    pub auto_slowmode: AutoSlowmodeCfg,
    pub filters: FiltersCfg,
}

/// Max heat and decay inside their limits, so heat never turns NaN or stops cooling.
fn heat_cfg<'de, D: serde::Deserializer<'de>>(d: D) -> Result<HeatCfg, D::Error> {
    HeatCfg::deserialize(d).map(HeatCfg::clamped)
}

/// Auto slowmode inside its limits. A value of the wrong type (say -1) gives this card its defaults,
/// not the whole automod section.
fn slowmode_cfg<'de, D: serde::Deserializer<'de>>(d: D) -> Result<AutoSlowmodeCfg, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value::<AutoSlowmodeCfg>(v)
        .unwrap_or_default()
        .clamped())
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
            auto_slowmode: AutoSlowmodeCfg::default(),
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

/// Per-filter switches (Filters page). Defaults in `FILTERS_OFF_BY_DEFAULT`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FiltersCfg {
    pub regular: FilterCfg,
    pub similar: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub emojis: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub newlines: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub chars_upper: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub chars_lower: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub zalgo: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub suspicion: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
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
    #[serde(deserialize_with = "off_filter")]
    pub invites: FilterCfg,
    /// Phishing and scam sites.
    #[serde(deserialize_with = "off_filter")]
    pub bad_sites: FilterCfg,
    /// Fake Nitro and Steam links.
    #[serde(deserialize_with = "off_filter")]
    pub scam_links: FilterCfg,
    #[serde(deserialize_with = "off_filter")]
    pub nsfw_links: FilterCfg,
    /// Images checked by the self-hosted NSFW model (NSFW card).
    #[serde(deserialize_with = "off_filter")]
    pub nsfw_images: FilterCfg,
    /// Score in percent from which an image counts as NSFW (`limits::NSFW_THRESHOLD`).
    pub nsfw_threshold: u32,
    /// Follow link shorteners before checking.
    pub nsfw_crawl_shorteners: bool,
    pub words: WordsCfg,
    #[serde(deserialize_with = "off_filter")]
    pub links: FilterCfg,
    /// Domains for the link blacklist.
    pub blacklisted_domains: Vec<String>,
}

impl Default for FiltersCfg {
    fn default() -> Self {
        let on = FilterCfg::default;
        let off = || FilterCfg {
            enabled: false,
            ..FilterCfg::default()
        };
        Self {
            regular: on(),
            similar: on(),
            emojis: off(),
            newlines: off(),
            chars_upper: off(),
            chars_lower: off(),
            zalgo: off(),
            suspicion: off(),
            inactive_channel: off(),
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
            invites: off(),
            bad_sites: off(),
            scam_links: off(),
            nsfw_links: off(),
            nsfw_images: off(),
            nsfw_threshold: 80,
            nsfw_crawl_shorteners: false,
            words: WordsCfg::default(),
            links: off(),
            blacklisted_domains: vec![],
        }
    }
}

/// A filter that starts off: a stored object without `enabled` (say only an action) stays off.
fn off_filter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<FilterCfg, D::Error> {
    let mut v = serde_json::Value::deserialize(d)?;
    if let Some(o) = v.as_object_mut() {
        o.entry("enabled").or_insert(false.into());
    }
    serde_json::from_value(v).map_err(serde::de::Error::custom)
}

/// The filters `FiltersCfg::default` starts off (config keys, `words.filter` for the word blacklist),
/// for the dashboard's defaults. A test keeps the two in step.
pub const FILTERS_OFF_BY_DEFAULT: &[&str] = &[
    "emojis",
    "newlines",
    "chars_upper",
    "chars_lower",
    "zalgo",
    "suspicion",
    "inactive_channel",
    "invites",
    "bad_sites",
    "scam_links",
    "nsfw_links",
    "nsfw_images",
    "words.filter",
    "links",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WordsCfg {
    #[serde(deserialize_with = "off_filter")]
    pub filter: FilterCfg,
    /// Matched as whole words.
    pub strict: Vec<String>,
    /// Matched anywhere in a word.
    pub wildcard: Vec<String>,
    /// Regular expressions, case-insensitive, matched anywhere in the message.
    pub regex: Patterns,
    pub premade_profane: bool,
    pub premade_vulgar: bool,
    pub premade_racist: bool,
    /// Keep the words in a native Discord AutoMod rule instead.
    pub use_discord_automod: bool,
}

impl Default for WordsCfg {
    fn default() -> Self {
        Self {
            // Word blacklist starts off and warns.
            filter: FilterCfg {
                enabled: false,
                action: Some(FilterAction::Warn),
                ..FilterCfg::default()
            },
            strict: vec![],
            wildcard: vec![],
            regex: Patterns::default(),
            premade_profane: false,
            premade_vulgar: false,
            premade_racist: false,
            use_discord_automod: false,
        }
    }
}

/// Regex word patterns, compiled once when the config loads, so a bad one fails the dashboard save.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(try_from = "Vec<String>")]
pub struct Patterns {
    list: Vec<String>,
    set: regex::RegexSet,
}

impl TryFrom<Vec<String>> for Patterns {
    type Error = regex::Error;

    fn try_from(list: Vec<String>) -> Result<Self, Self::Error> {
        let set = regex::RegexSetBuilder::new(&list)
            .case_insensitive(true)
            // Admin-supplied patterns: cap the compiled size (matching is linear time already).
            .size_limit(1 << 20)
            .build()?;
        Ok(Self { list, set })
    }
}

impl Patterns {
    /// Number of patterns that match `text`.
    pub fn hits(&self, text: &str) -> usize {
        self.set.matches(text).iter().count()
    }

    pub fn as_slice(&self) -> &[String] {
        &self.list
    }
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
    /// Partnering channels static: spam, @everyone and invites are allowed there.
    pub partner_channels: Vec<Id<ChannelMarker>>,
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

impl WhitelistCfg {
    /// Nothing is whitelisted from the automod (spam, invites, mentions, @everyone, partner channels).
    pub fn is_empty(&self) -> bool {
        [&self.spam, &self.invites, &self.mentions, &self.everyone]
            .iter()
            .all(|l| l.is_empty())
            && self.partner_channels.is_empty()
    }
}

impl WhitelistList {
    fn is_empty(&self) -> bool {
        self.users.is_empty()
            && self.roles.is_empty()
            && self.channels.is_empty()
            && self.categories.is_empty()
            && self.webhooks.is_empty()
    }

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

/// Sections the dashboard saves only after a TOTP step-up, so prefix panels may not edit them.
pub const SENSITIVE: &[&str] = &[
    "antinuke",
    "whitelist",
    "panic",
    "backups",
    "catch",
    "ban_sync",
    "raider_list",
];

#[derive(Clone, Debug, Deserialize)]
pub struct AppealField {
    pub label: String,
    /// Multi-line answer.
    #[serde(default)]
    pub long: bool,
    #[serde(default = "yes")]
    pub required: bool,
    /// Non-empty makes it a choice question: single choice, or multi choice with `multi`.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub multi: bool,
    /// Text length limits; the maximum defaults to 1000.
    #[serde(default)]
    pub min: Option<usize>,
    #[serde(default)]
    pub max: Option<usize>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TicketsCfg {
    /// Members can DM the bot to open a ticket with this server.
    pub enabled: bool,
    /// Where ticket channels are created.
    pub category: Option<Id<ChannelMarker>>,
    /// Gets "New Ticket" and "Ticket Closed" logs.
    pub log_channel: Option<Id<ChannelMarker>>,
    /// The close log carries the whole conversation as a file.
    pub full_log: bool,
    /// Staff besides permits with Manage Tickets.
    pub access_roles: Vec<Id<RoleMarker>>,
    /// Pinged in a new ticket; the @everyone role (the guild id) pings everyone.
    pub ping_roles: Vec<Id<RoleMarker>>,
    pub ping_here: bool,
    /// Sent to the member when a ticket opens / closes; `{username}` `{userid}` `{usermention}`.
    pub greeting: String,
    pub closing: String,
    /// Staff replies hide their name by default.
    pub anonymous: bool,
    /// Only `reply` reaches the member; other messages stay staff talk.
    pub command_only: bool,
    /// No new tickets; the reason is shown to members ("" = none given).
    pub paused: bool,
    pub pause_reason: String,
    /// Cannot open tickets or get replies.
    pub blocked: Vec<Id<UserMarker>>,
    pub snippets: Vec<Snippet>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Snippet {
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub author: Option<Id<UserMarker>>,
}

impl Default for TicketsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            category: None,
            log_channel: None,
            full_log: true,
            access_roles: vec![],
            ping_roles: vec![],
            ping_here: false,
            greeting: String::new(),
            closing: String::new(),
            anonymous: false,
            command_only: false,
            paused: false,
            pause_reason: String::new(),
            blocked: vec![],
            snippets: vec![],
        }
    }
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
                options: vec![],
                multi: false,
                min: None,
                max: None,
            }],
            channel: None,
        }
    }
}

/// Miscellaneous page.
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
    /// Take dangerous-permission roles from members who have no permit.
    pub enforce_permits: bool,
    /// Shown instead of the default appeal line in ban and timeout DMs.
    pub ban_appeal_message: String,
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
    /// Take the main roles away while a member is timed out.
    pub remove_main_roles_on_timeout: bool,
    /// Give a member's roles back when they rejoin (safe roles only, see `bot::sticky`).
    pub sticky_roles: bool,
    /// Roles sticky roles never gives back.
    pub sticky_never: Vec<Id<RoleMarker>>,
    /// Strip hoisting symbols from the front of names on join and on every name change.
    pub auto_dehoist: bool,
}

impl Default for MiscCfg {
    fn default() -> Self {
        Self {
            prefix: "t!".into(),
            use_slash: true,
            compact_replies: false,
            delete_replies_secs: None,
            enforce_quarantine_perms: true,
            ignore_no_permit: false,
            enforce_permits: false,
            ban_appeal_message: String::new(),
            confirm: true,
            always_dm: true,
            delete_invocation: false,
            hide_staff_in_dms: false,
            default_timeout_secs: 0,
            ban_purge_days: 7,
            show_reason_on_auto: true,
            purge_last_100: false,
            staff_only_permitted: false,
            warn_base_points: 5,
            auto_warn_actions: true,
            warn_actions: vec![],
            remove_main_roles_on_timeout: true,
            sticky_roles: false,
            sticky_never: vec![],
            auto_dehoist: false,
        }
    }
}

impl MiscCfg {
    /// Discord allows timeouts up to 28 days; 0 means the maximum ("permanent").
    pub fn timeout_secs(&self) -> u64 {
        match self.default_timeout_secs {
            0 => 28 * 86_400,
            n => n.min(28 * 86_400),
        }
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

/// The whole `verification` section as the bot reads it; the dashboard validates saves with it too.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct VerificationCfg {
    pub enabled: bool,
    /// Where `verifysetup` posts the button.
    pub channel: Option<Id<ChannelMarker>>,
    /// Roles handed out on a pass.
    pub roles: Vec<Id<RoleMarker>>,
    #[serde(flatten)]
    pub verify: tommybot_automod::join::VerifyCfg,
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
    pub lock_on_trigger: bool,
}

impl Default for PanicCfg {
    fn default() -> Self {
        Self {
            executors: 0,
            window_secs: 600,
            duration_secs: 60,
            lock_mod_commands: true,
            warned_roles: vec![],
            unlock_when_ending: true,
            heat_algorithm: false,
            heat_executors: 3,
            heat_decay: 0.01,
            whitelisted_categories: vec![],
            lock_on_trigger: true,
        }
    }
}

pub use tommybot_antinuke::{verification_shape, QUARANTINE_DENY, VERIFY_ALLOW, VERIFY_DENY};

/// Server backups: off until a guild turns on `enabled`; then the 3-hour auto backup runs. The rest are the
/// restore filters (`anr`), used by anti-nuke reverts and panic mode, never by `/backup load`.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct BackupsCfg {
    /// Allows taking backups at all, manual and automatic.
    pub enabled: bool,
    /// Backups kept per guild; older ones are dropped on save.
    pub max_images: i64,
    /// Hours between automatic backups.
    pub interval_hours: u64,
    pub auto: bool,
    pub restore: bool,
    pub restore_roles: bool,
    pub delete_roles: bool,
    pub restore_channels: bool,
    pub delete_channels: bool,
    pub delete_webhooks: bool,
}

impl Default for BackupsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_images: 10,
            interval_hours: 3,
            auto: true,
            restore: true,
            restore_roles: true,
            delete_roles: true,
            restore_channels: true,
            delete_channels: true,
            delete_webhooks: true,
        }
    }
}

/// What the catch channel does to a writer; `Log` is the test mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum CatchAction {
    #[default]
    Ban,
    Softban,
    Kick,
    Timeout,
    Log,
}

/// Honeypot channel: hacked accounts spam every channel, so anyone writing here is punished.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct CatchCfg {
    pub enabled: bool,
    pub channel: Option<Id<ChannelMarker>>,
    pub action: CatchAction,
    /// DM the member first (with the appeal link for bans and timeouts).
    pub dm: bool,
    /// Move the channel back to the top when it is dragged down (set by the dashboard's setup).
    pub keep_top: bool,
    /// Never punished, besides bots, permit holders and owners.
    pub roles: Vec<Id<RoleMarker>>,
    pub users: Vec<Id<UserMarker>>,
}

/// Ban Sync: copies bans between servers that have the same Discord owner. Off until the owner turns it on.
#[derive(Deserialize)]
#[serde(default)]
pub struct BanSyncCfg {
    pub enabled: bool,
    /// Send this server's bans to the other servers in the group.
    pub share: bool,
    /// Which automatic bans are sent too (staff bans always are while `share` is on).
    pub share_joinraid: bool,
    pub share_automod: bool,
    pub share_joingate: bool,
    pub share_verify: bool,
    pub receive: BanSyncReceive,
    /// Lift the copies when the source ban is lifted.
    pub sync_unbans: bool,
}

impl Default for BanSyncCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            share: true,
            share_joinraid: true,
            share_automod: true,
            share_joingate: false,
            share_verify: false,
            receive: BanSyncReceive::Alert,
            sync_unbans: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BanSyncReceive {
    Off,
    #[default]
    Alert,
    Ban,
}

/// Raider List: acts on confirmed raider accounts when they join. Off until saved on.
#[derive(Deserialize)]
#[serde(default)]
pub struct RaiderListCfg {
    pub enabled: bool,
    pub action: RaiderAction,
    #[serde(deserialize_with = "tommybot_logging::lenient::list")]
    pub categories: Vec<RaiderCategory>,
    /// What to do for listed users already in the server: only alert, or nothing.
    pub scan_members: bool,
    /// Unban here when the bot owner removes someone from the list.
    pub lift_on_removal: bool,
    /// Do not send this server's detections to the list.
    pub no_collect: bool,
}

impl Default for RaiderListCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            action: RaiderAction::Ban,
            categories: RaiderCategory::ALL.to_vec(),
            scan_members: true,
            lift_on_removal: true,
            no_collect: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RaiderAction {
    Alert,
    Quarantine,
    Kick,
    #[default]
    Ban,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RaiderCategory {
    Nuke,
    Raid,
    Scam,
    Spam,
}

impl RaiderCategory {
    pub const ALL: [RaiderCategory; 4] = [Self::Nuke, Self::Raid, Self::Scam, Self::Spam];
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn raider_list_is_off_until_saved_on() {
        let c: RaiderListCfg = serde_json::from_value(json!({})).unwrap();
        assert!(!c.enabled && c.scan_members && c.lift_on_removal && !c.no_collect);
        assert_eq!(c.action, RaiderAction::Ban);
        assert_eq!(c.categories, RaiderCategory::ALL);
    }

    #[test]
    fn ban_sync_is_off_until_saved_on() {
        let c: BanSyncCfg = serde_json::from_value(json!({})).unwrap();
        assert!(!c.enabled && c.share && c.sync_unbans);
        assert_eq!(c.receive, BanSyncReceive::Alert);
        assert!(c.share_joinraid && c.share_automod && !c.share_joingate && !c.share_verify);
    }

    #[test]
    fn regex_words_match_and_bad_ones_are_refused() {
        let w: WordsCfg =
            serde_json::from_value(json!({"regex": ["fr[e3]+ n[i1]tro", "a{1,3}b"]})).unwrap();
        assert_eq!(w.regex.hits("FREE NITRO here, aab"), 2);
        assert_eq!(w.regex.hits("hello"), 0);
        assert_eq!(WordsCfg::default().regex.hits("anything"), 0);
        assert!(serde_json::from_value::<WordsCfg>(json!({"regex": ["(unclosed"]})).is_err());
    }

    #[test]
    fn saved_caps_outside_the_limits_are_pulled_inside() {
        let c: AntinukeCfg = serde_json::from_value(
            json!({"limits": {"Ban": {"per_minute": 99, "per_hour": 1000, "heat": 0}}}),
        )
        .unwrap();
        let (l, heat) = c.limit(NukeAction::Ban).unwrap();
        assert_eq!((l.per_minute, l.per_hour, heat), (50, 500, 20));
        let c: AntinukeCfg =
            serde_json::from_value(json!({"limits": {"Ban": {"per_hour": 1, "heat": 101}}}))
                .unwrap();
        let (l, heat) = c.limit(NukeAction::Kick).unwrap();
        assert_eq!((l.per_minute, l.per_hour, heat), (5, 2, 100));
        // Prune is no card: its built-in 1/1 stays.
        let c: AntinukeCfg = serde_json::from_value(json!({"prune": true})).unwrap();
        assert_eq!(c.limit(NukeAction::Prune).unwrap().0.per_minute, 1);
    }

    #[test]
    fn catch_is_off_and_bans_by_default() {
        let c: CatchCfg = serde_json::from_value(json!({})).unwrap();
        assert!(!c.enabled && c.channel.is_none() && c.action == CatchAction::Ban);
        let c: CatchCfg = serde_json::from_value(json!({"action": "Log"})).unwrap();
        assert_eq!(c.action, CatchAction::Log);
    }

    #[test]
    fn auto_slowmode_is_clamped_and_a_bad_value_keeps_the_section() {
        let am = |v: serde_json::Value| -> AutomodCfg {
            serde_json::from_value(json!({"enabled": true, "auto_slowmode": v})).unwrap()
        };
        let c = am(
            json!({"enabled": true, "channel_pct": 0, "slowmode_secs": 0,
            "min_on_secs": u32::MAX, "calm_secs": 0}),
        )
        .auto_slowmode;
        assert!(c.enabled);
        assert_eq!(
            (c.channel_pct, c.slowmode_secs, c.min_on_secs, c.calm_secs),
            (200, 1, 3_600, 10)
        );
        let c = am(json!({"enabled": true, "slowmode_secs": 99_999})).auto_slowmode;
        assert_eq!((c.slowmode_secs, c.channel_pct), (21_600, 300));
        // -1 does not fit a u32: the card falls back to its defaults, automod stays on.
        let bad = am(json!({"enabled": true, "calm_secs": -1}));
        assert!(bad.enabled && !bad.auto_slowmode.enabled);
        assert_eq!(bad.auto_slowmode.calm_secs, 120);
        let none: AutomodCfg = serde_json::from_value(json!({})).unwrap();
        assert!(!none.auto_slowmode.enabled);
        assert_eq!(none.auto_slowmode.slowmode_secs, 5);
    }

    #[test]
    fn heat_is_clamped_when_read() {
        let am: AutomodCfg =
            serde_json::from_value(json!({"heat": {"max": 0, "decay_per_sec": 4, "base": 3}}))
                .unwrap();
        assert_eq!(
            (am.heat.max, am.heat.decay_per_sec, am.heat.base),
            (1.0, 0.99, 3.0)
        );
        let am: AutomodCfg = serde_json::from_value(json!({})).unwrap();
        assert_eq!((am.heat.max, am.heat.decay_per_sec), (100.0, 0.08));
    }

    #[test]
    fn panic_lasts_a_minute_by_default() {
        assert_eq!(PanicCfg::default().duration_secs, 60);
        let p: PanicCfg = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p.duration_secs, 60);
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres: the stored old default 600
    /// moves to 60, other durations stay.
    #[tokio::test]
    async fn migration_moves_only_the_old_panic_default() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        let g = 7_000_000_000 + std::process::id() as i64 * 10;
        let put = |gid: i64, secs: i64| {
            sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, 1, $2)")
                .bind(gid)
                .bind(json!({"panic": {"duration_secs": secs}, "misc": {"prefix": "t!"}}))
                .execute(&pool)
        };
        put(g, 600).await.unwrap();
        put(g + 1, 300).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/2200_panic_duration_60s.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let read = |gid: i64| {
            sqlx::query_scalar::<_, Value>("SELECT config FROM guild_config WHERE guild_id = $1")
                .bind(gid)
                .fetch_one(&pool)
        };
        let moved = read(g).await.unwrap();
        assert_eq!(moved["panic"]["duration_secs"], 60);
        assert_eq!(moved["misc"]["prefix"], "t!");
        assert_eq!(read(g + 1).await.unwrap()["panic"]["duration_secs"], 300);
        sqlx::query("DELETE FROM guild_config WHERE guild_id IN ($1, $2)")
            .bind(g)
            .bind(g + 1)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn antinuke_is_off_until_saved_on() {
        assert!(!AntinukeCfg::default().enabled);
        let off: AntinukeCfg = serde_json::from_value(json!({"quarantine_role": "7"})).unwrap();
        assert!(!off.enabled);
        let on: AntinukeCfg = serde_json::from_value(json!({"enabled": true})).unwrap();
        assert!(on.enabled);
    }

    #[test]
    fn off_filters_stay_off_until_enabled() {
        let get = |f: &FiltersCfg, k: &str| match k {
            "emojis" => f.emojis.enabled,
            "newlines" => f.newlines.enabled,
            "chars_upper" => f.chars_upper.enabled,
            "chars_lower" => f.chars_lower.enabled,
            "zalgo" => f.zalgo.enabled,
            "suspicion" => f.suspicion.enabled,
            "inactive_channel" => f.inactive_channel.enabled,
            "invites" => f.invites.enabled,
            "bad_sites" => f.bad_sites.enabled,
            "scam_links" => f.scam_links.enabled,
            "nsfw_links" => f.nsfw_links.enabled,
            "nsfw_images" => f.nsfw_images.enabled,
            "words.filter" => f.words.filter.enabled,
            "links" => f.links.enabled,
            _ => unreachable!("{k}"),
        };
        for k in FILTERS_OFF_BY_DEFAULT {
            let at = |v: Value| match k.split_once('.') {
                Some((a, b)) => json!({a: {b: v}}),
                None => json!({*k: v}),
            };
            assert!(!get(&FiltersCfg::default(), k), "{k}");
            let partial: FiltersCfg = serde_json::from_value(at(json!({"action": "ban"}))).unwrap();
            assert!(!get(&partial, k), "{k}");
            let on: FiltersCfg = serde_json::from_value(at(json!({"enabled": true}))).unwrap();
            assert!(get(&on, k), "{k}");
        }
        let d = FiltersCfg::default();
        assert!(
            d.regular.enabled && d.similar.enabled && d.mentions.enabled && d.attachments.enabled
        );
    }

    #[test]
    fn backups_are_off_until_saved_on() {
        let d = BackupsCfg::default();
        assert!(!d.enabled && d.auto);
        let on: BackupsCfg = serde_json::from_value(json!({"enabled": true})).unwrap();
        assert!(on.enabled && on.auto);
    }

    #[test]
    fn misc_defaults_fill_missing_fields_and_clamp() {
        let m: MiscCfg = serde_json::from_value(
            json!({"default_timeout_secs": 99_999_999, "ban_purge_days": 30}),
        )
        .unwrap();
        assert_eq!(m.prefix, "t!");
        assert!(m.use_slash && m.auto_warn_actions);
        assert_eq!(m.timeout_secs(), 28 * 86_400);
        let zero: MiscCfg = serde_json::from_value(json!({"default_timeout_secs": 0})).unwrap();
        assert_eq!(zero.timeout_secs(), 28 * 86_400);
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
    fn antinuke_limits_fall_back_to_defaults() {
        let c: AntinukeCfg = serde_json::from_value(json!({"limits": {
            "Ban": {"per_minute": 2}, "RoleCreate": {"enabled": false}, "BotAdd": {"per_minute": 1, "per_hour": 2},
            "Timeout": {"enabled": true}
        }}))
        .unwrap();
        let ban = c.limit(NukeAction::Ban).unwrap();
        assert_eq!((ban.0.per_minute, ban.0.per_hour, ban.1), (2, 15, 20));
        assert!(c.limit(NukeAction::RoleCreate).is_none());
        assert!(
            c.limit(NukeAction::Prune).is_none(),
            "prunes need the toggle"
        );
        // Opt-in actions are watched only once configured, then with their defaults.
        assert_eq!(c.limit(NukeAction::Timeout).unwrap().0.per_minute, 5);
        assert!(c.limit(NukeAction::Unban).is_none());
        // App additions are watched by default; the other new cards are opt-in.
        assert_eq!(
            c.limit(NukeAction::IntegrationCreate).unwrap().0.per_minute,
            2
        );
        assert_eq!(c.watched().len(), 10);
        for a in [
            NukeAction::IntegrationDelete,
            NukeAction::EmojiCreate,
            NukeAction::StickerCreate,
            NukeAction::SoundboardDelete,
            NukeAction::ScheduledEventCreate,
            NukeAction::GuildUpdate,
            NukeAction::VoiceDisconnect,
            NukeAction::VoiceMove,
        ] {
            assert!(c.limit(a).is_none(), "{a:?}");
            let on: AntinukeCfg =
                serde_json::from_value(json!({"limits": {format!("{a:?}"): {}}})).unwrap();
            assert!(on.limit(a).is_some(), "{a:?}");
        }
    }
}
