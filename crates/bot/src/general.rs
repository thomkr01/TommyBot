//! General commands: about, commands, help (and `-h` after any command), invite, ping, info,
//! ncases and tshoot.
use std::time::Instant;
use tommybot_logging::emoji::{
    ANTI_NUKE, CHEVRON, CPU, HEAT, HELP, JOIN_GATE, JOIN_RAID, LATENCY, OFF, ON, ONLINE,
    PERSISTENCE, QUARANTINED, RAM, RESOURCES, SERVERS, SIZE, UPTIME, VERIFICATION,
};

use anyhow::Result;
use tommybot_core::permits::Level;
use tommybot_logging::embed::{self, Log, ARROW, BLUE, GREEN, NO, YES};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            application_command::{CommandDataOption, CommandOptionValue},
            InteractionContextType,
        },
    },
    channel::message::{Component, Embed},
    gateway::payload::incoming::MessageCreate,
    guild::Permissions,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    info::{role_info, server_info, user_info},
    metrics,
    settings::{self, send},
    targets, welcome,
};
use twilight_util::builder::command::{CommandBuilder, SubCommandBuilder, UserBuilder};

/// (name, aliases, permit, usage, what, example). Permit "" is anyone, "*" any permit, "owner" Owner
/// or Extra Owner. An empty example means the usage is the example. `ARGS` explains the usage words.
type Cmd = (
    &'static str,
    &'static [&'static str],
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);
const CMDS: &[Cmd] = &[
    ("about", &[], "", "about", "About TommyBot", ""),
    (
        "commands",
        &["command", "cmd", "cmds"],
        "",
        "commands",
        "The commands you can use",
        "",
    ),
    (
        "help",
        &["docs", "h"],
        "",
        "help",
        "Help panel and links",
        "",
    ),
    ("invite", &[], "", "invite", "Add TommyBot to a server", ""),
    ("ping", &[], "", "ping", "Latency, uptime and memory", ""),
    (
        "info",
        &["whois", "i"],
        "",
        "info [MEMBER | ID | @role | -server]",
        "Member, user, role or server info",
        "info @member",
    ),
    (
        "roleinfo",
        &["ri", "rinfo"],
        "*",
        "roleinfo ROLE",
        "Role info",
        "roleinfo @Moderators",
    ),
    (
        "serverinfo",
        &["guildinfo", "sinfo", "ginfo", "si", "gi"],
        "*",
        "serverinfo",
        "Server info",
        "",
    ),
    ("setup", &[], "*", "setup", "Setup panel", ""),
    (
        "security",
        &["sec", "scan"],
        "*",
        "security",
        "Server security check",
        "",
    ),
    ("config", &[], "*", "config", "Config overview", ""),
    (
        "statics",
        &["s"],
        "*",
        "statics [VALUE ?set N | OBJ ?add N]",
        "Quarantine role, main roles, log channels",
        "statics @Muted ?set 1",
    ),
    (
        "heat",
        &[],
        "*",
        "heat [N ?on | VALUE ?set N]",
        "Heat system panel",
        "heat 3 ?on",
    ),
    (
        "hfilters",
        &[],
        "*",
        "hfilters [PAGE]",
        "Heat system filters",
        "hfilters 2",
    ),
    (
        "antinuke",
        &["an"],
        "*",
        "antinuke [N ?on | N ?off]",
        "Anti nuke panel",
        "antinuke 2 ?off",
    ),
    (
        "anpanic",
        &["anp"],
        "*",
        "anpanic [N ?on | OBJ ?add N]",
        "Panic mode panel",
        "anpanic 1 ?on",
    ),
    (
        "antinukerestore",
        &["anr"],
        "*",
        "anr [N ?on | N ?off]",
        "Restore system filters",
        "anr 1 ?on",
    ),
    (
        "joingate",
        &["jg"],
        "*",
        "jg [N ?on | VALUE ?set N]",
        "Join gate panel",
        "jg 2 ?on",
    ),
    (
        "joinraid",
        &["jr"],
        "*",
        "jr [N ?on | VALUE ?set N]",
        "Join raid panel",
        "jr 10 ?set 1",
    ),
    (
        "verification",
        &["v"],
        "*",
        "v [N ?on | VALUE ?set N]",
        "Verification panel",
        "v 1 ?on",
    ),
    (
        "misc",
        &[],
        "*",
        "misc [N ?on | VALUE ?set N]",
        "Miscellaneous panel",
        "misc t! ?set 1",
    ),
    (
        "whitelist",
        &[],
        "*",
        "whitelist TARGET",
        "Interactive whitelist",
        "whitelist @Helpers",
    ),
    (
        "tshoot",
        &[],
        "*",
        "tshoot [lockdown]",
        "Clear caches; `lockdown` resets View/Send on roles",
        "tshoot lockdown",
    ),
    (
        "ban",
        &["b"],
        "ban",
        "ban MEMBERS [TIME] [?r REASON] [?dm] [?s] [?p]",
        "Ban, softban with ?s",
        "ban @member 7d ?r spamming links ?dm",
    ),
    (
        "hackban",
        &["hb"],
        "ban",
        "hackban IDS [?r REASON] [?dm]",
        "Ban someone not in the server",
        "hackban 123456789012345678 ?r raid bot",
    ),
    (
        "unban",
        &["ub"],
        "ban",
        "unban MEMBERS [?r REASON]",
        "Lift a ban",
        "unban 123456789012345678 ?r appeal accepted",
    ),
    (
        "kick",
        &["k"],
        "kick",
        "kick MEMBERS [?r REASON] [?dm]",
        "Kick",
        "kick @member ?r ads ?dm",
    ),
    (
        "timeout",
        &["mute", "m", "to", "t"],
        "timeout",
        "timeout MEMBERS [TIME] [?r REASON]",
        "Time out",
        "timeout @member 10m ?r spam",
    ),
    (
        "untimeout",
        &["unmute", "um"],
        "timeout",
        "untimeout MEMBER",
        "Lift a timeout",
        "untimeout @member",
    ),
    (
        "warn",
        &["wa"],
        "warn",
        "warn MEMBERS [?r REASON]",
        "Warn",
        "warn @member ?r rude",
    ),
    (
        "unwarn",
        &[],
        "warn",
        "unwarn CASE",
        "Remove a warn",
        "unwarn 4151",
    ),
    (
        "warns",
        &[],
        "warn",
        "warns MEMBER",
        "A member's warns",
        "warns @member",
    ),
    (
        "clearwarns",
        &[],
        "warn",
        "clearwarns MEMBER",
        "Remove all warns",
        "clearwarns @member",
    ),
    (
        "quarantine",
        &["q"],
        "quarantine",
        "quarantine MEMBER [?r REASON]",
        "Quarantine",
        "quarantine @member ?r compromised account",
    ),
    (
        "unquarantine",
        &["uq", "restore", "r"],
        "quarantine",
        "unquarantine MEMBER",
        "Release from quarantine",
        "unquarantine @member",
    ),
    (
        "purge",
        &["clear", "sweep", "p", "c", "botclear", "bc"],
        "purge",
        "purge [COUNT] [MEMBER]",
        "Delete messages",
        "purge 50 @member",
    ),
    (
        "slowmode",
        &["sm"],
        "slowmode",
        "slowmode TIME | off",
        "Channel slowmode",
        "slowmode 10s",
    ),
    (
        "lock",
        &["lockdown", "ld"],
        "lockdown",
        "lock [?t TYPE] [TIME]",
        "Lock the server or channel",
        "lock ?t server 30m",
    ),
    (
        "unlock",
        &["ul"],
        "lockdown",
        "unlock [?t TYPE]",
        "Unlock",
        "unlock ?t server",
    ),
    (
        "sanitize",
        &["san", "dehoist"],
        "sanitize",
        "sanitize MEMBER",
        "Clean a nickname",
        "sanitize @member",
    ),
    (
        "verify",
        &[],
        "verify",
        "verify MEMBER",
        "Verify a member by hand",
        "verify @member",
    ),
    (
        "cases",
        &["case"],
        "cases",
        "cases [CASE | MEMBER] [?t TYPE]",
        "View cases",
        "cases @member ?t ban",
    ),
    (
        "modcases",
        &[],
        "cases",
        "modcases MODERATOR [?t TYPE]",
        "Cases made by a moderator",
        "modcases @moderator ?t warn",
    ),
    (
        "editc",
        &["editreason"],
        "cases",
        "editc CASE REASON",
        "Change a case's reason",
        "editc 4151 posted scam links",
    ),
    (
        "closecase",
        &["caseoff", "opencase", "caseon"],
        "cases",
        "closecase CASE",
        "Switch a case off or on",
        "closecase 4151",
    ),
    (
        "ncases",
        &[],
        "cases",
        "ncases [ID]",
        "Anti-nuke cases",
        "ncases n7Kd2x",
    ),
    (
        "backups",
        &["backup"],
        "backups",
        "backups [take | load ID ?confirm | delete ID | clear]",
        "Server backups",
        "backups load 3 ?confirm",
    ),
    (
        "notes",
        &["note"],
        "notes_view",
        "notes MEMBER | note add MEMBER ?r TEXT",
        "Staff notes",
        "note add @member ?r warned in DMs",
    ),
    (
        "reply",
        &["areply", "ar"],
        "tickets",
        "reply MESSAGE",
        "Reply to the ticket (areply: anonymously)",
        "reply Thanks, we are looking into it.",
    ),
    (
        "close",
        &["aclose", "ac"],
        "tickets",
        "close [REASON]",
        "Close the ticket (aclose: anonymously)",
        "close Solved, thanks",
    ),
    (
        "closeall",
        &["acloseall"],
        "tickets",
        "closeall [REASON]",
        "Close every ticket",
        "closeall Server maintenance",
    ),
    (
        "blacklist",
        &["block"],
        "tickets",
        "blacklist [USERS]",
        "Stop users from opening tickets",
        "blacklist @member",
    ),
    (
        "unblock",
        &[],
        "tickets",
        "unblock [USERS]",
        "Let users open tickets again",
        "unblock @member",
    ),
    (
        "blacklistclear",
        &[],
        "tickets",
        "blacklistclear",
        "Empty the ticket blacklist",
        "",
    ),
    (
        "viewblacklist",
        &[],
        "tickets",
        "viewblacklist",
        "The ticket blacklist",
        "",
    ),
    (
        "snippet",
        &["asnippet", "as"],
        "tickets",
        "snippet NAME",
        "Send a saved reply (asnippet: anonymously)",
        "snippet rules",
    ),
    (
        "snippetadd",
        &[],
        "tickets",
        "snippetadd NAME CONTENT",
        "Save a reply; {username} {userid} {usermention}",
        "snippetadd rules Hi {usermention}, please read #rules.",
    ),
    (
        "snippetremove",
        &[],
        "tickets",
        "snippetremove NAME",
        "Remove a saved reply",
        "snippetremove rules",
    ),
    (
        "snippetclear",
        &[],
        "tickets",
        "snippetclear",
        "Remove every saved reply",
        "",
    ),
    (
        "viewsnippet",
        &["viewsnippets", "snippetlist", "vs"],
        "tickets",
        "viewsnippet [NAME]",
        "Saved replies",
        "viewsnippet rules",
    ),
    (
        "ticketsetup",
        &[],
        "tickets",
        "ticketsetup",
        "Create the ticket category and log",
        "",
    ),
    (
        "category",
        &[],
        "tickets",
        "category [NAME]",
        "Create the ticket category again",
        "category Tickets",
    ),
    (
        "accessrole",
        &["modrole", "supportrole"],
        "tickets",
        "accessrole [ROLES]",
        "Roles that handle tickets",
        "accessrole @Support",
    ),
    (
        "pingrole",
        &["mentionrole"],
        "tickets",
        "pingrole [ROLES | everyone | here]",
        "Pinged on a new ticket",
        "pingrole @Support",
    ),
    (
        "logging",
        &["logs"],
        "tickets",
        "logging [CHANNEL]",
        "Ticket log on or off",
        "logging #ticket-log",
    ),
    (
        "loggingplus",
        &["advancedlogging", "advancedlogs"],
        "tickets",
        "loggingplus",
        "Full conversation in the ticket log",
        "",
    ),
    (
        "commandonly",
        &["commandrequired"],
        "tickets",
        "commandonly",
        "Only reply sends to the member",
        "",
    ),
    (
        "anonymous",
        &[],
        "tickets",
        "anonymous",
        "Anonymous replies by default",
        "",
    ),
    (
        "greetingmessage",
        &["welcomemessage", "greetmessage"],
        "tickets",
        "greetingmessage [TEXT]",
        "Sent when a ticket opens",
        "greetingmessage Hi {username}, staff will answer soon.",
    ),
    (
        "closingmessage",
        &["goodbyemessage", "closemessage"],
        "tickets",
        "closingmessage [TEXT]",
        "Sent when a ticket closes",
        "closingmessage Thanks {username}, this ticket is closed.",
    ),
    (
        "toggle",
        &["enable", "disable"],
        "tickets",
        "toggle [REASON]",
        "Turn new tickets off or on",
        "toggle Back on Monday",
    ),
    (
        "viewconfig",
        &[],
        "tickets",
        "viewconfig",
        "Ticket settings",
        "",
    ),
];

