//! The number maps, one row per line. Never renumber a row: people type these numbers.
use tommybot_antinuke::Action as NukeAction;
use tommybot_automod::join::{Action, GateFilter, RaidCfg};
use tommybot_logging::{embed::{self, NO}, LogKind};
use tommybot_core::limits::{NUKE_HEAT, NUKE_HOUR, NUKE_MINUTE, NSFW_THRESHOLD, SLOW_CALM, SLOW_ON, SLOW_PCT, SLOW_SECS};
use tommybot_core::warn::Action as WarnAction;
use twilight_model::id::{marker::RoleMarker, Id};

use super::{Kind, Module, Row};
use crate::app::GuildCfg;

macro_rules! r {
    ($code:expr, $name:literal, $path:expr, $kind:expr, |$c:ident| $get:expr) => {
        Row { code: $code, name: $name, path: $path, kind: $kind, get: |$c: &GuildCfg| -> String { $get } }
    };
}

macro_rules! group {
    ($code:literal, $name:literal) => {
        Row { code: $code, name: $name, path: "", kind: Kind::Group, get: |_| String::new() }
    };
}

macro_rules! dash {
    ($code:literal, $name:literal) => {
        Row { code: $code, name: $name, path: "", kind: Kind::Dashboard, get: |_| String::new() }
    };
}

/// One heat filter card: status, action and heat per unit.
macro_rules! filter {
    ($n:literal, $name:literal, $fp:literal, $($f:ident).+; $heat:ident, $unit:literal) => {
        [
            group!($n, $name),
            r!(concat!($n, "a"), "Status", concat!("automod.filters.", $fp, ".enabled"), Kind::Bool, |c| on(c.automod.filters.$($f).+.enabled)),
            r!(concat!($n, "b"), "Action", concat!("automod.filters.", $fp, ".action"), Kind::Choice(FILTER_ACTIONS), |c| opt(c.automod.filters.$($f).+.action)),
            r!(concat!($n, "c"), "Heat", concat!("automod.heat.", stringify!($heat)), Kind::Num(1.0), |c| format!("{} {}", pct(c.automod.heat.$heat), $unit)),
        ]
    };
}

/// One anti-nuke cap card: status, minute cap, hour cap and heat, unset numbers showing the defaults.
macro_rules! cap {
    ($n:literal, $name:literal, $a:ident) => {
        [
            group!($n, $name),
            r!(concat!($n, "a"), "Status", concat!("antinuke.limits.", stringify!($a), ".enabled"), Kind::Bool, |c| on(cap(c, NukeAction::$a).0)),
            r!(concat!($n, "b"), "Minute Cap", concat!("antinuke.limits.", stringify!($a), ".per_minute"), Kind::Int(NUKE_MINUTE.0, NUKE_MINUTE.1), |c| cap(c, NukeAction::$a).1.to_string()),
            r!(concat!($n, "c"), "Hour Cap", concat!("antinuke.limits.", stringify!($a), ".per_hour"), Kind::Int(NUKE_HOUR.0, NUKE_HOUR.1), |c| cap(c, NukeAction::$a).2.to_string()),
            r!(concat!($n, "d"), "Heat %", concat!("antinuke.limits.", stringify!($a), ".heat"), Kind::Int(NUKE_HEAT.0, NUKE_HEAT.1), |c| format!("{}%", cap(c, NukeAction::$a).3)),
        ]
    };
}

/// (status, minute cap, hour cap, heat %) of an anti-nuke card.
fn cap(c: &GuildCfg, a: NukeAction) -> (bool, u32, u32, u32) {
    let l = c.antinuke.limits.get(&a).copied().unwrap_or_default();
    let (m, h, heat) = l.values(a).unwrap_or_default();
    (l.enabled, m, h, heat)
}

/// One Join Gate card: status and action.
macro_rules! gate_filter {
    ($n:literal, $name:literal, $f:ident) => {
        gate_filter!($n, $name, $f, ACTIONS)
    };
    ($n:literal, $name:literal, $f:ident, $actions:ident) => {
        [
            group!($n, $name),
            r!(concat!($n, "a"), "Status", concat!("joingate.", stringify!($f), ".enabled"), Kind::Bool, |c| on(c.joingate.$f.enabled)),
            r!(concat!($n, "b"), "Action", concat!("joingate.", stringify!($f), ".action"), Kind::Choice($actions), |c| gate(c.joingate.$f)),
        ]
    };
}

