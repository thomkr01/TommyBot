//! Wick's Discord-side config. `w!<module>` shows a numbered panel; `VALUE ?set N`, `N ?on` / `N ?off`
//! and `OBJ ?add N` / `OBJ ?remove N` change it. The numbers are public API (people memorise
//! `w!jg 3c`), so never renumber a row: add new ones after the last.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_automod::join::{Action, RaidCfg};
use tommybot_core::{duration, permits::Level};
use tommybot_logging::embed::{self, ARROW, NO};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component},
        AllowedMentions, Embed, MessageFlags,
    },
    gateway::payload::incoming::MessageCreate,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{GuildMarker, RoleMarker},
        Id,
    },
};

use crate::{
    app::{App, GuildCfg},
    targets::{pick, Pick},
};

#[derive(Clone, Copy)]
pub enum Kind {
    Bool,
    Int(i64, i64),
    /// Stored as the typed number times this (0.01 for percentages kept as fractions).
    Num(f64),
    Secs,
    Ms,
    Text,
    /// Typed word -> stored JSON.
    Choice(&'static [(&'static str, &'static str)]),
    /// Stored JSON for `?on` and `?off`; `?on` keeps a value that is already on.
    Switch(&'static str, &'static str),
    Role,
    Channel,
    Roles,
    Channels,
    /// Header for the lettered rows below it.
    Group,
    Dashboard,
}

pub struct Row {
    pub code: &'static str,
    pub name: &'static str,
    /// `section.field.sub` in the config document.
    pub path: &'static str,
    pub kind: Kind,
    pub get: fn(&GuildCfg) -> String,
}

pub struct Module {
    pub names: &'static [&'static str],
    pub title: &'static str,
    /// Top-level entries per page; 0 shows everything on one page.
    pub per_page: usize,
    pub footer: &'static str,
    pub rows: &'static [Row],
}

macro_rules! r {
    ($code:expr, $name:literal, $path:expr, $kind:expr, |$c:ident| $get:expr) => {
        Row {
            code: $code,
            name: $name,
            path: $path,
            kind: $kind,
            get: |$c: &GuildCfg| -> String { $get },
        }
    };
}

macro_rules! group {
    ($code:literal, $name:literal) => {
        Row {
            code: $code,
            name: $name,
            path: "",
            kind: Kind::Group,
            get: |_| String::new(),
        }
    };
}