/// What each word in a `CMDS` usage means; shown by `COMMAND -h` and on the dashboard's Commands page.
const ARGS: &[(&str, &str)] = &[
    ("MEMBER", "A member: @mention, user id or name"),
    ("MEMBERS", "One or more members: @mentions, user ids or names (separate names with spaces by commas)"),
    ("MODERATOR", "A staff member: @mention or user id"),
    ("USERS", "One or more users: @mentions or user ids; inside a ticket, empty means its member"),
    ("IDS", "One or more user ids, also of people who are not in the server"),
    ("ID", "An id: the user for info, the anti-nuke case for ncases, the backup for backups"),
    ("TARGET", "What to whitelist: @member, @role, #channel, category id or webhook id"),
    ("ROLE", "A role: @mention, id or name"),
    ("ROLES", "One or more roles: @mentions or ids; empty clears the list"),
    ("@role", "A role mention, to show role info"),
    ("CHANNEL", "A channel: #mention or id; empty turns the log off"),
    ("-server", "Show this server instead of a member"),
    ("TIME", "A duration: a number then s, m, h, d or w (30s, 10m, 2h, 7d, 1w). ban: unban after it (none = permanent); timeout: its length (none = the default from settings); lock: unlock after it; slowmode: the delay between messages"),
    ("REASON", "Free text, saved in the case and the log"),
    ("?r", "Starts the reason (or note text): every word after it up to the next ?flag"),
    ("?dm", "Also tell the member by DM"),
    ("?s", "Softban: ban and unban right away, to delete their recent messages"),
    ("?p", "Short one-line reply instead of the result embed"),
    ("?t", "Starts the type, see TYPE"),
    ("TYPE", "lock/unlock: server (sc), blind, role (r), channel (c), kick (k), ban (b) or all (s); cases: b, ub, k, timeout, wa, q or uq"),
    ("CASE", "A case number, e.g. 4151"),
    ("COUNT", "How many messages to delete, up to 1000"),
    ("N", "The number shown next to a setting in the panel"),
    ("VALUE", "The new value for setting N"),
    ("OBJ", "A role or channel to add to list N"),
    ("?on", "Turn setting N on"),
    ("?off", "Turn setting N off"),
    ("?set", "Set setting N to VALUE"),
    ("?add", "Add OBJ to list N"),
    ("?confirm", "Apply the backup; without it you get a preview"),
    ("PAGE", "The panel page number"),
    ("NAME", "A name: the snippet, or the new ticket category"),
    ("CONTENT", "The saved reply text; message variables are filled in"),
    ("MESSAGE", "The reply sent to the member"),
    ("TEXT", "The message text, message variables are filled in; empty turns it off. notes: the note text"),
];