const FILTER_ACTIONS: &[(&str, &str)] = &[("delete", "\"delete\""), ("warn", "\"warn\""), ("timeout", "\"timeout\""), ("kick", "\"kick\""), ("ban", "\"ban\"")];
const ACTIONS: &[(&str, &str)] = &[("log", "\"Log\""), ("warn", "\"Warn\""), ("timeout", "\"Timeout\""), ("kick", "\"Kick\""), ("ban", "\"Ban\"")];
/// Only a name can be fixed by renaming, so only the username filter offers Rename.
const NAME_ACTIONS: &[(&str, &str)] = &[("log", "\"Log\""), ("rename", "\"Rename\""), ("warn", "\"Warn\""), ("timeout", "\"Timeout\""), ("kick", "\"Kick\""), ("ban", "\"Ban\"")];

fn on(b: bool) -> String {
    if b { "Enabled" } else { "Disabled" }.into()
}

fn ms(v: i64) -> String {
    embed::duration(v / 1000)
}

fn pct(v: f64) -> String {
    format!("{}%", (v * 100.0).round() / 100.0)
}

fn opt<T: std::fmt::Debug>(v: Option<T>) -> String {
    v.map_or("Default".into(), |v| format!("{v:?}"))
}

fn act(a: Action) -> String {
    match a {
        Action::Timeout(_) => "Timeout".into(),
        a => format!("{a:?}"),
    }
}

fn gate(f: GateFilter) -> String {
    f.action.map_or("Default".into(), act)
}

fn mentions(ids: impl IntoIterator<Item = u64>, pfx: &str) -> String {
    let s: Vec<_> = ids.into_iter().map(|i| format!("<{pfx}{i}>")).collect();
    if s.is_empty() { NO.into() } else { s.join(" ") }
}

fn roles(ids: &[Id<RoleMarker>]) -> String {
    mentions(ids.iter().map(|i| i.get()), "@&")
}

fn chans<T>(ids: &[Id<T>]) -> String {
    mentions(ids.iter().map(|i| i.get()), "#")
}

fn raid(c: &GuildCfg) -> RaidCfg {
    c.joinraid.clone().unwrap_or_default()
}

pub(super) static STATICS: Module = Module {
    names: &["statics", "s"],
    title: "Statics Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!("2", "Quarantine Role", "antinuke.quarantine_role", Kind::Role, |c| mentions(c.antinuke.quarantine_role.map(|r| r.get()), "@&")),
        r!("5", "Main Roles", "lockdown.main_roles", Kind::Roles, |c| roles(&c.lockdown.main_roles)),
        r!("6", "Logging Channel", "logging.fallback", Kind::Channel, |c| mentions(c.logging.fallback.map(|r| r.get()), "#")),
        r!("7", "Mod-Logging Channel", "logging.routes.mod_command", Kind::Channel, |c| mentions(c.logging.routes.get(&LogKind::ModCommand).map(|r| r.get()), "#")),
        r!("8", "Partnering Channels", "whitelist.partner_channels", Kind::Channels, |c| chans(&c.whitelist.partner_channels)),
        Row { code: "10", name: "Trusted Admins", path: "", kind: Kind::Users, get: |_| String::new() },
        dash!("11", "Extra Owners"),
        r!("12", "Verification Channel", "verification.channel", Kind::Channel, |c| mentions(c.verification.channel.map(|r| r.get()), "#")),
    ],
};