macro_rules! dash {
    ($code:literal, $name:literal) => {
        Row {
            code: $code,
            name: $name,
            path: "",
            kind: Kind::Dashboard,
            get: |_| String::new(),
        }
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

const FILTER_ACTIONS: &[(&str, &str)] = &[
    ("delete", "\"delete\""),
    ("warn", "\"warn\""),
    ("timeout", "\"timeout\""),
    ("kick", "\"kick\""),
    ("ban", "\"ban\""),
];
const ACTIONS: &[(&str, &str)] = &[
    ("log", "\"Log\""),
    ("warn", "\"Warn\""),
    ("timeout", "\"Timeout\""),
    ("kick", "\"Kick\""),
    ("ban", "\"Ban\""),
];

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

fn gate(f: tommybot_automod::join::GateFilter) -> String {
    f.action.map_or("Default".into(), act)
}

fn mentions(ids: impl IntoIterator<Item = u64>, pfx: &str) -> String {
    let s: Vec<_> = ids.into_iter().map(|i| format!("<{pfx}{i}>")).collect();
    if s.is_empty() {
        NO.into()
    } else {
        s.join(" ")
    }
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

static STATICS: Module = Module {
    names: &["statics", "s"],
    title: "Statics Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!(
            "2",
            "Quarantine Role",
            "antinuke.quarantine_role",
            Kind::Role,
            |c| mentions(c.antinuke.quarantine_role.map(|r| r.get()), "@&")
        ),
        r!("5", "Main Roles", "lockdown.main_roles", Kind::Roles, |c| {
            roles(&c.lockdown.main_roles)
        }),
        r!(
            "6",
            "Logging Channel",
            "logging.fallback",
            Kind::Channel,
            |c| mentions(c.logging.fallback.map(|r| r.get()), "#")
        ),
        r!(
            "7",
            "Mod-Logging Channel",
            "logging.routes.mod_command",
            Kind::Channel,
            |c| mentions(
                c.logging
                    .routes
                    .get(&tommybot_logging::LogKind::ModCommand)
                    .map(|r| r.get()),
                "#"
            )
        ),
        r!(
            "8",
            "Partnering Channels",
            "whitelist.partner_channels",
            Kind::Channels,
            |c| chans(&c.whitelist.partner_channels)
        ),
        dash!("10", "Trusted Admins"),
        dash!("11", "Extra Owners"),
        r!(
            "12",
            "Verification Channel",
            "verification.channel",
            Kind::Channel,
            |c| mentions(c.verification.channel.map(|r| r.get()), "#")
        ),
    ],
};

static HEAT: Module = Module {
    names: &["heat"],
    title: "Heat System Panel:",
    per_page: 0,
    footer: "Advanced Settings: Heat System Filters: w!hfilters",
    rows: &[
        r!("1", "Status", "automod.enabled", Kind::Bool, |c| on(c
            .automod
            .enabled)),
        r!(
            "2",
            "Spam Filters",
            "automod.anti_spam",
            Kind::Bool,
            |c| on(c.automod.anti_spam)
        ),
        r!(
            "3",
            "Max Heat Percentage",
            "automod.heat.max",
            Kind::Num(1.0),
            |c| pct(c.automod.heat.max)
        ),
        r!(
            "4",
            "Heat Degradation",
            "automod.heat.decay_per_sec",
            Kind::Num(0.01),
            |c| format!("{} per second", pct(c.automod.heat.decay_per_sec * 100.0))
        ),
        r!(
            "5",
            "Strikes CAP",
            "automod.strike.cap",
            Kind::Int(1, 100),
            |c| c.automod.strike.cap.to_string()
        ),
        r!(
            "6",
            "Webhook Coverage",
            "automod.monitor_webhooks",
            Kind::Bool,
            |c| on(c.automod.monitor_webhooks)
        ),
        group!("7", "Auto Timeouts"),
        r!(
            "7a",
            "Status",
            "automod.strike.enabled",
            Kind::Bool,
            |c| on(c.automod.strike.enabled)
        ),
        r!(
            "7b",
            "Regular Strike Duration",
            "automod.strike.strike_timeout_ms",
            Kind::Ms,
            |c| ms(c.automod.strike.strike_timeout_ms)
        ),
        r!(
            "7c",
            "CAP Strike Duration",
            "automod.strike.cap_timeout_ms",
            Kind::Ms,
            |c| ms(c.automod.strike.cap_timeout_ms)
        ),
        r!(
            "7d",
            "Multiplier Status",
            "automod.strike.multiply",
            Kind::Bool,
            |c| on(c.automod.strike.multiply)
        ),
        r!(
            "7e",
            "Multiplier Percentage",
            "automod.strike.multiplier_pct",
            Kind::Int(100, 1000),
            |c| format!("{}%", c.automod.strike.multiplier_pct)
        ),
        r!(
            "8",
            "Reset Heat On Timeout",
            "automod.strike.reset_heat_on_timeout",
            Kind::Bool,
            |c| on(c.automod.strike.reset_heat_on_timeout)
        ),
        dash!("9", "Heat Panic Mode"),
        group!("10", "Auto Server Lockdown"),
        r!(
            "10a",
            "Status",
            "automod.auto_lockdown.enabled",
            Kind::Bool,
            |c| on(c.automod.auto_lockdown.enabled)
        ),
        r!(
            "10b",
            "Mentions",
            "automod.auto_lockdown.mention_count",
            Kind::Int(1, 1000),
            |c| c.automod.auto_lockdown.mention_count.to_string()
        ),
        r!(
            "10c",
            "Under",
            "automod.auto_lockdown.mention_window_ms",
            Kind::Ms,
            |c| ms(c.automod.auto_lockdown.mention_window_ms)
        ),
    ],
};

/// The filter order follows Wick's panel where it is known (6-9); the rest is ours.
static HFILTERS_ROWS: [Row; 56] = {
    let [a1, a2, a3, a4] =
        filter!("1", "Normal Message Heat", "regular", regular; base, "per message");
    let [b1, b2, b3, b4] = filter!("2", "Similar Message Repetition Heat", "similar", similar; similarity, "at full similarity");
    let [c1, c2, c3, c4] =
        filter!("3", "Mentions Heat", "mentions", mentions; per_mention, "per mention");
    let [d1, d2, d3, d4] =
        filter!("4", "Attachments Heat", "attachments", attachments; per_image, "per image");
    let [e1, e2, e3, e4] =
        filter!("5", "Advertisement Heat", "invites", invites; invite, "per invite");
    let [f1, f2, f3, f4] =
        filter!("6", "Malicious Websites Heat", "bad_sites", bad_sites; malicious, "per message");
    let [g1, g2, g3, g4] = filter!("7", "Emojis Heat", "emojis", emojis; per_emoji, "per emoji");
    let [h1, h2, h3, h4] = filter!("8", "Uppercase Characters Heat", "chars_upper", chars_upper; per_upper, "per uppercased character");
    let [i1, i2, i3, i4] =
        filter!("9", "New Line Heat", "newlines", newlines; per_newline, "per newline");
    let [j1, j2, j3, j4] = filter!("10", "Lowercase Characters Heat", "chars_lower", chars_lower; per_lower, "per lowercased character");
    let [k1, k2, k3, k4] = filter!("11", "Inactive Channel Heat", "inactive_channel", inactive_channel; inactive_channel, "per message");
    let [l1, l2, l3, l4] =
        filter!("12", "NSFW Websites Heat", "nsfw_links", nsfw_links; nsfw, "per message");
    let [m1, m2, m3, m4] = filter!("13", "Words Blacklist Heat", "words.filter", words.filter; per_blacklisted_word, "per word");
    let [n1, n2, n3, n4] = filter!("14", "Suspicious Account Heat", "suspicion", suspicion; suspicious_account, "per message");
    [
        a1, a2, a3, a4, b1, b2, b3, b4, c1, c2, c3, c4, d1, d2, d3, d4, e1, e2, e3, e4, f1, f2, f3,
        f4, g1, g2, g3, g4, h1, h2, h3, h4, i1, i2, i3, i4, j1, j2, j3, j4, k1, k2, k3, k4, l1, l2,
        l3, l4, m1, m2, m3, m4, n1, n2, n3, n4,
    ]
};

static HFILTERS: Module = Module {
    names: &["hfilters"],
    title: "Heat System Filters Panel:",
    per_page: 4,
    footer: "",
    rows: &HFILTERS_ROWS,
};

static ANTINUKE: Module = Module {
    names: &["antinuke", "an"],
    title: "Anti Nuke Panel:",
    per_page: 0,
    footer: "Panic Mode: w!anp",
    rows: &[
        r!("1", "Status", "antinuke.enabled", Kind::Bool, |c| on(c
            .antinuke
            .enabled)),
        dash!("2", "Limits"),
        r!(
            "3",
            "Quarantine On Limit",
            "antinuke.auto_quarantine",
            Kind::Bool,
            |c| on(c.antinuke.auto_quarantine)
        ),
        group!("4", "Prune"),
        r!("4a", "Prune Detection", "antinuke.prune", Kind::Bool, |c| {
            on(c.antinuke.prune)
        }),
        group!("5", "Quarantine Hold"),
        r!(
            "5b",
            "Strict Mode",
            "antinuke.strict_mode",
            Kind::Bool,
            |c| on(c.antinuke.strict_mode)
        ),
        r!(
            "5c",
            "Monitor Public Roles",
            "antinuke.monitor_public_roles",
            Kind::Bool,
            |c| on(c.antinuke.monitor_public_roles)
        ),
        r!(
            "5d",
            "Vanity Protection",
            "antinuke.vanity_guard",
            Kind::Bool,
            |c| on(c.antinuke.vanity_guard)
        ),
        r!(
            "5e",
            "Strict Member Role Addition",
            "antinuke.strict_member_roles",
            Kind::Bool,
            |c| on(c.antinuke.strict_member_roles)
        ),
        r!(
            "5f",
            "Monitor Channel Permissions",
            "antinuke.monitor_channel_perms",
            Kind::Bool,
            |c| on(c.antinuke.monitor_channel_perms)
        ),
        r!(
            "6",
            "Ticket Categories",
            "antinuke.ticket_categories",
            Kind::Channels,
            |c| chans(&c.antinuke.ticket_categories)
        ),
    ],
};

static PANIC: Module = Module {
    names: &["anpanic", "anp"],
    title: "Panic Mode Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!(
            "1",
            "Status",
            "panic.executors",
            Kind::Switch("3", "0"),
            |c| on(c.panic.executors > 0)
        ),
        r!(
            "2",
            "Heat Algorithm",
            "panic.heat_algorithm",
            Kind::Bool,
            |c| on(c.panic.heat_algorithm)
        ),
        r!(
            "4",
            "Unlock Server When Ending",
            "panic.unlock_when_ending",
            Kind::Bool,
            |c| on(c.panic.unlock_when_ending)
        ),
        r!(
            "5",
            "Warned Roles",
            "panic.warned_roles",
            Kind::Roles,
            |c| roles(&c.panic.warned_roles)
        ),
        r!(
            "6",
            "Whitelisted Categories",
            "panic.whitelisted_categories",
            Kind::Channels,
            |c| chans(&c.panic.whitelisted_categories)
        ),
        r!(
            "7",
            "Lock Mod Cmds On Trigger",
            "panic.lock_mod_commands",
            Kind::Bool,
            |c| on(c.panic.lock_mod_commands)
        ),
        r!(
            "8",
            "Trigger",
            "panic.executors",
            Kind::Int(1, 50),
            |c| format!("{} quarantined staff", c.panic.executors)
        ),
        r!(
            "9",
            "Panic Duration",
            "panic.duration_secs",
            Kind::Secs,
            |c| embed::duration(c.panic.duration_secs as i64)
        ),
    ],
};

