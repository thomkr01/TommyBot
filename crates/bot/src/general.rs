//! Wick's general commands: about, commands, help (and `-h` after any command), invite, ping, info,
//! ncases and tshoot.
use std::time::Instant;

use anyhow::Result;
use tommybot_antinuke::DANGEROUS_PERMS;
use tommybot_core::{cases, permits::Level};
use tommybot_logging::embed::{self, Log, BLUE, GREEN, NO, YES};
use twilight_model::{
    channel::message::{AllowedMentions, Component, Embed},
    gateway::payload::incoming::MessageCreate,
    guild::Permissions,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    metrics, settings, targets, welcome,
};

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
    (
        "setup",
        &["config"],
        "*",
        "setup",
        "Setup panel and config overview",
    ),
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
        "hackban ID",
        "Ban someone not in the server",
    ),
    ("unban", &["ub"], "ban", "unban ID", "Lift a ban"),
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
        &["clear", "sweep", "p", "c"],
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
            "statics",
            "heat",
            "hfilters",
            "antinuke",
            "anpanic",
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

fn created_secs(id: u64) -> i64 {
    (((id >> 22) + 1_420_070_400_000) / 1000) as i64
}

fn yes(b: bool) -> &'static str {
    if b {
        YES
    } else {
        NO
    }
}

/// Names of the dangerous permission bits in `perms`.
fn dangerous(perms: u64) -> String {
    const NAMES: [(u64, &str); 9] = [
        (1 << 3, "Administrator"),
        (1 << 1, "Kick Members"),
        (1 << 2, "Ban Members"),
        (1 << 4, "Manage Channels"),
        (1 << 5, "Manage Server"),
        (1 << 13, "Manage Messages"),
        (1 << 28, "Manage Roles"),
        (1 << 29, "Manage Webhooks"),
        (1 << 40, "Moderate Members"),
    ];
    let names: Vec<_> = NAMES
        .iter()
        .filter(|(b, _)| perms & b & DANGEROUS_PERMS != 0)
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        NO.into()
    } else {
        names.join(", ")
    }
}

fn level_name(l: &Level) -> String {
    match l {
        Level::None => "None".into(),
        Level::Custom { rank, .. } => format!("Permit (rank {rank})"),
        Level::TrustedAdmin => "Trusted Admin".into(),
        Level::ExtraOwner => "Extra Owner".into(),
        Level::Owner => "Owner".into(),
    }
}

async fn user_info(app: &App, gid: Id<GuildMarker>, uid: Id<UserMarker>) -> Result<Embed> {
    let cfg = app.cfg(gid.get()).await?;
    let info = app.guild_info(gid).await?;
    let member = match app.http.guild_member(gid, uid).await {
        Ok(m) => Some(m.model().await?),
        Err(_) => None,
    };
    let user = match &member {
        Some(m) => m.user.clone(),
        None => app.http.user(uid).await?.model().await?,
    };
    let created = created_secs(uid.get());
    let mut e = Log::new(&format!("{}'s Info:", user.name), BLUE)
        .thumbnail(&user)
        .user("User", uid.get())
        .line("Created", &format!("<t:{created}:R>"));
    let Some(m) = member else {
        let banned = app.http.ban(gid, uid).await.is_ok();
        return Ok(e
            .line("In Server", NO)
            .line("Banned", yes(banned))
            .line("Bot", yes(user.bot))
            .build());
    };
    if let Some(j) = m.joined_at {
        e = e.line("Joined", &format!("<t:{}:R>", j.as_secs()));
    }
    let young = now_secs() - created < cfg.joingate.min_age_ms() / 1000;
    let suspicious = format!(
        "{} No Avatar\n{} Account Younger Than {}d",
        yes(user.avatar.is_none()),
        yes(young),
        cfg.joingate.min_age_days
    );
    let points = cases::warn_points(&app.pool, gid.get() as i64, uid.get() as i64).await?;
    let wl = &cfg.whitelist;
    let on = |l: &tommybot_core::sections::WhitelistList| {
        l.users.contains(&uid) || m.roles.iter().any(|r| l.roles.contains(r))
    };
    let whitelists = format!(
        "{} Spam\n{} Ping\n{} Advertising\n{} @everyone\n{} Quarantine",
        yes(on(&wl.spam)),
        yes(on(&wl.mentions)),
        yes(on(&wl.invites)),
        yes(on(&wl.everyone)),
        yes(cfg.antinuke.quarantine_whitelist.contains(&uid))
    );
    let perms = m
        .roles
        .iter()
        .chain([&gid.cast()])
        .filter_map(|r| info.role_perms.get(r))
        .fold(0, |a, p| a | p);
    let level = app.level(gid, info.owner, uid).await?;
    Ok(e.inline("Suspicious Status:", &suspicious)
        .inline("Whitelist:", &whitelists)
        .inline("Warn Points:", &format!("`{points}`"))
        .inline("Dangerous Permissions:", &dangerous(perms))
        .inline("TommyBot Permissions:", &level_name(&level))
        .build())
}