pub(super) static HEAT: Module = Module {
    names: &["heat"],
    title: "Heat System Panel:",
    per_page: 0,
    footer: "Advanced Settings: Heat System Filters: t!hfilters",
    rows: &[
        r!("1", "Status", "automod.enabled", Kind::Bool, |c| on(c.automod.enabled)),
        r!("2", "Spam Filters", "automod.anti_spam", Kind::Bool, |c| on(c.automod.anti_spam)),
        r!("3", "Max Heat Percentage", "automod.heat.max", Kind::Num(1.0), |c| pct(c.automod.heat.max)),
        r!("4", "Heat Degradation", "automod.heat.decay_per_sec", Kind::Num(0.01), |c| format!("{} per second", pct(c.automod.heat.decay_per_sec * 100.0))),
        r!("5", "Strikes CAP", "automod.strike.cap", Kind::Int(1, 100), |c| c.automod.strike.cap.to_string()),
        r!("6", "Webhook Coverage", "automod.monitor_webhooks", Kind::Bool, |c| on(c.automod.monitor_webhooks)),
        group!("7", "Auto Timeouts"),
        r!("7a", "Status", "automod.strike.enabled", Kind::Bool, |c| on(c.automod.strike.enabled)),
        r!("7b", "Regular Strike Duration", "automod.strike.strike_timeout_ms", Kind::Ms, |c| ms(c.automod.strike.strike_timeout_ms)),
        r!("7c", "CAP Strike Duration", "automod.strike.cap_timeout_ms", Kind::Ms, |c| ms(c.automod.strike.cap_timeout_ms)),
        r!("7d", "Multiplier Status", "automod.strike.multiply", Kind::Bool, |c| on(c.automod.strike.multiply)),
        r!("7e", "Multiplier Percentage", "automod.strike.multiplier_pct", Kind::Int(100, 1000), |c| format!("{}%", c.automod.strike.multiplier_pct)),
        r!("8", "Reset Heat On Timeout", "automod.strike.reset_heat_on_timeout", Kind::Bool, |c| on(c.automod.strike.reset_heat_on_timeout)),
        group!("9", "Heat Panic Mode"),
        r!("9a", "Status", "panic.heat_algorithm", Kind::Bool, |c| on(c.panic.heat_algorithm)),
        r!("9b", "Trigger", "panic.heat_executors", Kind::Int(1, 50), |c| format!("{} Raiders", c.panic.heat_executors)),
        r!("9c", "Panic Duration", "panic.duration_secs", Kind::Secs, |c| embed::duration(c.panic.duration_secs as i64)),
        group!("10", "Auto Server Lockdown"),
        r!("10a", "Status", "automod.auto_lockdown.enabled", Kind::Bool, |c| on(c.automod.auto_lockdown.enabled)),
        r!("10b", "Mentions", "automod.auto_lockdown.mention_count", Kind::Int(1, 1000), |c| c.automod.auto_lockdown.mention_count.to_string()),
        r!("10c", "Under", "automod.auto_lockdown.mention_window_ms", Kind::Ms, |c| ms(c.automod.auto_lockdown.mention_window_ms)),
        group!("11", "Auto Slowmode"),
        r!("11a", "Status", "automod.auto_slowmode.enabled", Kind::Bool, |c| on(c.automod.auto_slowmode.enabled)),
        r!("11b", "Trigger", "automod.auto_slowmode.channel_pct", Kind::Int(SLOW_PCT.0, SLOW_PCT.1), |c| format!("{}% of max heat", c.automod.auto_slowmode.channel_pct)),
        r!("11c", "Slowmode", "automod.auto_slowmode.slowmode_secs", Kind::Int(SLOW_SECS.0, SLOW_SECS.1), |c| embed::duration(c.automod.auto_slowmode.slowmode_secs.into())),
        r!("11d", "Minimum Time On", "automod.auto_slowmode.min_on_secs", Kind::Int(SLOW_ON.0, SLOW_ON.1), |c| embed::duration(c.automod.auto_slowmode.min_on_secs.into())),
        r!("11e", "Calm Time", "automod.auto_slowmode.calm_secs", Kind::Int(SLOW_CALM.0, SLOW_CALM.1), |c| embed::duration(c.automod.auto_slowmode.calm_secs.into())),
    ],
};

/// Panel shows filters 6-9 in this order; the rest of the order is ours.
static HFILTERS_ROWS: [Row; 61] = {
    let [a1, a2, a3, a4] = filter!("1", "Normal Message Heat", "regular", regular; base, "per message");
    let [b1, b2, b3, b4] = filter!("2", "Similar Message Repetition Heat", "similar", similar; similarity, "at full similarity");
    let [c1, c2, c3, c4] = filter!("3", "Mentions Heat", "mentions", mentions; per_mention, "per mention");
    let [d1, d2, d3, d4] = filter!("4", "Attachments Heat", "attachments", attachments; per_image, "per image");
    let [e1, e2, e3, e4] = filter!("5", "Advertisement Heat", "invites", invites; invite, "per invite");
    let [f1, f2, f3, f4] = filter!("6", "Malicious Websites Heat", "bad_sites", bad_sites; malicious, "per message");
    let f5 = r!("6d", "Anti-Scam", "automod.filters.scam_links.enabled", Kind::Bool, |c| on(c.automod.filters.scam_links.enabled));
    let [g1, g2, g3, g4] = filter!("7", "Emojis Heat", "emojis", emojis; per_emoji, "per emoji");
    // The panel has one Message Characters filter; status and action follow the uppercase one, the lowercase one keeps rows 10a-10c.
    let [_, h2, h3, _] = filter!("8", "Uppercase Characters Heat", "chars_upper", chars_upper; per_upper, "per uppercased character");
    let h1 = group!("8", "Message Characters Heat");
    let h4 = r!("8c", "LC Heat", "automod.heat.per_lower", Kind::Num(1.0), |c| format!("{} per lowercased character", pct(c.automod.heat.per_lower)));
    let h5 = r!("8d", "UC Heat", "automod.heat.per_upper", Kind::Num(1.0), |c| format!("{} per uppercased character", pct(c.automod.heat.per_upper)));
    let [i1, i2, i3, i4] = filter!("9", "New Line Heat", "newlines", newlines; per_newline, "per newline");
    let [j1, j2, j3, j4] = filter!("10", "Lowercase Characters Heat", "chars_lower", chars_lower; per_lower, "per lowercased character");
    let [k1, k2, k3, k4] = filter!("11", "Inactive Channel Heat", "inactive_channel", inactive_channel; inactive_channel, "per message");
    let [l1, l2, l3, l4] = filter!("12", "NSFW Websites Heat", "nsfw_links", nsfw_links; nsfw, "per message");
    let l5 = r!("12d", "NSFW Images", "automod.filters.nsfw_images.enabled", Kind::Bool, |c| on(c.automod.filters.nsfw_images.enabled));
    let l6 = r!("12e", "NSFW Images Action", "automod.filters.nsfw_images.action", Kind::Choice(FILTER_ACTIONS), |c| opt(c.automod.filters.nsfw_images.action));
    let l7 = r!("12f", "NSFW Image Score", "automod.filters.nsfw_threshold", Kind::Int(NSFW_THRESHOLD.0, NSFW_THRESHOLD.1), |c| format!("{}%", c.automod.filters.nsfw_threshold));
    let [m1, m2, m3, m4] = filter!("13", "Words Blacklist Heat", "words.filter", words.filter; per_blacklisted_word, "per word");
    let [n1, n2, n3, n4] = filter!("14", "Suspicious Account Heat", "suspicion", suspicion; suspicious_account, "per message");
    [
        a1, a2, a3, a4, b1, b2, b3, b4, c1, c2, c3, c4, d1, d2, d3, d4, e1, e2, e3, e4, f1, f2, f3, f4, f5,
        g1, g2, g3, g4, h1, h2, h3, h4, h5, i1, i2, i3, i4, j1, j2, j3, j4, k1, k2, k3, k4, l1, l2, l3, l4,
        l5, l6, l7,
        m1, m2, m3, m4, n1, n2, n3, n4,
    ]
};