macro_rules! gate_filter {
    ($n:literal, $name:literal, $f:ident) => {
        [
            group!($n, $name),
            r!(
                concat!($n, "a"),
                "Status",
                concat!("joingate.", stringify!($f), ".enabled"),
                Kind::Bool,
                |c| on(c.joingate.$f.enabled)
            ),
            r!(
                concat!($n, "b"),
                "Action",
                concat!("joingate.", stringify!($f), ".action"),
                Kind::Choice(ACTIONS),
                |c| gate(c.joingate.$f)
            ),
        ]
    };
}

static JOINGATE_ROWS: [Row; 24] = {
    let [a1, a2, a3] = gate_filter!("2", "No PfP Filter", no_avatar);
    let [b1, b2, b3] = gate_filter!("3", "Account Age Filter", account_age);
    let [c1, c2, c3] = gate_filter!("4", "Bot Addition Filter", unauthorized_bot);
    let [d1, d2, d3] = gate_filter!("5", "Advertising Names", invite_in_name);
    let [e1, e2, e3] = gate_filter!("6", "Unverified Bots", unverified_bot);
    let [f1, f2, f3] = gate_filter!("7", "Suspicious Accounts", suspicious);
    let [g1, g2, g3] = gate_filter!("8", "Username Filter", username_filter);
    [
        r!("1", "Status", "joingate.enabled", Kind::Bool, |c| on(c
            .joingate
            .enabled)),
        a1,
        a2,
        a3,
        b1,
        b2,
        b3,
        r!(
            "3c",
            "Minimum Age",
            "joingate.min_age_days",
            Kind::Int(1, 3650),
            |c| format!("{}d", c.joingate.min_age_days)
        ),
        c1,
        c2,
        c3,
        d1,
        d2,
        d3,
        e1,
        e2,
        e3,
        f1,
        f2,
        f3,
        g1,
        g2,
        g3,
        r!(
            "9",
            "DM Punished Members",
            "joingate.dm_punished",
            Kind::Bool,
            |c| on(c.joingate.dm_punished)
        ),
    ]
};

static JOINGATE: Module = Module {
    names: &["joingate", "jg"],
    title: "Join Gate Panel:",
    per_page: 0,
    footer: "Username words: Dashboard",
    rows: &JOINGATE_ROWS,
};

static JOINRAID: Module = Module {
    names: &["joinraid", "jr"],
    title: "Join Raid",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Status", "joinraid.enabled", Kind::Bool, |c| on(raid(
            c
        )
        .enabled)),
        r!(
            "2",
            "Action",
            "joinraid.action",
            Kind::Choice(ACTIONS),
            |c| act(raid(c).action)
        ),
        r!(
            "3",
            "Warned Roles",
            "joinraid.warned_roles",
            Kind::Roles,
            |c| mentions(
                raid(c).warned_roles.iter().filter_map(|r| r.parse().ok()),
                "@&"
            )
        ),
        group!("4", "Static Flags"),
        r!(
            "4a",
            "Minimum Trigger",
            "joinraid.count",
            Kind::Int(2, 500),
            |c| format!("{} accounts.", raid(c).count)
        ),
        r!(
            "4b",
            "Join History",
            "joinraid.span_ms",
            Kind::Ms,
            |c| format!("Past {}", ms(raid(c).span_ms))
        ),
        r!(
            "4c",
            "Trigger Duration",
            "joinraid.active_ms",
            Kind::Ms,
            |c| ms(raid(c).active_ms)
        ),
        r!(
            "4d",
            "Accounts Type",
            "joinraid.suspicious_only",
            Kind::Choice(&[("all", "false"), ("suspicious", "true")]),
            |c| if raid(c).suspicious_only {
                "Suspicious"
            } else {
                "All"
            }
            .into()
        ),
        dash!("5", "String Flag"),
        group!("6", "Age Flag"),
        r!(
            "6a",
            "Status",
            "joinraid.flag_younger_than_ms",
            Kind::Switch("86400000", "null"),
            |c| on(raid(c).flag_younger_than_ms.is_some())
        ),
        r!(
            "6b",
            "Minimum Age",
            "joinraid.flag_younger_than_ms",
            Kind::Ms,
            |c| raid(c).flag_younger_than_ms.map_or(NO.into(), ms)
        ),
        r!(
            "7",
            "NoPfP Flag",
            "joinraid.flag_no_avatar",
            Kind::Bool,
            |c| on(raid(c).flag_no_avatar)
        ),
        group!("8", "ID Flag"),
        r!(
            "8a",
            "Status",
            "joinraid.id_flag_min",
            Kind::Switch("4", "null"),
            |c| on(raid(c).id_flag_min.is_some())
        ),
        r!(
            "8b",
            "Granularity",
            "joinraid.id_granularity",
            Kind::Choice(&[
                ("adapt", "\"Adaptive\""),
                ("h", "\"Hour\""),
                ("d", "\"Day\""),
                ("m", "\"Month\"")
            ]),
            |c| format!("{:?}", raid(c).id_granularity)
        ),
        r!(
            "8c",
            "Margin",
            "joinraid.id_margin",
            Kind::Int(0, 24),
            |c| raid(c).id_margin.to_string()
        ),
        r!(
            "8d",
            "Minimum Matches",
            "joinraid.id_flag_min",
            Kind::Int(2, 500),
            |c| raid(c).id_flag_min.map_or(NO.into(), |n| n.to_string())
        ),
    ],
};

