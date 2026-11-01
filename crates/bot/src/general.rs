//! Wick's general commands: about, commands, help (and `-h` after any command), invite, ping, info,
//! ncases and tshoot.
use std::time::Instant;

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

/// (name, aliases, permit, usage, what). Permit "" is anyone, "*" any permit, "owner" Owner or Extra Owner.
const CMDS: &[(&str, &[&str], &str, &str, &str)] = &[
    ("about", &[], "", "about", "About TommyBot"),
    (
        "commands",
        &["command", "cmd", "cmds"],
        "",
        "commands",
        "The commands you can use",
    ),
    ("help", &["docs", "h"], "", "help", "Help panel and links"),
    ("invite", &[], "", "invite", "Add TommyBot to a server"),
    ("ping", &[], "", "ping", "Latency, uptime and memory"),
    (
        "info",
        &["whois", "i"],
        "",
        "info [MEMBER | ID | @role | -server]",
        "Member, user, role or server info",
    ),
    (
        "roleinfo",
        &["ri", "rinfo"],
        "*",
        "roleinfo ROLE",
        "Role info",
    ),
    (
        "serverinfo",
        &["guildinfo", "sinfo", "ginfo", "si", "gi"],
        "*",
        "serverinfo",
        "Server info",
    ),
    ("setup", &[], "*", "setup", "Setup panel"),
    ("config", &[], "*", "config", "Config overview"),
    (
        "statics",
        &["s"],
        "*",
        "statics [VALUE ?set N | OBJ ?add N]",
        "Quarantine role, main roles, log channels",
    ),
    (
        "heat",
        &[],
        "*",
        "heat [N ?on | VALUE ?set N]",
        "Heat system panel",
    ),
    (
        "hfilters",
        &[],
        "*",
        "hfilters [page]",
        "Heat system filters",
    ),
    (
        "antinuke",
        &["an"],
        "*",
        "antinuke [N ?on | N ?off]",
        "Anti nuke panel",
    ),
    (
        "anpanic",
        &["anp"],
        "*",
        "anpanic [N ?on | OBJ ?add N]",
        "Panic mode panel",
    ),
    (
        "antinukerestore",
        &["anr"],
        "*",
        "anr [N ?on | N ?off]",
        "Restore system filters",
    ),
    (
        "joingate",
        &["jg"],
        "*",
        "jg [N ?on | VALUE ?set N]",
        "Join gate panel",
    ),
    (
        "joinraid",
        &["jr"],
        "*",
        "jr [N ?on | VALUE ?set N]",
        "Join raid panel",
    ),
    (
        "verification",
        &["v"],
        "*",
        "v [N ?on | VALUE ?set N]",
        "Verification panel",
    ),
    (
        "misc",
        &[],
        "*",
        "misc [N ?on | VALUE ?set N]",
        "Miscellaneous panel",
    ),
    (
        "whitelist",
        &[],
        "*",
        "whitelist TARGET",
        "Interactive whitelist",
    ),
    (
        "tshoot",
        &[],
        "*",
        "tshoot [lockdown]",
        "Clear caches; `lockdown` resets View/Send on roles",
    ),
    (
        "ban",
        &["b"],
        "ban",
        "ban MEMBERS [TIME] [?r REASON] [?dm] [?s] [?p]",
        "Ban, softban with ?s",
    ),
    (
        "hackban",
        &["hb"],
        "ban",
        "hackban IDS [?r REASON] [?dm]",
        "Ban someone not in the server",
    ),
    (
        "unban",
        &["ub"],
        "ban",
        "unban MEMBERS [?r REASON]",
        "Lift a ban",
    ),
    (
        "kick",
        &["k"],
        "kick",
        "kick MEMBERS [?r REASON] [?dm]",
        "Kick",
    ),
    (
        "timeout",
        &["mute", "m", "to", "t"],
        "timeout",
        "timeout MEMBERS [TIME] [?r REASON]",
        "Time out",
    ),
    (
        "untimeout",
        &["unmute", "um"],
        "timeout",
        "untimeout MEMBER",
        "Lift a timeout",
    ),
    ("warn", &["wa"], "warn", "warn MEMBERS [?r REASON]", "Warn"),
    ("unwarn", &[], "warn", "unwarn CASE", "Remove a warn"),
    ("warns", &[], "warn", "warns MEMBER", "A member's warns"),
    (
        "clearwarns",
        &[],
        "warn",
        "clearwarns MEMBER",
        "Remove all warns",
    ),
    (
        "quarantine",
        &["q"],
        "quarantine",
        "quarantine MEMBER [?r REASON]",
        "Quarantine",
    ),
    (
        "unquarantine",
        &["uq", "restore", "r"],
        "quarantine",
        "unquarantine MEMBER",
        "Release from quarantine",
    ),
    (
        "purge",
        &["clear", "sweep", "p", "c", "botclear", "bc"],
        "purge",
        "purge [COUNT] [MEMBER]",
        "Delete messages",
    ),
    (
        "slowmode",
        &["sm"],
        "slowmode",
        "slowmode TIME | off",
        "Channel slowmode",
    ),
    (
        "lock",
        &["lockdown", "ld"],
        "lockdown",
        "lock [?t TYPE] [TIME]",
        "Lock the server or channel",
    ),
    ("unlock", &["ul"], "lockdown", "unlock [?t TYPE]", "Unlock"),
    (
        "sanitize",
        &["san", "dehoist"],
        "sanitize",
        "sanitize MEMBER",
        "Clean a nickname",
    ),
    (
        "verify",
        &[],
        "verify",
        "verify MEMBER",
        "Verify a member by hand",
    ),
    (
        "cases",
        &["case"],
        "cases",
        "cases [CASE | MEMBER] [?t TYPE]",
        "View cases",
    ),
    (
        "modcases",
        &[],
        "cases",
        "modcases MODERATOR [?t TYPE]",
        "Cases made by a moderator",
    ),
    (
        "editc",
        &["editreason"],
        "cases",
        "editc CASE REASON",
        "Change a case's reason",
    ),
    (
        "closecase",
        &["caseoff", "opencase", "caseon"],
        "cases",
        "closecase CASE",
        "Switch a case off or on",
    ),
    ("ncases", &[], "cases", "ncases [ID]", "Anti-nuke cases"),
    (
        "backups",
        &["backup"],
        "backups",
        "backups [take | load ID ?confirm | delete ID | clear]",
        "Server backups",
    ),
    (
        "notes",
        &["note"],
        "notes_view",
        "notes MEMBER | note add MEMBER ?r TEXT",
        "Staff notes",
    ),
];