pub(super) static HFILTERS: Module = Module {
    names: &["hfilters"],
    title: "Heat System Filters Panel:",
    per_page: 4,
    footer: "",
    rows: &HFILTERS_ROWS,
};

pub(super) static ANTINUKE: Module = Module {
    names: &["antinuke", "an"],
    title: "Anti Nuke Panel:",
    per_page: 0,
    footer: "Panic Mode: t!anp · Restore Filters: t!anr",
    rows: &[
        r!("1", "Status", "antinuke.enabled", Kind::Bool, |c| on(c.antinuke.enabled)),
        dash!("2", "Limits"),
        r!("3", "Quarantine On Limit", "antinuke.auto_quarantine", Kind::Bool, |c| on(c.antinuke.auto_quarantine)),
        group!("4", "Prune"),
        r!("4a", "Prune Detection", "antinuke.prune", Kind::Bool, |c| on(c.antinuke.prune)),
        group!("5", "Quarantine Hold"),
        r!("5a", "Quarantine Hold", "antinuke.quarantine_hold", Kind::Bool, |c| on(c.antinuke.quarantine_hold)),
        r!("5b", "Strict Mode", "antinuke.strict_mode", Kind::Bool, |c| on(c.antinuke.strict_mode)),
        r!("5c", "Monitor Public Roles", "antinuke.monitor_public_roles", Kind::Bool, |c| on(c.antinuke.monitor_public_roles)),
        r!("5d", "Vanity Protection", "antinuke.vanity_guard", Kind::Bool, |c| on(c.antinuke.vanity_guard)),
        r!("5e", "Strict Member Role Addition", "antinuke.strict_member_roles", Kind::Bool, |c| on(c.antinuke.strict_member_roles)),
        r!("5f", "Monitor Channel Permissions", "antinuke.monitor_channel_perms", Kind::Bool, |c| on(c.antinuke.monitor_channel_perms)),
        r!("6", "Ticket Categories", "antinuke.ticket_categories", Kind::Channels, |c| chans(&c.antinuke.ticket_categories)),
    ],
};

static CAPS_ROWS: [Row; 35] = {
    let [a1, a2, a3, a4, a5] = cap!("1", "Kicks & Bans", Ban);
    let [b1, b2, b3, b4, b5] = cap!("2", "Role Creations", RoleCreate);
    let [c1, c2, c3, c4, c5] = cap!("3", "Role Deletions", RoleDelete);
    let [d1, d2, d3, d4, d5] = cap!("4", "Channel Creations", ChannelCreate);
    let [e1, e2, e3, e4, e5] = cap!("5", "Channel Deletions", ChannelDelete);
    let [f1, f2, f3, f4, f5] = cap!("6", "Webhook Creations", WebhookCreate);
    let [g1, g2, g3, g4, g5] = cap!("7", "Webhook Deletions", WebhookDelete);
    [
        a1, a2, a3, a4, a5, b1, b2, b3, b4, b5, c1, c2, c3, c4, c5, d1, d2, d3, d4, d5, e1, e2, e3, e4, e5, f1, f2, f3, f4, f5, g1, g2, g3,
        g4, g5,
    ]
};