/// The `ARGS` entries for the words in `usage`, in order of first use.
fn args(usage: &str) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&str, &str)> = vec![];
    for w in usage.split([' ', '[', ']', '|']) {
        if let Some(a) = ARGS.iter().find(|a| a.0 == w).filter(|a| !v.contains(a)) {
            v.push(*a);
        }
    }
    v
}

/// Who may run a command, as the `-h` help and the Commands page say it.
fn who(perm: &str) -> String {
    match perm {
        "" => "Anyone".into(),
        "*" => "Anyone with a permit".into(),
        "owner" => "Owner or Extra Owner".into(),
        p => {
            let cat = tommybot_core::permits::CATALOG;
            match cat.iter().flat_map(|s| s.items).find(|d| d.key == p) {
                Some(d) => format!("Permit: {}", d.label),
                // `lockdown` is any permit of the Lockdown section.
                None => format!(
                    "Any {} permit",
                    cat.iter().find(|s| s.id == p).map_or(p, |s| s.title)
                ),
            }
        }
    }
}

const CATEGORIES: [(&str, &[&str]); 4] = [
    (
        "General Commands:",
        &[
            "about",
            "commands",
            "help",
            "invite",
            "ping",
            "info",
            "roleinfo",
            "serverinfo",
            "security",
        ],
    ),
    (
        "Settings Commands:",
        &[
            "setup",
            "config",
            "statics",
            "heat",
            "hfilters",
            "antinuke",
            "anpanic",
            "antinukerestore",
            "joingate",
            "joinraid",
            "verification",
            "misc",
            "whitelist",
            "tshoot",
        ],
    ),
    (
        "Moderation Commands:",
        &[
            "ban",
            "hackban",
            "unban",
            "kick",
            "timeout",
            "untimeout",
            "warn",
            "unwarn",
            "warns",
            "clearwarns",
            "quarantine",
            "unquarantine",
            "purge",
            "slowmode",
            "lock",
            "unlock",
            "sanitize",
            "verify",
            "cases",
            "modcases",
            "editc",
            "closecase",
            "ncases",
            "notes",
            "backups",
        ],
    ),
    (
        "Ticket Commands:",
        &[
            "reply",
            "close",
            "closeall",
            "blacklist",
            "unblock",
            "blacklistclear",
            "viewblacklist",
            "snippet",
            "snippetadd",
            "snippetremove",
            "snippetclear",
            "viewsnippet",
            "ticketsetup",
            "category",
            "accessrole",
            "pingrole",
            "logging",
            "loggingplus",
            "commandonly",
            "anonymous",
            "greetingmessage",
            "closingmessage",
            "toggle",
            "viewconfig",
        ],
    ),
];