async fn role_info(app: &App, gid: Id<GuildMarker>, rid: u64) -> Result<Option<Embed>> {
    let roles = app.http.roles(gid).await?.models().await?;
    let Some(r) = roles.into_iter().find(|r| r.id.get() == rid) else {
        return Ok(None);
    };
    let cfg = app.cfg(gid.get()).await?;
    let wl = &cfg.whitelist;
    let main = cfg.lockdown.main_roles.contains(&r.id);
    let whitelists = format!(
        "{} Spam\n{} Ping\n{} Advertising\n{} @everyone",
        yes(wl.spam.roles.contains(&r.id)),
        yes(wl.mentions.roles.contains(&r.id)),
        yes(wl.invites.roles.contains(&r.id)),
        yes(wl.everyone.roles.contains(&r.id))
    );
    Ok(Some(
        Log::new(&format!("{}'s Info:", r.name), BLUE)
            .line("Role", &embed::role(rid))
            .line("Created", &format!("<t:{}:R>", created_secs(rid)))
            .line("Position", &format!("`{}`", r.position))
            .inline(
                "Details:",
                &format!(
                    "{} Managed\n{} Mentionable\n{} Hoisted\n{} Main Role",
                    yes(r.managed),
                    yes(r.mentionable),
                    yes(r.hoist),
                    yes(main)
                ),
            )
            .inline("Whitelist:", &whitelists)
            .inline("Dangerous Permissions:", &dangerous(r.permissions.bits()))
            .build(),
    ))
}