pub(super) static CAPS: Module = Module {
    names: &["ancaps"],
    title: "Anti-Nuke Caps:",
    per_page: 0,
    footer: "Change one: `1 ?off a` or `6 ?set 1b`. More caps on the dashboard.",
    rows: &CAPS_ROWS,
};

pub(super) static PANIC: Module = Module {
    names: &["anpanic", "anp"],
    title: "Panic Mode Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Status", "panic.executors", Kind::Switch("3", "0"), |c| on(c.panic.executors > 0)),
        r!("2", "Heat Algorithm", "panic.heat_algorithm", Kind::Bool, |c| on(c.panic.heat_algorithm)),
        r!("3", "Lockdown Server On Trigger", "panic.lock_on_trigger", Kind::Bool, |c| on(c.panic.lock_on_trigger)),
        r!("4", "Unlock Server When Ending", "panic.unlock_when_ending", Kind::Bool, |c| on(c.panic.unlock_when_ending)),
        r!("5", "Warned Roles", "panic.warned_roles", Kind::Roles, |c| roles(&c.panic.warned_roles)),
        r!("6", "Whitelisted Categories", "panic.whitelisted_categories", Kind::Channels, |c| chans(&c.panic.whitelisted_categories)),
        r!("7", "Lock Mod Cmds On Trigger", "panic.lock_mod_commands", Kind::Bool, |c| on(c.panic.lock_mod_commands)),
        r!("8", "Trigger", "panic.executors", Kind::Int(1, 50), |c| format!("{} quarantined staff", c.panic.executors)),
        r!("9", "Panic Duration", "panic.duration_secs", Kind::Secs, |c| embed::duration(c.panic.duration_secs as i64)),
    ],
};

pub(super) static RESTORE: Module = Module {
    names: &["antinukerestore", "anr"],
    title: "Restore System Filters:",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Status", "backups.restore", Kind::Bool, |c| on(c.backups.restore)),
        r!("2", "Restoring Deleted Roles", "backups.restore_roles", Kind::Bool, |c| on(c.backups.restore_roles)),
        r!("3", "Deleting Created Roles", "backups.delete_roles", Kind::Bool, |c| on(c.backups.delete_roles)),
        r!("4", "Restoring Deleted Channels", "backups.restore_channels", Kind::Bool, |c| on(c.backups.restore_channels)),
        r!("5", "Deleting Created Channels", "backups.delete_channels", Kind::Bool, |c| on(c.backups.delete_channels)),
        r!("6", "Deleting Created Webhooks", "backups.delete_webhooks", Kind::Bool, |c| on(c.backups.delete_webhooks)),
    ],
};

static JOINGATE_ROWS: [Row; 30] = {
    let [a1, a2, a3] = gate_filter!("2", "No PfP Filter", no_avatar);
    let [b1, b2, b3] = gate_filter!("3", "Account Age Filter", account_age);
    let [c1, c2, c3] = gate_filter!("4", "Bot Addition Filter", unauthorized_bot);
    let [d1, d2, d3] = gate_filter!("5", "Advertising Names", invite_in_name);
    let [e1, e2, e3] = gate_filter!("6", "Unverified Bots", unverified_bot);
    let [f1, f2, f3] = gate_filter!("7", "Suspicious Accounts", suspicious);
    let [g1, g2, g3] = gate_filter!("8", "Username Filter", username_filter, NAME_ACTIONS);
    let [h1, h2, h3] = gate_filter!("9", "Spam Flag Filter", spammer);
    [
        group!("1", "General"),
        r!("1a", "Status", "joingate.enabled", Kind::Bool, |c| on(c.joingate.enabled)),
        r!("1b", "DM Members", "joingate.dm_punished", Kind::Bool, |c| on(c.joingate.dm_punished)),
        r!("1c", "Exclude Owner Bots", "joingate.exclude_trusted_bots", Kind::Bool, |c| on(c.joingate.exclude_trusted_bots)),
        a1, a2, a3, b1, b2, b3,
        r!("3c", "Minimum Age", "joingate.min_age_override_ms", Kind::Age, |c| ms(c.joingate.min_age_ms())),
        r!("3d", "Expose Minimum Age", "joingate.expose_min_age", Kind::Bool, |c| on(c.joingate.expose_min_age)),
        c1, c2, c3, d1, d2, d3, e1, e2, e3, f1, f2, f3, g1, g2, g3, h1, h2, h3,
    ]
};