static VERIFICATION: Module = Module {
    names: &["verification", "v"],
    title: "Verification Panel:",
    per_page: 0,
    footer: "Captcha Customization: Dashboard",
    rows: &[
        r!("1", "Status", "verification.enabled", Kind::Bool, |c| on(c
            .verification
            .enabled)),
        r!(
            "2",
            "Action",
            "verification.on_fail",
            Kind::Choice(&[
                ("quarantine", "\"Quarantine\""),
                ("kick", "\"Kick\""),
                ("ban", "\"Ban\""),
                ("none", "\"Nothing\"")
            ]),
            |c| format!("{:?}", c.verification.verify.on_fail)
        ),
        r!(
            "3",
            "Target",
            "verification.target",
            Kind::Choice(&[
                ("all", "\"Everyone\""),
                ("suspicious", "\"SuspiciousOnly\"")
            ]),
            |c| format!("{:?}", c.verification.verify.target)
        ),
        r!(
            "4",
            "Verified Roles",
            "verification.roles",
            Kind::Roles,
            |c| roles(&c.verification.roles)
        ),
        r!(
            "5",
            "Duration",
            "verification.time_limit_ms",
            Kind::Ms,
            |c| ms(c.verification.verify.time_limit_ms)
        ),
        r!(
            "6",
            "Mode",
            "verification.mode",
            Kind::Choice(&[
                ("captcha", "\"Captcha\""),
                ("rucaptcha", "\"SimpleCaptcha\""),
                ("none", "\"Button\""),
                ("web", "\"Web\""),
                ("dm", "\"Dm\"")
            ]),
            |c| format!("{:?}", c.verification.verify.mode)
        ),
        r!(
            "13",
            "Notes",
            "verification.show_notes",
            Kind::Bool,
            |c| on(c.verification.verify.show_notes)
        ),
    ],
};

static MISC: Module = Module {
    names: &["misc"],
    title: "Miscellaneous Panel:",
    per_page: 0,
    footer: "",
    rows: &[
        r!("1", "Prefix", "misc.prefix", Kind::Text, |c| c
            .misc
            .prefix
            .clone()),
        group!("2", "Moderation"),
        r!("2a", "Confirm Actions", "misc.confirm", Kind::Bool, |c| on(
            c.misc.confirm
        )),
        r!(
            "2b",
            "Days Purged On Bans",
            "misc.ban_purge_days",
            Kind::Int(0, 7),
            |c| c.misc.ban_purge_days.to_string()
        ),
        r!(
            "2c",
            "Default Timeout Duration",
            "misc.default_timeout_secs",
            Kind::Secs,
            |c| embed::duration(c.misc.timeout_secs() as i64)
        ),
        r!(
            "2d",
            "Anonymous Mod",
            "misc.hide_staff_in_dms",
            Kind::Bool,
            |c| on(c.misc.hide_staff_in_dms)
        ),
        r!("2e", "DM Targets", "misc.always_dm", Kind::Bool, |c| on(c
            .misc
            .always_dm)),
        r!(
            "2f",
            "Display Reason On Auto Timeout",
            "misc.show_reason_on_auto",
            Kind::Bool,
            |c| on(c.misc.show_reason_on_auto)
        ),
        r!(
            "2h",
            "Auto Delete Mod Cmd",
            "misc.delete_invocation",
            Kind::Bool,
            |c| on(c.misc.delete_invocation)
        ),
        r!(
            "2i",
            "Remove Main Roles On TO",
            "misc.remove_main_roles_on_timeout",
            Kind::Bool,
            |c| on(c.misc.remove_main_roles_on_timeout)
        ),
        group!("3", "Warn Settings"),
        r!(
            "3a",
            "Status",
            "misc.auto_warn_actions",
            Kind::Bool,
            |c| on(c.misc.auto_warn_actions)
        ),
        dash!("3b", "Warn Actions"),
        r!(
            "4",
            "Compact Outputs",
            "misc.compact_replies",
            Kind::Bool,
            |c| on(c.misc.compact_replies)
        ),
        r!(
            "5",
            "Informing Messages",
            "misc.delete_replies_secs",
            Kind::Switch("5", "null"),
            |c| on(c.misc.delete_replies_secs.is_some())
        ),
        r!(
            "6",
            "Enforce Quarantine Perms",
            "misc.enforce_quarantine_perms",
            Kind::Bool,
            |c| on(c.misc.enforce_quarantine_perms)
        ),
        r!(
            "7",
            "Ignore Permit 1 Commands",
            "misc.ignore_no_permit",
            Kind::Bool,
            |c| on(c.misc.ignore_no_permit)
        ),
        r!(
            "8",
            "Enforce Permits",
            "misc.enforce_permits",
            Kind::Bool,
            |c| on(c.misc.enforce_permits)
        ),
    ],
};

/// Wick's `w!setup` walks through these; the roles and channels themselves are made by the dashboard wizard.
static SETUP: Module = Module {
    names: &["setup"],
    title: "Setup Panel:",
    per_page: 0,
    footer: "Auto setup (roles, log channels): Dashboard wizard",
    rows: &[
        r!("1", "Anti Nuke", "antinuke.enabled", Kind::Bool, |c| on(c
            .antinuke
            .enabled)),
        r!("2", "Auto Mod", "automod.enabled", Kind::Bool, |c| on(c
            .automod
            .enabled)),
        r!("3", "Join Gate", "joingate.enabled", Kind::Bool, |c| on(c
            .joingate
            .enabled)),
        r!("4", "Join Raid", "joinraid.enabled", Kind::Bool, |c| on(
            raid(c).enabled
        )),
        r!(
            "5",
            "Verification",
            "verification.enabled",
            Kind::Bool,
            |c| on(c.verification.enabled)
        ),
        r!(
            "6",
            "Panic Mode",
            "panic.executors",
            Kind::Switch("3", "0"),
            |c| on(c.panic.executors > 0)
        ),
        r!(
            "7",
            "Quarantine Role",
            "antinuke.quarantine_role",
            Kind::Role,
            |c| mentions(c.antinuke.quarantine_role.map(|r| r.get()), "@&")
        ),
        r!(
            "8",
            "Logging Channel",
            "logging.fallback",
            Kind::Channel,
            |c| mentions(c.logging.fallback.map(|r| r.get()), "#")
        ),
        r!("9", "Main Roles", "lockdown.main_roles", Kind::Roles, |c| {
            roles(&c.lockdown.main_roles)
        }),
        dash!("10", "Wizard"),
    ],
};