fn allowed(level: &Level, perm: &str) -> bool {
    match perm {
        "" => true,
        "*" => *level != Level::None,
        "owner" => matches!(level, Level::Owner | Level::ExtraOwner),
        p => level.has(p),
    }
}

/// `COMMAND -h`: name, who may run it, category, what it does, usage, each argument, the message
/// variables where text is filled in, an example and the aliases.
fn help_embed(c: &Cmd, pfx: &str, asker: &str) -> Embed {
    let cat = CATEGORIES
        .iter()
        .find(|k| k.1.contains(&c.0))
        .map_or("general", |k| k.0.trim_end_matches(" Commands:"));
    let mut e = Log::new(&format!("{asker}, here's some help!"), BLUE);
    if let Some(d) = dashboard() {
        e = e.text(&format!(
            "{ARROW} [Every command explained]({}/#/commands)",
            d.trim_end_matches('/')
        ));
    }
    e = e
        .text("**Command Details:**")
        .text(&format!("{ARROW} **Name:** `{}`", c.0))
        .text(&format!("{ARROW} **Permit:** `{}`", who(c.2)))
        .text(&format!("{ARROW} **Category:** `{}`", cat.to_lowercase()))
        .text(&format!("{ARROW} **Description:** {}", c.4))
        .text(&format!("{ARROW} **Usage:** `{pfx}{}`", c.3));
    let a = args(c.3);
    if !a.is_empty() {
        e = e.text("**Variables:**");
        for (w, what) in &a {
            e = e.text(&format!("{CHEVRON} `{w}` {what}"));
        }
    }
    if a.iter()
        .any(|(w, _)| matches!(*w, "TEXT" | "CONTENT" | "MESSAGE"))
    {
        let tags: Vec<String> = crate::tickets::TAGS
            .iter()
            .map(|(k, w)| format!("`{k}` {w}"))
            .collect();
        e = e.text(&format!("**Message variables:** {}", tags.join(", ")));
    }
    if !c.5.is_empty() {
        e = e.text(&format!("**Example:** `{pfx}{}`", c.5));
    }
    if !c.1.is_empty() {
        e = e.text(&format!("**Aliases:** {}", c.1.join(", ")));
    }
    e.build()
}

/// Every command with its help and slash form, published at startup for the dashboard's public
/// Commands page (the dashboard cannot use this crate).
pub fn catalog() -> serde_json::Value {
    use twilight_model::application::command::CommandOptionType as T;
    let defs = crate::slash::definitions();
    let opts = |o: &[twilight_model::application::command::CommandOption]| {
        o.iter()
            .map(|o| match o.required {
                Some(true) => format!(" {}", o.name),
                _ => format!(" [{}]", o.name),
            })
            .collect::<String>()
    };
    // `/ban add user [reason]`, one line per subcommand.
    let forms = |d: &Command| -> Vec<String> {
        match d.options.first().map(|o| o.kind) {
            Some(T::SubCommand) => d
                .options
                .iter()
                .map(|s| {
                    format!(
                        "/{} {}{}",
                        d.name,
                        s.name,
                        opts(s.options.as_deref().unwrap_or(&[]))
                    )
                })
                .collect(),
            _ => vec![format!("/{}{}", d.name, opts(&d.options))],
        }
    };
    let cat = |n: &str| {
        CATEGORIES
            .iter()
            .find(|c| c.1.contains(&n))
            .map_or("", |c| c.0.trim_end_matches(':'))
    };
    let commands: Vec<_> = CMDS
        .iter()
        .map(|c| {
            let slash: Vec<String> = defs
                .iter()
                .filter(|d| d.kind == CommandType::ChatInput && (d.name == c.0 || c.1.contains(&d.name.as_str())))
                .flat_map(forms)
                .collect();
            serde_json::json!({
                "name": c.0, "aliases": c.1, "category": cat(c.0), "usage": c.3, "what": c.4,
                "example": if c.5.is_empty() { c.3 } else { c.5 }, "who": who(c.2),
                "args": args(c.3).iter().map(|(a, w)| serde_json::json!([a, w])).collect::<Vec<_>>(),
                "slash": slash,
            })
        })
        .collect();
    // Slash commands and right-click apps with no prefix command of the same name.
    let apps: Vec<_> = defs
        .iter()
        .filter(|d| d.kind != CommandType::ChatInput || lookup(&d.name).is_none())
        .map(|d| match d.kind {
            CommandType::ChatInput => {
                serde_json::json!({"name": forms(d).join("\n"), "what": d.description})
            }
            CommandType::User => {
                serde_json::json!({"name": d.name, "what": "Right-click a member > Apps"})
            }
            _ => serde_json::json!({"name": d.name, "what": "Right-click a message > Apps"}),
        })
        .collect();
    let vars: Vec<_> = crate::tickets::TAGS
        .iter()
        .map(|(k, w)| serde_json::json!([k, w]))
        .collect();
    serde_json::json!({
        "prefix": tommybot_core::sections::MiscCfg::default().prefix,
        "commands": commands, "apps": apps, "variables": vars, "variables_in": crate::tickets::TAGS_IN,
    })
}