const CATEGORIES: [(&str, &[&str]); 3] = [
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
];

fn allowed(level: &Level, perm: &str) -> bool {
    match perm {
        "" => true,
        "*" => *level != Level::None,
        "owner" => matches!(level, Level::Owner | Level::ExtraOwner),
        p => level.has(p),
    }
}

fn lookup(
    word: &str,
) -> Option<&'static (
    &'static str,
    &'static [&'static str],
    &'static str,
    &'static str,
    &'static str,
)> {
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

/// `about`: page 1 the pitch, page 2 the version lines ("Nerdy Stats").
pub fn about_page(page: usize) -> (Embed, usize) {
    let page = page.min(1);
    let e = if page == 0 {
        Log::new("About TommyBot:", BLUE).text("A Rust moderation bot: anti-nuke, heat automod, join gate, join raid, verification and logs.")
    } else {
        let up = now_secs() - metrics::STARTED.load(std::sync::atomic::Ordering::Relaxed) as i64;
        Log::new("About TommyBot [Nerdy Stats]:", BLUE)
            .line("Version", &format!("`{}`", env!("CARGO_PKG_VERSION")))
            .line("Uptime", &format!("`{}`", embed::duration(up)))
            .line("Price", "Everything is free")
    };
    (e.footer(&format!("Page: {}/2", page + 1)).build(), 2)
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
        e = e.text(&format!("❯ `{prefix}{}` {}", name.0, name.4));
    }
    let e = e.footer(&format!(
        "Need more? {prefix}COMMAND -h · Page: {}/{}",
        page + 1,
        CATEGORIES.len()
    ));
    (e.build(), CATEGORIES.len())
}

fn dashboard() -> Option<String> {
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

/// Wick's troubleshooting embed: result lines, then the finished block.
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
        let aliases = if c.1.is_empty() {
            String::new()
        } else {
            format!("\nAliases: {}", c.1.join(", "))
        };
        send(
            app,
            msg,
            &format!("**{pfx}{}**: {}\nUsage: `{pfx}{}`{aliases}", c.0, c.4, c.3),
            None,
            vec![],
        )
        .await?;
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
    let (text, embed, rows) = answer(app, gid, msg.author.id, &level, name, &words).await?;
    send(app, msg, &text, embed, rows).await?;
    Ok(true)
}

/// `/about`, `/ping`, `/help`, `/invite`, `/commands`, `/info user|server` as the words of the prefix command.
pub fn slash_words(name: &str, o: &[CommandDataOption]) -> Option<Vec<String>> {
    if !matches!(
        name,
        "about" | "ping" | "help" | "invite" | "commands" | "info"
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
            let e = Log::new("TommyBot's Help Panel:", BLUE)
                .text("🫴 **General Help:**")
                .text(&format!("{ARROW} To view the commands you can use, do: `{pfx}commands`"))
                .text(&format!("{ARROW} To see how one command works, do: `{pfx}COMMAND -h`"))
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
                "Latency: `{gw}` (API `{rest_ms}ms`)\nUptime: `{}`\nResources:\n  RAM: `{ram}`\n  CPU: `{}`\nSize:\n  Servers: `{servers}`",
                embed::duration(up),
                cpu_percent(up),
            );
            embed(
                Log::new("TommyBot Status:", GREEN)
                    .inline("🟢 Shard [0 -Current-]:", &shard)
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
            let sys = |on: bool| if on { YES } else { NO };
            embed(
                Log::new(&format!("{}'s Config Overview", g.name), BLUE)
                    .section("Essential Roles")
                    .line("Quarantine Role", &role)
                    .section("Major Systems")
                    .line("Anti Nuke", sys(cfg.antinuke.enabled))
                    .line("Heat System", sys(cfg.automod.enabled))
                    .line("Verification", sys(cfg.verification.enabled))
                    .line("Join Gate", sys(cfg.joingate.enabled))
                    .line(
                        "Join Raid",
                        sys(cfg.joinraid.as_ref().is_some_and(|r| r.enabled)),
                    )
                    .line("Persistence", sys(cfg.misc.enforce_quarantine_perms))
                    .build(),
            )
        }
        "serverinfo" => server_info(app, gid, invoker).await,
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
    fn commands_are_filtered_by_permit() {
        let (e, pages) = commands_page(&Level::None, "w!", 2);
        assert_eq!(pages, 3);
        assert!(e.description.is_none());
        let (e, _) = commands_page(&Level::Owner, "w!", 2);
        assert!(e.description.unwrap().starts_with("❯ `w!ban` Ban"));
        let (e, _) = commands_page(&Level::None, "w!", 0);
        assert!(e.description.unwrap().contains("`w!ping`"));
    }
}