static MODULES: &[&Module] = &[
    &STATICS,
    &HEAT,
    &HFILTERS,
    &ANTINUKE,
    &PANIC,
    &JOINGATE,
    &JOINRAID,
    &VERIFICATION,
    &MISC,
    &SETUP,
];

pub fn module(name: &str) -> Option<&'static Module> {
    MODULES.iter().copied().find(|m| m.names.contains(&name))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Op {
    Show(usize),
    Set(String, String),
    Toggle(String, bool),
    Add(String, String),
    Remove(String, String),
}

const USAGE: &str =
    "Usage: `VALUE ?set N`, `N ?on`, `N ?off`, `OBJECT ?add N` or `OBJECT ?remove N`";

/// Everything after the module name.
pub fn parse_op(args: &str) -> Result<Op, &'static str> {
    let toks: Vec<&str> = args.split_whitespace().collect();
    let Some(at) = toks.iter().position(|t| t.starts_with('?')) else {
        return match toks.as_slice() {
            [] => Ok(Op::Show(0)),
            [p] => p
                .parse::<usize>()
                .map(|p| Op::Show(p.saturating_sub(1)))
                .map_err(|_| USAGE),
            _ => Err(USAGE),
        };
    };
    let flag = toks[at].to_lowercase();
    let value = toks[..at].join(" ");
    match flag.as_str() {
        "?on" | "?off" => {
            let code = toks[..at].last().ok_or(USAGE)?.to_lowercase();
            Ok(Op::Toggle(code, flag == "?on"))
        }
        "?set" | "?add" | "?remove" => {
            let code = toks.get(at + 1).ok_or(USAGE)?.to_lowercase();
            if value.is_empty() {
                return Err(USAGE);
            }
            Ok(match flag.as_str() {
                "?set" => Op::Set(code, value),
                "?add" => Op::Add(code, value),
                _ => Op::Remove(code, value),
            })
        }
        _ => Err(USAGE),
    }
}

#[derive(Debug, PartialEq)]
pub enum Edit {
    Put(Value),
    Add(Vec<Value>),
    Remove(Vec<Value>),
}

fn literal(s: &str) -> Value {
    serde_json::from_str(s).expect("settings table literal")
}

/// Turns an op on one row into an edit. `ids` are the resolved roles or channels for object rows.
pub fn edit_for(row: &Row, op: &Op, ids: &[u64], current: Option<&Value>) -> Result<Edit, String> {
    let label = format!("[{}] {}", row.code.to_uppercase(), row.name);
    let ids_json = || ids.iter().map(|i| json!(i.to_string())).collect::<Vec<_>>();
    match (row.kind, op) {
        (Kind::Dashboard, _) => Err(format!("{label} can only be changed on the dashboard")),
        (Kind::Group, _) => Err(format!(
            "{label} has sub-settings, use {}a, {}b...",
            row.code, row.code
        )),
        (Kind::Bool, Op::Toggle(_, on)) => Ok(Edit::Put(json!(on))),
        (Kind::Switch(on, off), Op::Toggle(_, true)) => Ok(Edit::Put(match current {
            Some(v) if !v.is_null() && *v != literal(off) => v.clone(),
            _ => literal(on),
        })),
        (Kind::Switch(_, off), Op::Toggle(_, false)) => Ok(Edit::Put(literal(off))),
        (Kind::Bool | Kind::Switch(..), _) => Err(format!(
            "{label} is a switch: use `{} ?on` or `{} ?off`",
            row.code, row.code
        )),
        (_, Op::Toggle(..)) => Err(format!(
            "{label} is not a switch: use `VALUE ?set {}`",
            row.code
        )),
        (Kind::Roles | Kind::Channels, Op::Set(..) | Op::Add(..)) => Ok(Edit::Add(ids_json())),
        (Kind::Roles | Kind::Channels, Op::Remove(..)) => Ok(Edit::Remove(ids_json())),
        (_, Op::Add(..) | Op::Remove(..)) => Err(format!(
            "{label} is not a list: use `VALUE ?set {}`",
            row.code
        )),
        (kind, Op::Set(_, v)) => {
            let bad = || format!("`{v}` is not a valid value for {label}");
            Ok(Edit::Put(match kind {
                Kind::Int(min, max) => {
                    let n: i64 = v.parse().map_err(|_| bad())?;
                    if !(min..=max).contains(&n) {
                        return Err(format!("{label} must be between {min} and {max}"));
                    }
                    json!(n)
                }
                Kind::Num(scale) => {
                    let n: f64 = v.trim_end_matches('%').parse().map_err(|_| bad())?;
                    if !n.is_finite() || n < 0.0 {
                        return Err(bad());
                    }
                    json!(n * scale)
                }
                Kind::Secs => json!(duration::parse(v).ok_or_else(bad)?),
                Kind::Ms => json!(duration::parse(v).ok_or_else(bad)? * 1000),
                Kind::Text if !v.is_empty() && v.len() <= 32 => json!(v),
                Kind::Choice(opts) => {
                    let v = v.to_lowercase();
                    let found = opts.iter().find(|(k, _)| *k == v);
                    let Some((_, lit)) = found else {
                        let names: Vec<_> = opts.iter().map(|(k, _)| *k).collect();
                        return Err(format!("{label} takes one of: {}", names.join(", ")));
                    };
                    literal(lit)
                }
                Kind::Role | Kind::Channel => match ids {
                    [one] => json!(one.to_string()),
                    _ => return Err(format!("{label} takes exactly one")),
                },
                _ => return Err(bad()),
            }))
        }
        (_, Op::Show(_)) => Err(USAGE.into()),
    }
}

fn id_of(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

pub fn get_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(doc, |v, k| v.get(k))
}