pub(super) static JOINGATE: Module = Module {
    names: &["joingate", "jg"],
    title: "JoinGate Panel:",
    per_page: 0,
    footer: "Username words: Dashboard",
    rows: &JOINGATE_ROWS,
};

pub(super) static JOINRAID: Module = Module {
    names: &["joinraid", "jr"],
    title: "Join Raid",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Status", "joinraid.enabled", Kind::Bool, |c| on(raid(c).enabled)),
        r!("2", "Action", "joinraid.action", Kind::Choice(ACTIONS), |c| act(raid(c).action)),
        r!("3", "Warned Roles", "joinraid.warned_roles", Kind::Roles, |c| mentions(raid(c).warned_roles.iter().filter_map(|r| r.parse().ok()), "@&")),
        group!("4", "Static Flags"),
        r!("4a", "Minimum Trigger", "joinraid.count", Kind::Int(2, 500), |c| format!("{} accounts.", raid(c).count)),
        r!("4b", "Join History", "joinraid.span_ms", Kind::Ms, |c| format!("Past {}", ms(raid(c).span_ms))),
        r!("4c", "Trigger Duration", "joinraid.active_ms", Kind::Ms, |c| ms(raid(c).active_ms)),
        r!("4d", "Accounts Type", "joinraid.suspicious_only", Kind::Choice(&[("all", "false"), ("suspicious", "true")]), |c| if raid(c).suspicious_only { "Suspicious" } else { "All" }.into()),
        dash!("5", "String Flag"),
        group!("6", "Age Flag"),
        r!("6a", "Status", "joinraid.flag_younger_than_ms", Kind::Switch("86400000", "null"), |c| on(raid(c).flag_younger_than_ms.is_some())),
        r!("6b", "Minimum Age", "joinraid.flag_younger_than_ms", Kind::Ms, |c| raid(c).flag_younger_than_ms.map_or(NO.into(), ms)),
        r!("7", "NoPfP Flag", "joinraid.flag_no_avatar", Kind::Bool, |c| on(raid(c).flag_no_avatar)),
        group!("8", "ID Flag"),
        r!("8a", "Status", "joinraid.id_flag_min", Kind::Switch("4", "null"), |c| on(raid(c).id_flag_min.is_some())),
        r!("8b", "Granularity", "joinraid.id_granularity", Kind::Choice(&[("adapt", "\"Adaptive\""), ("h", "\"Hour\""), ("d", "\"Day\""), ("m", "\"Month\"")]), |c| format!("{:?}", raid(c).id_granularity)),
        r!("8c", "Margin", "joinraid.id_margin_ms", Kind::Ms, |c| ms(raid(c).id_margin_ms)),
        r!("8d", "Minimum Matches", "joinraid.id_flag_min", Kind::Int(2, 500), |c| raid(c).id_flag_min.map_or(NO.into(), |n| n.to_string())),
        r!("9", "Spammer Flag", "joinraid.flag_spammer", Kind::Bool, |c| on(raid(c).flag_spammer)),
    ],
};

pub(super) static VERIFICATION: Module = Module {
    names: &["verification", "v"],
    title: "Verification Panel:",
    per_page: 0,
    footer: "Captcha Customization: Dashboard",
    rows: &[
        r!("1", "Status", "verification.enabled", Kind::Bool, |c| on(c.verification.enabled)),
        r!("2", "Action", "verification.on_fail", Kind::Choice(&[("quarantine", "\"Quarantine\""), ("kick", "\"Kick\""), ("ban", "\"Ban\""), ("none", "\"Nothing\"")]), |c| format!("{:?}", c.verification.verify.on_fail)),
        r!("3", "Target", "verification.target", Kind::Choice(&[("all", "\"Everyone\""), ("suspicious", "\"SuspiciousOnly\"")]), |c| format!("{:?}", c.verification.verify.target)),
        r!("4", "Verified Roles", "verification.roles", Kind::Roles, |c| roles(&c.verification.roles)),
        r!("5", "Duration", "verification.time_limit_ms", Kind::Ms, |c| ms(c.verification.verify.time_limit_ms)),
        r!("6", "Mode", "verification.mode", Kind::Choice(&[("captcha", "\"Captcha\""), ("rucaptcha", "\"SimpleCaptcha\""), ("none", "\"Button\""), ("web", "\"Web\""), ("dm", "\"Dm\"")]), |c| format!("{:?}", c.verification.verify.mode)),
        r!("12", "Regeneration", "verification.regen_ms", Kind::Ms, |c| c.verification.verify.regen_ms.map_or(NO.into(), ms)),
        r!("12a", "Regeneration Status", "verification.regen_ms", Kind::Switch("12000", "null"), |c| on(c.verification.verify.regen_ms.is_some())),
        r!("13", "Notes", "verification.show_notes", Kind::Bool, |c| on(c.verification.verify.show_notes)),
        r!("14", "Audit Log Reason", "verification.audit_reason", Kind::Bool, |c| on(c.verification.verify.audit_reason)),
        r!("15", "Block VPN", "verification.block_vpn", Kind::Bool, |c| on(c.verification.verify.block_vpn)),
        r!("16", "Block Hosting Networks", "verification.block_hosting", Kind::Bool, |c| on(c.verification.verify.block_hosting)),
    ],
};

