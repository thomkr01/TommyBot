//! The number maps, one row per line. Never renumber a row: people type these numbers.
use tommybot_automod::join::{Action, GateFilter, RaidCfg};
use tommybot_logging::{embed::{self, NO}, LogKind};
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

/// One Join Gate card: status and action.
macro_rules! gate_filter {
    ($n:literal, $name:literal, $f:ident) => {
        [
            group!($n, $name),
            r!(concat!($n, "a"), "Status", concat!("joingate.", stringify!($f), ".enabled"), Kind::Bool, |c| on(c.joingate.$f.enabled)),
            r!(concat!($n, "b"), "Action", concat!("joingate.", stringify!($f), ".action"), Kind::Choice(ACTIONS), |c| gate(c.joingate.$f)),
        ]
    };
}

const FILTER_ACTIONS: &[(&str, &str)] = &[("delete", "\"delete\""), ("warn", "\"warn\""), ("timeout", "\"timeout\""), ("kick", "\"kick\""), ("ban", "\"ban\"")];
const ACTIONS: &[(&str, &str)] = &[("log", "\"Log\""), ("warn", "\"Warn\""), ("timeout", "\"Timeout\""), ("kick", "\"Kick\""), ("ban", "\"Ban\"")];

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
        dash!("10", "Trusted Admins"),
        dash!("11", "Extra Owners"),
        r!("12", "Verification Channel", "verification.channel", Kind::Channel, |c| mentions(c.verification.channel.map(|r| r.get()), "#")),
    ],
};

pub(super) static HEAT: Module = Module {
    names: &["heat"],
    title: "Heat System Panel:",
    per_page: 0,
    footer: "Advanced Settings: Heat System Filters: w!hfilters",
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
        dash!("9", "Heat Panic Mode"),
        group!("10", "Auto Server Lockdown"),
        r!("10a", "Status", "automod.auto_lockdown.enabled", Kind::Bool, |c| on(c.automod.auto_lockdown.enabled)),
        r!("10b", "Mentions", "automod.auto_lockdown.mention_count", Kind::Int(1, 1000), |c| c.automod.auto_lockdown.mention_count.to_string()),
        r!("10c", "Under", "automod.auto_lockdown.mention_window_ms", Kind::Ms, |c| ms(c.automod.auto_lockdown.mention_window_ms)),
    ],
};

/// Wick's panel shows filters 6-9 in this order; the rest of the order is ours.
static HFILTERS_ROWS: [Row; 56] = {
    let [a1, a2, a3, a4] = filter!("1", "Normal Message Heat", "regular", regular; base, "per message");
    let [b1, b2, b3, b4] = filter!("2", "Similar Message Repetition Heat", "similar", similar; similarity, "at full similarity");
    let [c1, c2, c3, c4] = filter!("3", "Mentions Heat", "mentions", mentions; per_mention, "per mention");
    let [d1, d2, d3, d4] = filter!("4", "Attachments Heat", "attachments", attachments; per_image, "per image");
    let [e1, e2, e3, e4] = filter!("5", "Advertisement Heat", "invites", invites; invite, "per invite");
    let [f1, f2, f3, f4] = filter!("6", "Malicious Websites Heat", "bad_sites", bad_sites; malicious, "per message");
    let [g1, g2, g3, g4] = filter!("7", "Emojis Heat", "emojis", emojis; per_emoji, "per emoji");
    let [h1, h2, h3, h4] = filter!("8", "Uppercase Characters Heat", "chars_upper", chars_upper; per_upper, "per uppercased character");
    let [i1, i2, i3, i4] = filter!("9", "New Line Heat", "newlines", newlines; per_newline, "per newline");
    let [j1, j2, j3, j4] = filter!("10", "Lowercase Characters Heat", "chars_lower", chars_lower; per_lower, "per lowercased character");
    let [k1, k2, k3, k4] = filter!("11", "Inactive Channel Heat", "inactive_channel", inactive_channel; inactive_channel, "per message");
    let [l1, l2, l3, l4] = filter!("12", "NSFW Websites Heat", "nsfw_links", nsfw_links; nsfw, "per message");
    let [m1, m2, m3, m4] = filter!("13", "Words Blacklist Heat", "words.filter", words.filter; per_blacklisted_word, "per word");
    let [n1, n2, n3, n4] = filter!("14", "Suspicious Account Heat", "suspicion", suspicion; suspicious_account, "per message");
    [
        a1, a2, a3, a4, b1, b2, b3, b4, c1, c2, c3, c4, d1, d2, d3, d4, e1, e2, e3, e4, f1, f2, f3, f4,
        g1, g2, g3, g4, h1, h2, h3, h4, i1, i2, i3, i4, j1, j2, j3, j4, k1, k2, k3, k4, l1, l2, l3, l4,
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
    footer: "Panic Mode: w!anp",
    rows: &[
        r!("1", "Status", "antinuke.enabled", Kind::Bool, |c| on(c.antinuke.enabled)),
        dash!("2", "Limits"),
        r!("3", "Quarantine On Limit", "antinuke.auto_quarantine", Kind::Bool, |c| on(c.antinuke.auto_quarantine)),
        group!("4", "Prune"),
        r!("4a", "Prune Detection", "antinuke.prune", Kind::Bool, |c| on(c.antinuke.prune)),
        group!("5", "Quarantine Hold"),
        r!("5b", "Strict Mode", "antinuke.strict_mode", Kind::Bool, |c| on(c.antinuke.strict_mode)),
        r!("5c", "Monitor Public Roles", "antinuke.monitor_public_roles", Kind::Bool, |c| on(c.antinuke.monitor_public_roles)),
        r!("5d", "Vanity Protection", "antinuke.vanity_guard", Kind::Bool, |c| on(c.antinuke.vanity_guard)),
        r!("5e", "Strict Member Role Addition", "antinuke.strict_member_roles", Kind::Bool, |c| on(c.antinuke.strict_member_roles)),
        r!("5f", "Monitor Channel Permissions", "antinuke.monitor_channel_perms", Kind::Bool, |c| on(c.antinuke.monitor_channel_perms)),
        r!("6", "Ticket Categories", "antinuke.ticket_categories", Kind::Channels, |c| chans(&c.antinuke.ticket_categories)),
    ],
};

pub(super) static PANIC: Module = Module {
    names: &["anpanic", "anp"],
    title: "Panic Mode Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Status", "panic.executors", Kind::Switch("3", "0"), |c| on(c.panic.executors > 0)),
        r!("2", "Heat Algorithm", "panic.heat_algorithm", Kind::Bool, |c| on(c.panic.heat_algorithm)),
        r!("4", "Unlock Server When Ending", "panic.unlock_when_ending", Kind::Bool, |c| on(c.panic.unlock_when_ending)),
        r!("5", "Warned Roles", "panic.warned_roles", Kind::Roles, |c| roles(&c.panic.warned_roles)),
        r!("6", "Whitelisted Categories", "panic.whitelisted_categories", Kind::Channels, |c| chans(&c.panic.whitelisted_categories)),
        r!("7", "Lock Mod Cmds On Trigger", "panic.lock_mod_commands", Kind::Bool, |c| on(c.panic.lock_mod_commands)),
        r!("8", "Trigger", "panic.executors", Kind::Int(1, 50), |c| format!("{} quarantined staff", c.panic.executors)),
        r!("9", "Panic Duration", "panic.duration_secs", Kind::Secs, |c| embed::duration(c.panic.duration_secs as i64)),
    ],
};