async fn server_info(app: &App, gid: Id<GuildMarker>) -> Result<Embed> {
    let g = app.http.guild(gid).with_counts(true).await?.model().await?;
    let channels = app.http.guild_channels(gid).await?.models().await?.len();
    Ok(Log::new(&format!("{}'s Info:", g.name), BLUE)
        .line("Owner", &embed::user(g.owner_id.get()))
        .line("Created", &format!("<t:{}:R>", created_secs(gid.get())))
        .line(
            "Members",
            &format!("`{}`", g.approximate_member_count.unwrap_or_default()),
        )
        .line("Roles", &format!("`{}`", g.roles.len()))
        .line("Channels", &format!("`{channels}`"))
        .line(
            "Boosts",
            &format!(
                "`{}` (tier {})",
                g.premium_subscription_count.unwrap_or_default(),
                u8::from(g.premium_tier)
            ),
        )
        .build())
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
fn tshoot(app: &App, gid: Id<GuildMarker>) -> String {
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
    format!(
        "{YES} Decached heat\n{YES} Decached nuke heat\n{YES} Decached server and members\n{YES} Reloaded config\nTroubleshooting Finished! TommyBot has made some changes in `{:.2}ms`",
        started.elapsed().as_secs_f64() * 1000.0
    )
}

/// `tshoot lockdown`: main roles (or @everyone when there are none) get View Channel and Send Messages,
/// every other role below the bot loses them.
async fn tshoot_lockdown(app: &App, gid: Id<GuildMarker>) -> Result<String> {
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
    Ok(match tweaked {
        0 => "No roles needed changes. You are clear!".into(),
        n => format!("{YES} {n} roles were tweaked"),
    })
}

async fn send(
    app: &App,
    msg: &MessageCreate,
    text: &str,
    embed: Option<Embed>,
    rows: Vec<Component>,
) -> Result<()> {
    let embeds: Vec<Embed> = embed.into_iter().collect();
    app.http
        .create_message(msg.channel_id)
        .content(text)
        .embeds(&embeds)
        .components(&rows)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
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
    let arg = words.get(1).copied();
    match name {
        "about" => {
            let up =
                now_secs() - metrics::STARTED.load(std::sync::atomic::Ordering::Relaxed) as i64;
            let e = Log::new("About TommyBot:", BLUE)
                .text("A Rust moderation bot: anti-nuke, heat automod, join gate, join raid, verification and logs.")
                .line("Version", &format!("`{}`", env!("CARGO_PKG_VERSION")))
                .line("Uptime", &format!("`{}`", embed::duration(up)))
                .line("Price", "Everything is free")
                .build();
            send(app, msg, "", Some(e), links(app)).await?;
        }
        "commands" => {
            let (e, pages) = commands_page(&level, &pfx, 0);
            send(
                app,
                msg,
                "",
                Some(e),
                vec![settings::page_row(msg.author.id.get(), "cmds", 0, pages)],
            )
            .await?;
        }
        "help" => {
            let e = Log::new("TommyBot Help Panel", BLUE)
                .line(
                    "Commands",
                    &format!("`{pfx}commands` lists what you can use"),
                )
                .line(
                    "One command",
                    &format!("`{pfx}COMMAND -h` shows how it works"),
                )
                .line(
                    "Settings",
                    &format!("`{pfx}setup`, then `{pfx}heat`, `{pfx}an`, `{pfx}jg`..."),
                )
                .build();
            send(app, msg, "", Some(e), links(app)).await?;
        }
        "invite" => {
            let e = Log::new("Invite TommyBot!", BLUE)
                .text("Add TommyBot to your server with the button below.")
                .build();
            send(app, msg, "", Some(e), links(app)).await?;
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
            let e = Log::new("TommyBot Status:", GREEN)
                .line(
                    "Gateway Latency",
                    &if gw == 0 {
                        "measuring".into()
                    } else {
                        format!("`{gw}ms`")
                    },
                )
                .line("API Latency", &format!("`{rest_ms}ms`"))
                .line("Uptime", &format!("`{}`", embed::duration(up)))
                .line("RAM", &format!("`{ram}`"))
                .build();
            send(app, msg, "", Some(e), vec![]).await?;
        }
        "serverinfo" => send(app, msg, "", Some(server_info(app, gid).await?), vec![]).await?,
        "roleinfo" | "info" => {
            let staff = level != Level::None;
            let e = match arg {
                Some("-server" | "-guild" | "-g") if staff => Some(server_info(app, gid).await?),
                _ if name == "roleinfo" || arg.is_some_and(|a| a.starts_with("<@&")) => {
                    let id = arg
                        .and_then(|a| a.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok());
                    match id {
                        Some(id) if staff => role_info(app, gid, id).await?,
                        _ => None,
                    }
                }
                None => Some(user_info(app, gid, msg.author.id).await?),
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
                            Err(e) => return send(app, msg, &e, None, vec![]).await.map(|_| true),
                        },
                    }
                }
            };
            match e {
                Some(e) => send(app, msg, "", Some(e), vec![]).await?,
                None => send(app, msg, "Nothing found", None, vec![]).await?,
            }
        }
        "ncases" => send(app, msg, "", Some(ncases(app, gid, arg).await?), vec![]).await?,
        "tshoot" if arg == Some("lockdown") => {
            let text = if allowed(&level, "owner") {
                tshoot_lockdown(app, gid).await?
            } else {
                "Only the owner or an Extra Owner can run this".into()
            };
            send(app, msg, &text, None, vec![]).await?;
        }
        _ => send(app, msg, &tshoot(app, gid), None, vec![]).await?,
    }
    Ok(true)
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

    #[test]
    fn dangerous_names() {
        assert_eq!(dangerous(0), NO);
        assert_eq!(
            dangerous((1 << 3) | (1 << 2) | (1 << 10)),
            "Administrator, Ban Members"
        );
    }
}