/// Writes `e` at `path`, creating objects on the way. Lists compare ids, so `"5"` and `5` are one entry.
pub fn apply(doc: &mut Value, path: &str, e: Edit) {
    // Join raid has no serde defaults for its required fields, so a first write starts from ours.
    if path.starts_with("joinraid.") && doc.get("joinraid").is_none_or(Value::is_null) {
        doc["joinraid"] = serde_json::to_value(RaidCfg::default()).expect("raid defaults");
    }
    let mut v = doc;
    for k in path.split('.') {
        if !v.is_object() {
            *v = json!({});
        }
        v = v.as_object_mut().unwrap().entry(k).or_insert(Value::Null);
    }
    match e {
        Edit::Put(x) => *v = x,
        Edit::Add(xs) => {
            if !v.is_array() {
                *v = json!([]);
            }
            let list = v.as_array_mut().unwrap();
            for x in xs {
                if !list.iter().any(|y| id_of(y) == id_of(&x)) {
                    list.push(x);
                }
            }
        }
        Edit::Remove(xs) => {
            if let Some(list) = v.as_array_mut() {
                list.retain(|y| !xs.iter().any(|x| id_of(x) == id_of(y)));
            }
        }
    }
}

/// Wick: Trusted Admins may not change Miscellaneous or Anti-Nuke; everyone else needs Dashboard Edit.
pub fn may_edit(level: &Level, section: &str) -> bool {
    match section {
        "misc" | "antinuke" | "panic" => matches!(level, Level::Owner | Level::ExtraOwner),
        "whitelist" => level.immune(),
        _ => level.has("dashboard_edit"),
    }
}

const GREY: u32 = 0x2B2D31;

fn dashboard_link(gid: u64) -> String {
    match std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| !u.is_empty())
    {
        Some(u) => format!("[Dashboard]({}/#/{gid})", u.trim_end_matches('/')),
        None => "Dashboard".into(),
    }
}

/// Fields as (name, value): top-level rows start one, lettered rows add a `▸ [A] Name: value` line to it.
fn entries(m: &Module, cfg: &GuildCfg, dash: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for row in m.rows {
        let val = match row.kind {
            Kind::Dashboard => dash.to_string(),
            Kind::Role | Kind::Channel | Kind::Roles | Kind::Channels => (row.get)(cfg),
            _ => format!("*`{}`*", (row.get)(cfg)),
        };
        let letter = row.code.trim_start_matches(|c: char| c.is_ascii_digit());
        match (letter.is_empty(), out.last_mut()) {
            (false, Some((_, lines))) => {
                if !lines.is_empty() {
                    lines.push('\n');
                }
                lines.push_str(&format!(
                    "{ARROW} [{}] {}: {val}",
                    letter.to_uppercase(),
                    row.name
                ));
            }
            _ => {
                let lines = match row.kind {
                    Kind::Group => String::new(),
                    _ => format!("{ARROW} {val}"),
                };
                out.push((format!("[{}] {}:", row.code, row.name), lines));
            }
        }
    }
    out
}

/// One page of a module's panel and the page count.
pub fn panel(m: &Module, cfg: &GuildCfg, gid: u64, page: usize) -> (Embed, usize) {
    let all = entries(m, cfg, &dashboard_link(gid));
    let per = if m.per_page == 0 {
        all.len().max(1)
    } else {
        m.per_page
    };
    let pages = all.len().div_ceil(per).max(1);
    let page = page.min(pages - 1);
    let mut e = embed::Log::new(m.title, GREY);
    for (name, value) in all.into_iter().skip(page * per).take(per) {
        e = e.inline(&name, &value);
    }
    let footer = match (pages, m.footer) {
        (1, f) => f.to_string(),
        (n, "") => format!("Page: {}/{n}", page + 1),
        (n, f) => format!("{f} · Page: {}/{n}", page + 1),
    };
    if !footer.is_empty() {
        e = e.footer(&footer);
    }
    (e.build(), pages)
}

const PAGE: &str = "pg:";

fn button(label: &str, id: String, style: ButtonStyle, disabled: bool) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id),
        disabled,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

/// Wick's `≪ ‹ › ≫ ✖` row. The custom id carries the invoker, the panel and the target page, so no
/// state is kept; the last field only keeps ids unique when two buttons lead to the same page.
pub fn page_row(invoker: u64, kind: &str, page: usize, pages: usize) -> Component {
    let id = |to: usize, slot: &str| format!("{PAGE}{invoker}:{kind}:{to}:{slot}");
    let last = pages - 1;
    let grey = ButtonStyle::Secondary;
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            button("≪", id(0, "f"), grey, page == 0),
            button("‹", id(page.saturating_sub(1), "p"), grey, page == 0),
            button("›", id((page + 1).min(last), "n"), grey, page >= last),
            button("≫", id(last, "l"), grey, page >= last),
            button(
                "✖",
                format!("{PAGE}{invoker}:x"),
                ButtonStyle::Danger,
                false,
            ),
        ],
    })
}

/// Turns a typed object list into ids: mentions and ids as given, names (comma separated when they
/// have spaces) by exact or unique prefix match.
async fn resolve(
    app: &App,
    gid: Id<GuildMarker>,
    roles: bool,
    value: &str,
) -> Result<Result<Vec<u64>, String>> {
    let mut ids = vec![];
    let mut words = vec![];
    for tok in value.split_whitespace() {
        let digits = tok.trim_matches(|c: char| !c.is_ascii_digit());
        match digits.parse::<u64>() {
            Ok(id) if tok.starts_with('<') || tok == digits => ids.push(id),
            _ => words.push(tok),
        }
    }
    let joined = words.join(" ");
    let names: Vec<&str> = if joined.contains(',') {
        joined
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .collect()
    } else {
        words
    };
    if names.is_empty() {
        return Ok(Ok(ids));
    }
    let all: Vec<(u64, String)> = if roles {
        app.http
            .roles(gid)
            .await?
            .models()
            .await?
            .into_iter()
            .map(|r| (r.id.get(), r.name))
            .collect()
    } else {
        app.http
            .guild_channels(gid)
            .await?
            .models()
            .await?
            .into_iter()
            .map(|c| (c.id.get(), c.name.unwrap_or_default()))
            .collect()
    };
    for n in names {
        let q = n.trim_start_matches(['@', '#']).to_lowercase();
        let cands: Vec<(u64, Vec<String>)> = all
            .iter()
            .filter(|(_, name)| name.to_lowercase().starts_with(&q))
            .map(|(id, name)| (*id, vec![name.clone()]))
            .collect();
        match pick(&q, &cands) {
            Pick::One(id) => ids.push(id),
            Pick::None => return Ok(Err(format!("Nothing found for `{n}`"))),
            Pick::Ambiguous => return Ok(Err(format!("`{n}` is ambiguous, use a mention or id"))),
        }
    }
    Ok(Ok(ids))
}