fn lookup(word: &str) -> Option<&'static Cmd> {
    CMDS.iter().find(|c| c.0 == word || c.1.contains(&word))
}

/// Average CPU use of this process since boot, from /proc (clock ticks at 100 Hz).
fn cpu_percent(up_secs: i64) -> String {
    let ticks = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            // The command name is in parentheses and may hold spaces: count fields after it.
            let f: Vec<&str> = s.rsplit_once(')')?.1.split_whitespace().collect();
            Some(f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?)
        });
    match ticks {
        Some(t) if up_secs > 0 => format!("{:.2}%", t / 100.0 / up_secs as f64 * 100.0),
        _ => "unknown".into(),
    }
}

/// `about`: the pitch, what it protects, and the version lines ("Nerdy Stats").
pub fn about_page(page: usize) -> (Embed, usize) {
    const PAGES: usize = 3;
    let page = page.min(PAGES - 1);
    let e = match page {
        0 => Log::new("About TommyBot:", BLUE)
            .text(&format!("{ARROW} TommyBot is a Discord security and moderation bot, written in Rust to be fast and light. It stops nukes, raids and spam on its own, undoes the damage, and gives your staff moderation commands that are quick to use."))
            .text(&format!("{ARROW} Every feature is free. There is no premium tier.")),
        1 => Log::new("About TommyBot [What it protects]:", BLUE)
            .text(&format!("{ANTI_NUKE} **Anti-nuke:** quarantines staff who mass ban, kick or delete, then reverts what they did"))
            .text(&format!("{HEAT} **Heat automod:** spam, links, scams, mentions, words and more add heat until the member is punished"))
            .text(&format!("{JOIN_GATE} **Join gate:** filters bots, young accounts and suspicious names the moment they join"))
            .text(&format!("{JOIN_RAID} **Join raid:** spots join waves and kicks or bans the raiders"))
            .text(&format!("{VERIFICATION} **Verification:** captcha, web or button checks before members get in"))
            .text(&format!("{QUARANTINED} **Lockdown, tickets, appeals, backups and logs:** all in the dashboard")),
        _ => {
            let up = now_secs() - metrics::STARTED.load(std::sync::atomic::Ordering::Relaxed) as i64;
            Log::new("About TommyBot [Nerdy Stats]:", BLUE)
                .line("Version", &format!("`{}`", env!("CARGO_PKG_VERSION")))
                .line("Uptime", &format!("`{}`", embed::duration(up)))
                .line("Language", "`Rust`")
                .line("Commands", &format!("`{}`", CMDS.len()))
                .line("Price", "Everything is free")
        }
    };
    (
        e.footer(&format!("Page: {}/{PAGES}", page + 1)).build(),
        PAGES,
    )
}

/// One category per page, only the commands `level` may use.
pub fn commands_page(level: &Level, prefix: &str, page: usize) -> (Embed, usize) {
    let page = page.min(CATEGORIES.len() - 1);
    let (title, names) = CATEGORIES[page];
    let mut e = Log::new(title, BLUE);
    for name in names
        .iter()
        .filter_map(|n| lookup(n))
        .filter(|c| allowed(level, c.2))
    {
        e = e.text(&format!("{CHEVRON} `{prefix}{}` {}", name.0, name.4));
    }
    let e = e
        .text("**Need more?**")
        .text(&format!(
            "{ARROW} Only commands you can use are displayed! With a higher permit you see more."
        ))
        .text(&format!(
            "{ARROW} For details on one command, add `-h` after it, for example: `{prefix}ban -h`"
        ))
        .footer(&format!("Page: {}/{}", page + 1, CATEGORIES.len()));
    (e.build(), CATEGORIES.len())
}