pub(super) static MISC: Module = Module {
    names: &["misc"],
    title: "Miscellaneous Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Prefix", "misc.prefix", Kind::Text, |c| c.misc.prefix.clone()),
        group!("2", "Moderation"),
        r!("2a", "Confirm Actions", "misc.confirm", Kind::Bool, |c| on(c.misc.confirm)),
        r!("2b", "Days Purged On Bans", "misc.ban_purge_days", Kind::Int(0, 7), |c| c.misc.ban_purge_days.to_string()),
        r!("2c", "Default Timeout Duration", "misc.default_timeout_secs", Kind::Secs, |c| embed::duration(c.misc.timeout_secs() as i64)),
        r!("2d", "Anonymous Mod", "misc.hide_staff_in_dms", Kind::Bool, |c| on(c.misc.hide_staff_in_dms)),
        r!("2e", "DM Targets", "misc.always_dm", Kind::Bool, |c| on(c.misc.always_dm)),
        r!("2f", "Display Reason On Auto Timeout", "misc.show_reason_on_auto", Kind::Bool, |c| on(c.misc.show_reason_on_auto)),
        r!("2g", "Purge On Auto Timeout", "misc.purge_last_100", Kind::Bool, |c| on(c.misc.purge_last_100)),
        r!("2h", "Auto Delete Mod Cmd", "misc.delete_invocation", Kind::Bool, |c| on(c.misc.delete_invocation)),
        r!("2i", "Remove Main Roles On TO", "misc.remove_main_roles_on_timeout", Kind::Bool, |c| on(c.misc.remove_main_roles_on_timeout)),
        group!("3", "Warn Settings"),
        r!("3a", "Status", "misc.auto_warn_actions", Kind::Bool, |c| on(c.misc.auto_warn_actions)),
        // The panel has one warn action; ours is a list, so the prefix edits its first entry.
        r!("3b", "Warn Action", "misc.warn_actions.0.action", Kind::Choice(&[("timeout", "\"timeout\""), ("kick", "\"kick\""), ("ban", "\"ban\"")]), |c| c.misc.warn_actions.first().map_or(NO.into(), |t| match t.action { WarnAction::Timeout { .. } => "Timeout".into(), a => format!("{a:?}") })),
        r!("3c", "Max Warns", "misc.warn_actions.0.points", Kind::Int(1, 1000), |c| c.misc.warn_actions.first().map_or(NO.into(), |t| t.points.to_string())),
        r!("4", "Compact Outputs", "misc.compact_replies", Kind::Bool, |c| on(c.misc.compact_replies)),
        r!("5", "Informing Messages", "misc.delete_replies_secs", Kind::Switch("5", "null"), |c| on(c.misc.delete_replies_secs.is_some())),
        r!("6", "Enforce Quarantine Perms", "misc.enforce_quarantine_perms", Kind::Bool, |c| on(c.misc.enforce_quarantine_perms)),
        r!("7", "Ignore Permit 1 Commands", "misc.ignore_no_permit", Kind::Bool, |c| on(c.misc.ignore_no_permit)),
        r!("8", "Enforce Permits", "misc.enforce_permits", Kind::Bool, |c| on(c.misc.enforce_permits)),
        r!("9", "Ban Appeal Message", "misc.ban_appeal_message", Kind::Text, |c| Some(c.misc.ban_appeal_message.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| NO.into())),
        r!("10", "Sticky Roles", "misc.sticky_roles", Kind::Bool, |c| on(c.misc.sticky_roles)),
        r!("11", "Never Sticky Roles", "misc.sticky_never", Kind::Roles, |c| roles(&c.misc.sticky_never)),
        r!("12", "Auto Dehoist", "misc.auto_dehoist", Kind::Bool, |c| on(c.misc.auto_dehoist)),
    ],
};