/// Appends a config version the way the dashboard does (same lock, same NOTIFY). Ok(Err) is a refused value.
pub async fn save(
    app: &App,
    gid: Id<GuildMarker>,
    actor: u64,
    sections: &[&str],
    f: impl FnOnce(&mut Value) -> Result<(), String>,
) -> Result<Result<Value, String>> {
    let guild = gid.get() as i64;
    let mut tx = app.pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(guild)
        .execute(&mut *tx)
        .await?;
    let (version, mut doc): (i32, Value) = sqlx::query_as(
        "select version, config from guild_config where guild_id = $1 order by version desc limit 1",
    )
    .bind(guild)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or((0, json!({})));
    if let Err(e) = f(&mut doc) {
        return Ok(Err(e));
    }
    for s in sections.iter().filter(|s| !doc[**s].is_null()) {
        if let Err(e) = crate::app::check_section(s, &doc[*s]) {
            return Ok(Err(format!("Not saved: {e}")));
        }
    }
    sqlx::query(
        "insert into guild_config (guild_id, version, config, changed_by) values ($1,$2,$3,$4)",
    )
    .bind(guild)
    .bind(version + 1)
    .bind(&doc)
    .bind(actor as i64)
    .execute(&mut *tx)
    .await?;
    sqlx::query("select pg_notify('config_changed', $1)")
        .bind(guild.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    app.forget_cfg(gid.get());
    Ok(Ok(doc))
}

async fn send(
    app: &App,
    msg: &MessageCreate,
    text: Option<&str>,
    embed: Option<Embed>,
    row: Option<Component>,
) -> Result<()> {
    let embeds: Vec<Embed> = embed.into_iter().collect();
    let rows: Vec<Component> = row.into_iter().collect();
    app.http
        .create_message(msg.channel_id)
        .content(text.unwrap_or_default())
        .embeds(&embeds)
        .components(&rows)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

/// Handles `w!<module> ...`; false when `rest` is not a settings command.
pub async fn try_run(app: &App, msg: &MessageCreate, rest: &str) -> Result<bool> {
    let Some(gid) = msg.guild_id else {
        return Ok(false);
    };
    let (word, args) = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .unwrap_or((rest.trim(), ""));
    let word = word.to_lowercase();
    if word == "whitelist" {
        crate::whitelist::run(app, msg, args).await?;
        return Ok(true);
    }
    let Some(m) = module(&word) else {
        return Ok(false);
    };
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, msg.author.id).await?;
    if level == Level::None {
        return Ok(true);
    }
    let op = match parse_op(args) {
        Ok(op) => op,
        Err(e) => {
            send(app, msg, Some(e), None, None).await?;
            return Ok(true);
        }
    };
    let code = match &op {
        Op::Show(page) => {
            let cfg = app.cfg(gid.get()).await?;
            let (embed, pages) = panel(m, &cfg, gid.get(), *page);
            let row = (pages > 1).then(|| {
                page_row(
                    msg.author.id.get(),
                    m.names[0],
                    (*page).min(pages - 1),
                    pages,
                )
            });
            send(app, msg, None, Some(embed), row).await?;
            return Ok(true);
        }
        Op::Set(c, _) | Op::Toggle(c, _) | Op::Add(c, _) | Op::Remove(c, _) => c.clone(),
    };
    let Some(row) = m.rows.iter().find(|r| r.code == code) else {
        send(
            app,
            msg,
            Some(&format!(
                "{} has no setting `{code}`",
                m.title.trim_end_matches(':')
            )),
            None,
            None,
        )
        .await?;
        return Ok(true);
    };
    let section = row.path.split('.').next().unwrap_or_default();
    if !row.path.is_empty() && !may_edit(&level, section) {
        send(
            app,
            msg,
            Some("You may not change this setting"),
            None,
            None,
        )
        .await?;
        return Ok(true);
    }
    let ids = match (row.kind, &op) {
        (
            Kind::Role | Kind::Roles | Kind::Channel | Kind::Channels,
            Op::Set(_, v) | Op::Add(_, v) | Op::Remove(_, v),
        ) => match resolve(app, gid, matches!(row.kind, Kind::Role | Kind::Roles), v).await? {
            Ok(ids) => ids,
            Err(e) => {
                send(app, msg, Some(&e), None, None).await?;
                return Ok(true);
            }
        },
        _ => vec![],
    };
    let sections = [section];
    let saved = save(app, gid, msg.author.id.get(), &sections, |doc| {
        let e = edit_for(row, &op, &ids, get_path(doc, row.path))?;
        apply(doc, row.path, e);
        Ok(())
    });
    let text = match saved.await? {
        Ok(doc) => {
            let cfg = GuildCfg::parse(&doc);
            format!(
                "✅ [{}] {}: {}",
                row.code.to_uppercase(),
                row.name,
                (row.get)(&cfg)
            )
        }
        Err(e) => e,
    };
    send(app, msg, Some(&text), None, None).await?;
    Ok(true)
}

/// The page buttons. Returns false for other interactions.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(PAGE) else {
        return Ok(false);
    };
    let mut it = rest.split(':');
    let invoker = it.next().and_then(|s| s.parse::<u64>().ok());
    let client = app.http.interaction(app.me.cast());
    if invoker != i.author_id().map(|a| a.get()) {
        let r = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                content: Some("Only the person who opened this panel can use it.".into()),
                flags: Some(MessageFlags::EPHEMERAL),
                ..Default::default()
            }),
        };
        client.create_response(i.id, &i.token, &r).await?;
        return Ok(true);
    }
    let (Some(kind), Some(gid)) = (it.next(), i.guild_id) else {
        return Ok(true);
    };
    if kind == "x" {
        if let Some(m) = &i.message {
            app.http.delete_message(m.channel_id, m.id).await.ok();
        }
        let r = InteractionResponse {
            kind: InteractionResponseType::DeferredUpdateMessage,
            data: None,
        };
        client.create_response(i.id, &i.token, &r).await.ok();
        return Ok(true);
    }
    let page: usize = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let Some(m) = module(kind) else {
        return Ok(true);
    };
    let cfg = app.cfg(gid.get()).await?;
    let (embed, pages) = panel(m, &cfg, gid.get(), page);
    let r = InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            components: Some(vec![page_row(
                invoker.unwrap_or_default(),
                kind,
                page.min(pages - 1),
                pages,
            )]),
            ..Default::default()
        }),
    };
    client.create_response(i.id, &i.token, &r).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(m: &'static Module, code: &str) -> &'static Row {
        m.rows.iter().find(|r| r.code == code).unwrap()
    }

    #[test]
    fn parses_wick_syntax() {
        assert_eq!(parse_op(""), Ok(Op::Show(0)));
        assert_eq!(parse_op("2"), Ok(Op::Show(1)));
        assert_eq!(parse_op("3c ?on"), Ok(Op::Toggle("3c".into(), true)));
        assert_eq!(parse_op("1 ?OFF"), Ok(Op::Toggle("1".into(), false)));
        assert_eq!(
            parse_op("15 ?set 3C"),
            Ok(Op::Set("3c".into(), "15".into()))
        );
        assert_eq!(
            parse_op("Muted Role, @x ?add 5"),
            Ok(Op::Add("5".into(), "Muted Role, @x".into()))
        );
        assert_eq!(
            parse_op("<#1> ?remove 8"),
            Ok(Op::Remove("8".into(), "<#1>".into()))
        );
        assert!(parse_op("?set 3").is_err());
        assert!(parse_op("?on").is_err());
        assert!(parse_op("x ?what 3").is_err());
    }

    #[test]
    fn codes_are_unique_and_lettered_rows_have_a_parent() {
        for m in MODULES {
            let mut seen = std::collections::HashSet::new();
            for r in m.rows {
                assert!(seen.insert(r.code), "{} repeats {}", m.title, r.code);
                let lettered = r.code.ends_with(|c: char| c.is_ascii_alphabetic());
                assert!(
                    !lettered
                        || m.rows
                            .iter()
                            .any(|g| r.code.starts_with(g.code) && g.code.len() < r.code.len()),
                    "{} {} has no parent",
                    m.title,
                    r.code
                );
            }
        }
    }

    #[test]
    fn every_writable_row_saves_a_valid_section() {
        let cfg = GuildCfg::default();
        for m in MODULES {
            for r in m.rows {
                let op = match r.kind {
                    Kind::Group | Kind::Dashboard => continue,
                    Kind::Bool | Kind::Switch(..) => Op::Toggle(r.code.into(), true),
                    Kind::Int(min, _) => Op::Set(r.code.into(), min.to_string()),
                    Kind::Num(_) => Op::Set(r.code.into(), "50".into()),
                    Kind::Secs | Kind::Ms => Op::Set(r.code.into(), "10m".into()),
                    Kind::Text => Op::Set(r.code.into(), "t!".into()),
                    Kind::Choice(o) => Op::Set(r.code.into(), o[o.len() - 1].0.into()),
                    Kind::Role | Kind::Channel | Kind::Roles | Kind::Channels => {
                        Op::Set(r.code.into(), "<#5>".into())
                    }
                };
                let mut doc = json!({});
                let e = edit_for(r, &op, &[5], None).unwrap_or_else(|e| panic!("{e}"));
                apply(&mut doc, r.path, e);
                let section = r.path.split('.').next().unwrap();
                crate::app::check_section(section, &doc[section])
                    .unwrap_or_else(|e| panic!("{} {}: {e}", m.title, r.code));
                // The panel getter reads the result back without panicking.
                (r.get)(&GuildCfg::parse(&doc));
            }
        }
        panel(&HFILTERS, &cfg, 1, 9);
    }

    #[test]
    fn edits_round_trip() {
        let mut doc = json!({});
        let r = row(&JOINGATE, "3c");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Set("3c".into(), "15".into()), &[], None).unwrap(),
        );
        assert_eq!(GuildCfg::parse(&doc).joingate.min_age_days, 15);
        assert!(edit_for(r, &Op::Set("3c".into(), "0".into()), &[], None).is_err());
        assert!(edit_for(r, &Op::Toggle("3c".into(), true), &[], None).is_err());

        let r = row(&STATICS, "5");
        apply(&mut doc, r.path, Edit::Add(vec![json!("7"), json!("8")]));
        apply(&mut doc, r.path, Edit::Add(vec![json!(7)]));
        apply(&mut doc, r.path, Edit::Remove(vec![json!("8")]));
        assert_eq!(doc["lockdown"]["main_roles"], json!(["7"]));

        // ?on keeps a custom value, ?off clears it.
        let r = row(&JOINRAID, "6a");
        let on =
            |cur: Option<&Value>| edit_for(r, &Op::Toggle("6a".into(), true), &[], cur).unwrap();
        assert_eq!(on(None), Edit::Put(json!(86_400_000)));
        assert_eq!(on(Some(&json!(5))), Edit::Put(json!(5)));
        apply(&mut doc, r.path, on(None));
        assert_eq!(GuildCfg::parse(&doc).joinraid.unwrap().count, 10);

        let r = row(&HEAT, "4");
        apply(
            &mut doc,
            r.path,
            edit_for(r, &Op::Set("4".into(), "8%".into()), &[], None).unwrap(),
        );
        assert_eq!((r.get)(&GuildCfg::parse(&doc)), "8% per second");
    }

    #[test]
    fn panel_layout_and_pages() {
        let cfg = GuildCfg::default();
        let (e, pages) = panel(&HEAT, &cfg, 1, 0);
        assert_eq!(pages, 1);
        let f = &e.fields;
        assert_eq!(
            (f[0].name.as_str(), f[0].value.as_str()),
            ("[1] Status:", "▸ *`Disabled`*")
        );
        let t = f.iter().find(|f| f.name == "[7] Auto Timeouts:").unwrap();
        assert!(t
            .value
            .starts_with("▸ [A] Status: *`Enabled`*\n▸ [B] Regular Strike Duration: *`1m`*"));
        let (e, pages) = panel(&HFILTERS, &cfg, 1, 1);
        assert_eq!(pages, 4);
        assert_eq!(e.fields[2].name, "[7] Emojis Heat:");
        assert_eq!(e.footer.unwrap().text, "Page: 2/4");
        assert_eq!(
            panel(&HFILTERS, &cfg, 1, 99).0.fields[0].name,
            "[13] Words Blacklist Heat:"
        );
    }

    #[test]
    fn who_may_edit() {
        assert!(!may_edit(&Level::TrustedAdmin, "misc"));
        assert!(may_edit(&Level::ExtraOwner, "antinuke"));
        assert!(may_edit(&Level::TrustedAdmin, "joingate"));
        assert!(!may_edit(&Level::None, "joingate"));
    }
}