static JOINGATE_ROWS: [Row; 24] = {
    let [a1, a2, a3] = gate_filter!("2", "No PfP Filter", no_avatar);
    let [b1, b2, b3] = gate_filter!("3", "Account Age Filter", account_age);
    let [c1, c2, c3] = gate_filter!("4", "Bot Addition Filter", unauthorized_bot);
    let [d1, d2, d3] = gate_filter!("5", "Advertising Names", invite_in_name);
    let [e1, e2, e3] = gate_filter!("6", "Unverified Bots", unverified_bot);
    let [f1, f2, f3] = gate_filter!("7", "Suspicious Accounts", suspicious);
    let [g1, g2, g3] = gate_filter!("8", "Username Filter", username_filter);
    [
        r!("1", "Status", "joingate.enabled", Kind::Bool, |c| on(c.joingate.enabled)),
        a1, a2, a3, b1, b2, b3,
        r!("3c", "Minimum Age", "joingate.min_age_days", Kind::Int(1, 3650), |c| format!("{}d", c.joingate.min_age_days)),
        c1, c2, c3, d1, d2, d3, e1, e2, e3, f1, f2, f3, g1, g2, g3,
        r!("9", "DM Punished Members", "joingate.dm_punished", Kind::Bool, |c| on(c.joingate.dm_punished)),
    ]
};

pub(super) static JOINGATE: Module = Module {
    names: &["joingate", "jg"],
    title: "Join Gate Panel:",
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
        r!("8c", "Margin", "joinraid.id_margin", Kind::Int(0, 24), |c| raid(c).id_margin.to_string()),
        r!("8d", "Minimum Matches", "joinraid.id_flag_min", Kind::Int(2, 500), |c| raid(c).id_flag_min.map_or(NO.into(), |n| n.to_string())),
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
        r!("13", "Notes", "verification.show_notes", Kind::Bool, |c| on(c.verification.verify.show_notes)),
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
        r!("2h", "Auto Delete Mod Cmd", "misc.delete_invocation", Kind::Bool, |c| on(c.misc.delete_invocation)),
        r!("2i", "Remove Main Roles On TO", "misc.remove_main_roles_on_timeout", Kind::Bool, |c| on(c.misc.remove_main_roles_on_timeout)),
        group!("3", "Warn Settings"),
        r!("3a", "Status", "misc.auto_warn_actions", Kind::Bool, |c| on(c.misc.auto_warn_actions)),
        dash!("3b", "Warn Actions"),
        r!("4", "Compact Outputs", "misc.compact_replies", Kind::Bool, |c| on(c.misc.compact_replies)),
        r!("5", "Informing Messages", "misc.delete_replies_secs", Kind::Switch("5", "null"), |c| on(c.misc.delete_replies_secs.is_some())),
        r!("6", "Enforce Quarantine Perms", "misc.enforce_quarantine_perms", Kind::Bool, |c| on(c.misc.enforce_quarantine_perms)),
        r!("7", "Ignore Permit 1 Commands", "misc.ignore_no_permit", Kind::Bool, |c| on(c.misc.ignore_no_permit)),
        r!("8", "Enforce Permits", "misc.enforce_permits", Kind::Bool, |c| on(c.misc.enforce_permits)),
    ],
};

/// Wick's `w!setup` walks through these; the roles and channels themselves are made by the dashboard wizard.
pub(super) static SETUP: Module = Module {
    names: &["setup", "config"],
    title: "Setup Panel:",
    per_page: 0,
    footer: "Auto setup (roles, log channels): Dashboard wizard",
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

pub(super) static MODULES: &[&Module] = &[&STATICS, &HEAT, &HFILTERS, &ANTINUKE, &PANIC, &JOINGATE, &JOINRAID, &VERIFICATION, &MISC, &SETUP];

pub fn module(name: &str) -> Option<&'static Module> {
    MODULES.iter().copied().find(|m| m.names.contains(&name))
}