pub(crate) fn dashboard() -> Option<String> {
    std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

fn invite_url(app: &App) -> String {
    format!(
        "https://discord.com/oauth2/authorize?client_id={}&scope=bot+applications.commands&permissions=8",
        app.me
    )
}

fn links(app: &App) -> Vec<Component> {
    let mut b = vec![welcome::link("Add TommyBot", invite_url(app))];
    if let Some(d) = dashboard() {
        b.push(welcome::link("Dashboard", d));
    }
    if let Some(s) = std::env::var("SUPPORT_URL").ok().filter(|u| !u.is_empty()) {
        b.push(welcome::link("Support", s));
    }
    vec![Component::ActionRow(
        twilight_model::channel::message::component::ActionRow {
            id: None,
            components: b,
        },
    )]
}

/// `ncases`: the latest anti-nuke cases, or one case by id.
async fn ncases(app: &App, gid: Id<GuildMarker>, id: Option<&str>) -> Result<Embed> {
    let rows: Vec<(String, serde_json::Value, i64)> = sqlx::query_as(
        "SELECT case_id, data, EXTRACT(EPOCH FROM created_at)::bigint FROM nuke_incidents
         WHERE guild_id=$1 AND ($2::text IS NULL OR case_id=$2) ORDER BY created_at DESC LIMIT 10",
    )
    .bind(gid.get() as i64)
    .bind(id)
    .fetch_all(&app.pool)
    .await?;
    let mut e = Log::new("Nuke Cases:", BLUE);
    if rows.is_empty() {
        return Ok(e.text("No nuke cases").build());
    }
    for (case, data, at) in rows {
        let quarantined = data["quarantined"].as_array().map_or(0, Vec::len);
        let reverted = data["reverted"].as_array().cloned().unwrap_or_default();
        if id.is_none() {
            e = e.text(&format!(
                "`{case}` <t:{at}:R>: `{quarantined}` quarantined, `{}` reverted",
                reverted.len()
            ));
            continue;
        }
        // Each reverted step is `{"DeleteChannel": {...}}` or a bare name; count them per kind.
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        for r in &reverted {
            let k = r
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .or_else(|| r.as_str().map(String::from))
                .unwrap_or_default();
            *kinds.entry(k).or_default() += 1;
        }
        let who: Vec<_> = data["quarantined"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_u64())
            .map(embed::user)
            .collect();
        e = e
            .line("Case", &format!("`{case}` <t:{at}:R>"))
            .field(
                "Quarantined:",
                &if who.is_empty() {
                    NO.into()
                } else {
                    who.join("\n")
                },
            )
            .field(
                "Reverted:",
                &if kinds.is_empty() {
                    NO.into()
                } else {
                    kinds
                        .iter()
                        .map(|(k, n)| format!("{k}: `{n}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            );
    }
    Ok(e.build())
}

/// Drops this guild's caches so the next event reloads them.
fn tshoot(app: &App, gid: Id<GuildMarker>) -> Embed {
    let started = Instant::now();
    let g = gid.get();
    app.invalidate_guild(gid);
    app.forget_cfg(g);
    app.heat.lock().unwrap().retain(|(guild, _), _| *guild != g);
    app.nuke_heat
        .lock()
        .unwrap()
        .retain(|(guild, _), _| *guild != g);
    app.pings
        .lock()
        .unwrap()
        .retain(|(guild, _), _| *guild != g);
    app.mentions.lock().unwrap().remove(&g);
    let lines = [
        "Heat Panic Decache",
        "Nuke Panic Decache",
        "Members Decache",
        "Server Decache",
    ];
    tshoot_embed(&lines.map(|l| format!("{YES} {l}")).join("\n"), started)
}

/// Troubleshooting embed: result lines, then the finished block.
fn tshoot_embed(lines: &str, started: Instant) -> Embed {
    Log::new("TommyBot Troubleshooting:", BLUE)
        .text(lines)
        .text("Troubleshooting Finished!")
        .text(&format!(
            "*TommyBot has made some changes in `{:.2}ms`. If issues are still happening, ask in the support server.*",
            started.elapsed().as_secs_f64() * 1000.0
        ))
        .build()
}

/// `tshoot lockdown`: main roles (or @everyone when there are none) get View Channel and Send Messages,
/// every other role below the bot loses them.
async fn tshoot_lockdown(app: &App, gid: Id<GuildMarker>) -> Result<Embed> {
    let started = Instant::now();
    const BITS: u64 = (1 << 10) | (1 << 11);
    let cfg = app.cfg(gid.get()).await?;
    let info = app.guild_info(gid).await?;
    let main: Vec<u64> = match cfg.lockdown.main_roles.as_slice() {
        [] => vec![gid.get()],
        r => r.iter().map(|r| r.get()).collect(),
    };
    let mut tweaked = 0;
    for r in app.http.roles(gid).await?.models().await? {
        if r.managed || r.position >= info.bot_top {
            continue;
        }
        let p = r.permissions.bits();
        let want = if main.contains(&r.id.get()) {
            p | BITS
        } else {
            p & !BITS
        };
        if want != p {
            app.http
                .update_role(gid, r.id)
                .permissions(Permissions::from_bits_truncate(want))
                .await?;
            tweaked += 1;
        }
    }
    app.invalidate_guild(gid);
    Ok(tshoot_embed(
        &match tweaked {
            0 => format!("{YES} No roles were found to not be complying with the lockdown system. You are clear!"),
            n => format!("{YES} {n} roles were tweaked to comply with the lockdown system!"),
        },
        started,
    ))
}

/// Handles the commands above and `COMMAND -h`; false for anything else.
pub async fn try_run(app: &App, msg: &MessageCreate, rest: &str) -> Result<bool> {
    let Some(gid) = msg.guild_id else {
        return Ok(false);
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    let Some(first) = words.first().map(|w| w.to_lowercase()) else {
        return Ok(false);
    };
    let cfg = app.cfg(gid.get()).await?;
    let pfx = cfg.misc.prefix.clone();
    if words.last() == Some(&"-h") {
        let Some(c) = lookup(&first) else {
            return Ok(false);
        };
        let e = help_embed(c, &pfx, &msg.author.name);
        send(app, msg, "", Some(e), vec![]).await?;
        return Ok(true);
    }
    let Some(&(name, _, perm, ..)) = lookup(&first) else {
        return Ok(false);
    };
    if !matches!(
        name,
        "about"
            | "commands"
            | "help"
            | "invite"
            | "ping"
            | "info"
            | "roleinfo"
            | "serverinfo"
            | "config"
            | "ncases"
            | "tshoot"
    ) {
        return Ok(false);
    }
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, msg.author.id).await?;
    if !allowed(&level, perm) || (level == Level::None && cfg.misc.ignore_no_permit) {
        return Ok(true);
    }
    if level == Level::None && !app.cooled_down(gid.get(), msg.author.id.get()) {
        return Ok(true);
    }
    let (text, embed, rows) = answer(app, gid, msg.author.id, &level, name, &words).await?;
    send(app, msg, &text, embed, rows).await?;
    Ok(true)
}

/// `/about`, `/ping`, `/help`, `/invite`, `/commands`, `/info user|server` as the words of the prefix command.
pub fn slash_words(name: &str, o: &[CommandDataOption]) -> Option<Vec<String>> {
    if !matches!(
        name,
        "about" | "ping" | "help" | "invite" | "commands" | "info" | "security"
    ) {
        return None;
    }
    let mut w = vec![name.to_string()];
    if let Some(CommandDataOption {
        name: sub,
        value: CommandOptionValue::SubCommand(so),
    }) = o.first()
    {
        match (sub.as_str(), crate::slash::user(so, "user")) {
            ("server", _) => w.push("-server".into()),
            (_, Some(u)) => w.push(format!("<@{u}>")),
            _ => {}
        }
    }
    Some(w)
}

pub fn definitions() -> Vec<Command> {
    let chat = |n: &str, d: &str| {
        CommandBuilder::new(n, d, CommandType::ChatInput).contexts([InteractionContextType::Guild])
    };
    let mut defs: Vec<Command> = [
        ("about", "About TommyBot"),
        ("ping", "Latency, uptime and memory"),
        ("help", "Help panel and links"),
        ("invite", "Add TommyBot to a server"),
        ("commands", "The commands you can use"),
        ("security", "Server security check"),
    ]
    .map(|(n, d)| chat(n, d).build())
    .into();
    defs.push(
        chat("info", "Info about a member, user or the server")
            .option(
                SubCommandBuilder::new("user", "A member or any user")
                    .option(UserBuilder::new("user", "Default: you")),
            )
            .option(SubCommandBuilder::new("server", "This server"))
            .build(),
    );
    defs
}

/// Slash entry: the answer of a general command as (text, embeds, button row).
pub async fn slash(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: Id<UserMarker>,
    words: Vec<String>,
) -> Result<(String, Vec<Embed>, Option<Component>)> {
    let Some(&(name, _, perm, ..)) = lookup(&words[0]) else {
        return Ok(("Unknown command".into(), vec![], None));
    };
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, invoker).await?;
    if !allowed(&level, perm) {
        return Ok(("You lack the permit for this command".into(), vec![], None));
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let (text, embed, rows) = answer(app, gid, invoker, &level, name, &words).await?;
    Ok((text, embed.into_iter().collect(), rows.into_iter().next()))
}

/// The reply of command `name`; `words` are the typed words, the command first.
async fn answer(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: Id<UserMarker>,
    level: &Level,
    name: &str,
    words: &[&str],
) -> Result<(String, Option<Embed>, Vec<Component>)> {
    let pfx = app.cfg(gid.get()).await?.misc.prefix.clone();
    let arg = words.get(1).copied();
    let text = |t: &str| Ok((t.to_string(), None, vec![]));
    let embed = |e| Ok((String::new(), Some(e), vec![]));
    match name {
        "about" => {
            let (e, pages) = about_page(0);
            let row = settings::page_row(invoker.get(), "about", 0, pages);
            Ok((String::new(), Some(e), vec![row]))
        }
        "commands" => {
            let (e, pages) = commands_page(level, &pfx, 0);
            Ok((
                String::new(),
                Some(e),
                vec![settings::page_row(invoker.get(), "cmds", 0, pages)],
            ))
        }
        "help" => {
            let mut e = Log::new("TommyBot's Help Panel:", BLUE)
                .text(&format!("{HELP} **General Help:**"))
                .text(&format!(
                    "{ARROW} To view the commands you can use, do: `{pfx}commands`"
                ))
                .text(&format!(
                    "{ARROW} To see how one command works, do: `{pfx}COMMAND -h`"
                ));
            if let Some(d) = dashboard() {
                e = e.text(&format!(
                    "{ARROW} Every command explained: {}/#/commands",
                    d.trim_end_matches('/')
                ));
            }
            let e = e
                .text(&format!("{ARROW} To set things up, do: `{pfx}setup`, then `{pfx}heat`, `{pfx}an`, `{pfx}jg`..."))
                .text(&format!("{ARROW} Quick Troubleshooting: `{pfx}tshoot`"))
                .build();
            Ok((String::new(), Some(e), links(app)))
        }
        "invite" => {
            let e = Log::new("Invite TommyBot!", BLUE)
                .text("Thank you for your interest in using TommyBot, get ready to maximize the security for your server and have the best moderation tools at your service!")
                .build();
            Ok((String::new(), Some(e), links(app)))
        }
        "ping" => {
            let t = Instant::now();
            app.http.current_user().await?;
            let rest_ms = t.elapsed().as_millis();
            let gw = metrics::LATENCY_MS.load(std::sync::atomic::Ordering::Relaxed);
            let up =
                now_secs() - metrics::STARTED.load(std::sync::atomic::Ordering::Relaxed) as i64;
            let ram = std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map_or("unknown".into(), |pages| {
                    format!("{} MB", pages * 4096 / 1_048_576)
                });
            let servers = app
                .http
                .current_user_application()
                .await?
                .model()
                .await?
                .approximate_guild_count
                .map_or("unknown".into(), |n| n.to_string());
            let gw = if gw == 0 {
                "measuring".into()
            } else {
                format!("{gw}ms")
            };
            // One process is one shard.
            let shard = format!(
                "{LATENCY} **Latency:** `{gw}` (API `{rest_ms}ms`)\n{UPTIME} **Uptime:** `{}`\n{RESOURCES} **Resources:**\n\u{2003}{RAM} RAM: `{ram}`\n\u{2003}{CPU} CPU: `{}`\n{SIZE} **Size:**\n\u{2003}{SERVERS} Servers: `{servers}`",
                embed::duration(up),
                cpu_percent(up),
            );
            embed(
                Log::new("TommyBot Status:", GREEN)
                    .inline(&format!("{ONLINE} Shard [0 -Current-]:"), &shard)
                    .build(),
            )
        }
        "config" => {
            let cfg = app.cfg(gid.get()).await?;
            let g = app.http.guild(gid).await?.model().await?;
            let role = cfg
                .antinuke
                .quarantine_role
                .map_or(NO.to_string(), |r| embed::role(r.get()));
            let systems = [
                (ANTI_NUKE, "Anti Nuke", cfg.antinuke.enabled),
                (HEAT, "Heat System", cfg.automod.enabled),
                (VERIFICATION, "Verification", cfg.verification.enabled),
                (JOIN_GATE, "Join Gate", cfg.joingate.enabled),
                (
                    JOIN_RAID,
                    "Join Raid",
                    cfg.joinraid.as_ref().is_some_and(|r| r.enabled),
                ),
                (
                    PERSISTENCE,
                    "Persistence",
                    cfg.misc.enforce_quarantine_perms,
                ),
            ];
            let mut log = Log::new(&format!("{}'s Config Overview", g.name), BLUE)
                .section("Essential Roles")
                .text(&format!("{QUARANTINED} **Quarantine Role:** {role}"))
                .section("Major Systems");
            for (icon, name, on) in systems {
                log = log.text(&format!("{icon} **{name}:** {}", if on { ON } else { OFF }));
            }
            embed(log.build())
        }
        "serverinfo" => server_info(app, gid, invoker).await,
        "security" => embed(crate::info::security_page(app, gid).await?),
        "roleinfo" | "info" => {
            let staff = *level != Level::None;
            let e = match arg {
                Some("-server" | "-guild" | "-g") if staff => {
                    return server_info(app, gid, invoker).await
                }
                _ if name == "roleinfo" || arg.is_some_and(|a| a.starts_with("<@&")) => {
                    let id = arg
                        .and_then(|a| a.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok());
                    match id {
                        Some(id) if staff => role_info(app, gid, id).await?,
                        _ => None,
                    }
                }
                None => Some(user_info(app, gid, invoker).await?),
                Some(a) => {
                    let digits = a.trim_matches(|c: char| !c.is_ascii_digit());
                    match digits.parse::<u64>() {
                        Ok(id) if a.starts_with('<') || a == digits => {
                            match role_info(app, gid, id).await? {
                                Some(r) if staff => Some(r),
                                _ => Some(user_info(app, gid, Id::new(id)).await?),
                            }
                        }
                        _ => match targets::resolve(app, gid, &words[1..].join(" ")).await? {
                            Ok(u) => Some(user_info(app, gid, u).await?),
                            Err(e) => return text(&e),
                        },
                    }
                }
            };
            match e {
                Some(e) => embed(e),
                None => text("Nothing found"),
            }
        }
        "ncases" => embed(ncases(app, gid, arg).await?),
        "tshoot" if arg == Some("lockdown") => {
            if allowed(level, "owner") {
                embed(tshoot_lockdown(app, gid).await?)
            } else {
                text("Only the owner or an Extra Owner can run this")
            }
        }
        _ => embed(tshoot(app, gid)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_command_exists_and_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, aliases, ..) in CMDS {
            for n in std::iter::once(name).chain(aliases.iter()) {
                assert!(seen.insert(*n), "{n} listed twice");
            }
        }
        for (_, names) in CATEGORIES {
            assert!(names.iter().all(|n| lookup(n).is_some()));
        }
        assert_eq!(
            CATEGORIES.iter().map(|c| c.1.len()).sum::<usize>(),
            CMDS.len()
        );
    }

    #[test]
    fn every_command_has_help_and_every_usage_word_is_explained() {
        for c in CMDS {
            assert!(!c.4.is_empty(), "{} has no description", c.0);
            let names = |s: &str| {
                s.split(' ')
                    .next()
                    .is_some_and(|w| w == c.0 || c.1.contains(&w))
            };
            assert!(names(c.3), "{} usage must start with its name", c.0);
            assert!(
                c.5.is_empty() || names(c.5),
                "{} example must start with its name",
                c.0
            );
            for w in c.3.split([' ', '[', ']', '|']) {
                let arg = w.starts_with(['?', '-', '@'])
                    || (!w.is_empty() && w.bytes().all(|b| b.is_ascii_uppercase()));
                assert!(
                    !arg || ARGS.iter().any(|a| a.0 == w),
                    "{}: `{w}` is not in ARGS",
                    c.0
                );
            }
            assert!(
                args(c.3).is_empty() || !c.5.is_empty(),
                "{} needs an example",
                c.0
            );
        }
        let used: Vec<_> = CMDS.iter().flat_map(|c| args(c.3)).collect();
        assert!(ARGS.iter().all(|a| used.contains(a)), "unused ARGS entry");
        let h = help_embed(lookup("b").unwrap(), "t!", "Tom")
            .description
            .unwrap();
        assert!(
            h.contains("`TIME` A duration")
                && h.contains("**Example:** `t!ban @member 7d")
                && h.contains("**Permit:** `Permit: Ban Command`")
                && h.contains("**Category:** `moderation`"),
            "{h}"
        );
        let g = help_embed(lookup("greetingmessage").unwrap(), "t!", "Tom");
        assert!(g.description.unwrap().contains("Message variables"));
    }

    #[test]
    fn catalog_lists_commands_slash_forms_and_variables() {
        let c = catalog();
        assert_eq!(c["commands"].as_array().unwrap().len(), CMDS.len());
        let ban = &c["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "ban")
            .unwrap();
        assert_eq!(ban["category"], "Moderation Commands");
        assert!(ban["slash"][0]
            .as_str()
            .unwrap()
            .starts_with("/ban add user [reason]"));
        let apps = c["apps"].as_array().unwrap();
        assert!(apps
            .iter()
            .any(|a| a["name"].as_str().unwrap().starts_with("/softban user")));
        assert!(apps.iter().any(|a| a["name"] == "Report Message"));
        assert_eq!(
            c["variables"].as_array().unwrap().len(),
            crate::tickets::TAGS.len()
        );
    }

    #[test]
    fn commands_are_filtered_by_permit() {
        let (e, pages) = commands_page(&Level::None, "t!", 2);
        assert_eq!(pages, 4);
        assert!(!e.description.unwrap().contains("`t!ban`"));
        let (e, _) = commands_page(&Level::Owner, "t!", 2);
        let d = e.description.unwrap();
        assert!(d.starts_with("❯ `t!ban` Ban") && d.contains("Only commands you can use"));
        let (e, _) = commands_page(&Level::None, "t!", 0);
        assert!(e.description.unwrap().contains("`t!ping`"));
    }
}