/// `t!setup` walks through these; the roles and channels themselves are made by the dashboard wizard.
pub(super) static SETUP: Module = Module {
    names: &["setup"],
    title: "Setup Panel:",
    per_page: 0,
    footer: "Quarantine role and log channels: t!setup run · Everything else: Dashboard wizard",
    rows: &[
        r!("1", "Anti Nuke", "antinuke.enabled", Kind::Bool, |c| on(c.antinuke.enabled)),
        r!("2", "Auto Mod", "automod.enabled", Kind::Bool, |c| on(c.automod.enabled)),
        r!("3", "Join Gate", "joingate.enabled", Kind::Bool, |c| on(c.joingate.enabled)),
        r!("4", "Join Raid", "joinraid.enabled", Kind::Bool, |c| on(raid(c).enabled)),
        r!("5", "Verification", "verification.enabled", Kind::Bool, |c| on(c.verification.enabled)),
        r!("6", "Panic Mode", "panic.executors", Kind::Switch("3", "0"), |c| on(c.panic.executors > 0)),
        r!("7", "Quarantine Role", "antinuke.quarantine_role", Kind::Role, |c| mentions(c.antinuke.quarantine_role.map(|r| r.get()), "@&")),
        r!("8", "Logging Channel", "logging.fallback", Kind::Channel, |c| mentions(c.logging.fallback.map(|r| r.get()), "#")),
        r!("9", "Main Roles", "lockdown.main_roles", Kind::Roles, |c| roles(&c.lockdown.main_roles)),
        dash!("10", "Wizard"),
    ],
};

/// Read only here: only the server owner changes it, with 2FA on the dashboard.
pub(super) static BANSYNC: Module = Module {
    names: &["bansync"],
    title: "Ban Sync Panel:",
    per_page: 0,
    footer: "Only the server owner can change Ban Sync, on the dashboard",
    rows: &[
        r!("1", "Status", "ban_sync.enabled", Kind::Bool, |c| on(c.ban_sync.enabled)),
        r!("2", "Send Bans", "ban_sync.share", Kind::Bool, |c| on(c.ban_sync.share)),
        group!("3", "Send Automatic Bans"),
        r!("3a", "Join Raid", "ban_sync.share_joinraid", Kind::Bool, |c| on(c.ban_sync.share_joinraid)),
        r!("3b", "Automod", "ban_sync.share_automod", Kind::Bool, |c| on(c.ban_sync.share_automod)),
        r!("3c", "Join Gate", "ban_sync.share_joingate", Kind::Bool, |c| on(c.ban_sync.share_joingate)),
        r!("3d", "Verification", "ban_sync.share_verify", Kind::Bool, |c| on(c.ban_sync.share_verify)),
        r!("4", "Receive", "ban_sync.receive", Kind::Choice(&[("off", "\"off\""), ("alert", "\"alert\""), ("ban", "\"ban\"")]), |c| format!("{:?}", c.ban_sync.receive)),
        r!("5", "Sync Unbans", "ban_sync.sync_unbans", Kind::Bool, |c| on(c.ban_sync.sync_unbans)),
    ],
};

/// Read only here: it needs 2FA on the dashboard.
pub(super) static RAIDERLIST: Module = Module {
    names: &["raiderlist"],
    title: "Raider List Panel:",
    per_page: 0,
    footer: "Change the Raider List on the dashboard · Look someone up: t!raider check @user",
    rows: &[
        r!("1", "Status", "raider_list.enabled", Kind::Bool, |c| on(c.raider_list.enabled)),
        r!("2", "Action", "raider_list.action", Kind::Choice(&[("alert", "\"alert\""), ("quarantine", "\"quarantine\""), ("kick", "\"kick\""), ("ban", "\"ban\"")]), |c| format!("{:?}", c.raider_list.action)),
        r!("3", "Categories", "raider_list.categories", Kind::Dashboard, |c| c.raider_list.categories.iter().map(|k| format!("{k:?}")).collect::<Vec<_>>().join(", ")),
        r!("4", "Alert For Members Already Here", "raider_list.scan_members", Kind::Bool, |c| on(c.raider_list.scan_members)),
        r!("5", "Lift When Removed From The List", "raider_list.lift_on_removal", Kind::Bool, |c| on(c.raider_list.lift_on_removal)),
        r!("6", "Do Not Send Detections", "raider_list.no_collect", Kind::Bool, |c| on(c.raider_list.no_collect)),
    ],
};

pub(super) static MODULES: &[&Module] = &[&STATICS, &HEAT, &HFILTERS, &ANTINUKE, &CAPS, &PANIC, &RESTORE, &JOINGATE, &JOINRAID, &VERIFICATION, &MISC, &SETUP, &BANSYNC, &RAIDERLIST];

pub fn module(name: &str) -> Option<&'static Module> {
    MODULES.iter().copied().find(|m| m.names.contains(&name))
}

/// Numbers an older panel used for a row that the panel numbers differently.
pub(super) fn alias<'a>(m: &Module, code: &'a str) -> &'a str {
    match (m.names[0], code) {
        ("joingate", "9") => "1b",
        (_, c) => c,
    }
}
