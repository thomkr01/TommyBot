//! Wizard cards: a few settings at a time with a live preview of what they change.
use crate::form::{name_of, strs};
use leptos::prelude::*;
use serde_json::Value;

pub struct Card {
    pub title: &'static str,
    pub desc: &'static str,
    pub notes: &'static [&'static str],
    /// (full config path, label); an empty label keeps the schema's title.
    pub fields: &'static [(&'static str, &'static str)],
    /// What the settings look like in Discord, from (config, Discord channels and roles).
    pub preview: fn(&Value, &Value) -> AnyView,
    /// A button that POSTs `/api/guilds/{id}/<path>` with the config version: (label, path).
    pub action: Option<(&'static str, &'static str)>,
    /// The fields after the first sit in a "While you are at it" box that rings red while the first (the switch) is off.
    pub boxed: bool,
    /// (field, switch): the field slides open only while that switch is on.
    pub when: &'static [(&'static str, &'static str)],
    /// A red warning box under the description.
    pub alert: Option<&'static str>,
    /// (field, follower): changing the field sets the follower to the same value.
    pub links: &'static [(&'static str, &'static str)],
}

const fn card(
    title: &'static str,
    desc: &'static str,
    fields: &'static [(&'static str, &'static str)],
    preview: fn(&Value, &Value) -> AnyView,
) -> Card {
    Card {
        title,
        desc,
        notes: &[],
        fields,
        preview,
        action: None,
        boxed: false,
        when: &[],
        alert: None,
        links: &[],
    }
}

const fn boxed(c: Card) -> Card {
    Card { boxed: true, ..c }
}

use crate::discord::{bot, chat, sample, show, show_msg};

/// The message box with something typed in it.
fn typed(text: String) -> AnyView {
    view! { <div class="dinput">{text}</div> }.into_any()
}

fn text(t: &str) -> AnyView {
    t.to_string().into_any()
}

fn on(c: &Value, path: &str, default: bool) -> bool {
    path.split('.')
        .fold(c, |v, k| &v[k])
        .as_bool()
        .unwrap_or(default)
}

pub const MISC: &[Card] = &[
    card("Prefix", "Choose a prefix that your staff members can memorize easily, the prefix . is usually recommended.", &[("misc.prefix", "Prefix")], |c, _| {
        let p = c["misc"]["prefix"].as_str().unwrap_or("t!");
        view! { {chat("you", text("Your mom gay"))}{typed(format!("{p}ban @member ?r toxic"))} }.into_any()
    }),
    card("Slash commands", "Enable TommyBot's slash commands.", &[("misc.use_slash", "Enable slash commands")], |c, _| {
        let p = c["misc"]["prefix"].as_str().unwrap_or("t!");
        let note = format!("Prefix commands like {p}ban always work too, with slash commands on or off.");
        if on(c, "misc.use_slash", true) { view! { {typed("/ban user: @member reason: toxic".into())}<p class="muted">{note}</p> }.into_any() } else { view! { <p>"Slash commands are off."</p><p class="muted">{note}</p> }.into_any() }
    }),
    card("Compact mode", "Do you want TommyBot's messages to have a compact style?", &[("misc.compact_replies", "Minify TommyBot's message replies")], |c, _| {
        show(if on(c, "misc.compact_replies", false) { "ban_compact" } else { "ban_result" })
    }),
    card("Confirmation", "Should TommyBot ask Yes or No before punishing a member typed as an ID or a name?", &[("misc.confirm", "Ask for a Yes/No confirmation")], |c, _| {
        if on(c, "misc.confirm", true) { show("confirm_question") } else { view! { <p>"Commands run at once."</p> }.into_any() }
    }),
    card("Punishment DMs", "Should TommyBot DM members when they are punished or when a punishment is removed?", &[("misc.always_dm", "DM punished members")], |c, _| {
        if on(c, "misc.always_dm", true) { show("dm_ban") } else { view! { <p>"Only commands with ?dm send a DM."</p> }.into_any() }
    }),
    Card {
        notes: &["If you want TommyBot to make the log channel for you, leave it empty and run Auto Setup."],
        ..card("Logging channel", "Where do you want TommyBot to log its actions?", &[("logging.fallback", "Logs channel")], |c, dc| {
            let ch = name_of(dc, "channels", c["logging"]["fallback"].as_str().unwrap_or(""));
            view! { <p class="muted">{format!("Logs go to {}", if ch.is_empty() { "nowhere yet".into() } else { ch })}</p>{show("automod_warn")} }.into_any()
        })
    },
    Card {
        notes: &["If you want TommyBot to make the modlog channel for you, leave it empty and run Auto Setup."],
        ..card("Moderation logs channel", "Where do you want TommyBot to log moderation actions made by your staff members?", &[("logging.routes.mod_command", "Modlogs channel")], |_, _| {
            show("mod_log")
        })
    },
    Card {
        notes: &["A main role is super important to fight off raiders and spammers. Auto Setup creates Member when it is empty."],
        ..card("Main roles", "What is the role your members need to access the server, something like @Member or @Verified? Select it.", &[("lockdown.main_roles", "Main roles")], |c, dc| {
            let names: Vec<String> = strs(&c["lockdown"]["main_roles"]).iter().map(|r| name_of(dc, "roles", r)).collect();
            view! { <p>"ROLES"</p><div>{names.into_iter().map(|n| view! { <span class="chip">{n}" · Main Role"</span>" " }).collect_view()}</div> }.into_any()
        })
    },
];

pub const AUTOMOD: &[Card] = &[
    boxed(card(
        "Automod status",
        "Do you want TommyBot to monitor messages?",
        &[
            ("automod.enabled", "Enable Automod"),
            (
                "automod.filters.invites.enabled",
                "Moderate invite links posted",
            ),
            (
                "automod.filters.nsfw_links.enabled",
                "Filter out NSFW links",
            ),
            (
                "automod.filters.bad_sites.enabled",
                "Filter out malicious links",
            ),
            (
                "automod.filters.scam_links.enabled",
                "Filter out scam links and login QR codes",
            ),
        ],
        |_, _| show("automod_kick"),
    )),
    Card {
        // One switch for long messages, as both character filters (lowercase and uppercase).
        links: &[(
            "automod.filters.chars_lower.enabled",
            "automod.filters.chars_upper.enabled",
        )],
        ..boxed(card(
            "Antispam module",
            "Do you want TommyBot to also monitor and fight off spam in your server?",
            &[
                ("automod.anti_spam", "Enable Anti Spam"),
                (
                    "automod.filters.mentions.enabled",
                    "Track and stop @mention spam",
                ),
                (
                    "automod.filters.attachments.enabled",
                    "Track and stop attachment (image, embed, file) spam",
                ),
                (
                    "automod.filters.similar.enabled",
                    "Monitor repetitive message spam",
                ),
                (
                    "automod.filters.chars_lower.enabled",
                    "Monitor long messages",
                ),
                ("automod.filters.emojis.enabled", "Monitor emoji spam"),
                (
                    "automod.filters.newlines.enabled",
                    "Monitor new lines in messages",
                ),
                ("automod.filters.zalgo.enabled", "Monitor zalgo in messages"),
                (
                    "automod.auto_slowmode.enabled",
                    "Slow down channels that flood",
                ),
            ],
            |_, _| show("automod_timeout"),
        ))
    },
    card(
        "Webhook coverage",
        "Do you want TommyBot to also monitor webhooks sending messages in your server?",
        &[("automod.monitor_webhooks", "Monitor unwhitelisted webhooks")],
        |c, _| {
            text(if on(c, "automod.monitor_webhooks", true) {
                "Webhook messages count towards spam like members' messages do."
            } else {
                "Webhook messages are ignored."
            })
        },
    ),
];

pub const ANTINUKE: &[Card] = &[
    boxed(card("Antinuke status", "Do you want TommyBot to monitor moderation actions made by your staff members?", &[
        ("antinuke.enabled", "Enable Antinuke"),
        ("antinuke.limits.Ban.enabled", "Monitor kicks and bans"),
        ("antinuke.limits.RoleCreate.enabled", "Monitor role creations"),
        ("antinuke.limits.RoleDelete.enabled", "Monitor role deletions"),
        ("antinuke.limits.ChannelCreate.enabled", "Monitor channel creations"),
        ("antinuke.limits.ChannelDelete.enabled", "Monitor channel deletions"),
        ("antinuke.limits.WebhookCreate.enabled", "Monitor webhook creations"),
        ("antinuke.limits.WebhookDelete.enabled", "Monitor webhook deletions"),
    ], |_, _| show("nuke_quarantined"))),
    Card {
        notes: &["Leave the role empty and Auto Setup makes @Quarantine for you (old Quarantine roles are deleted). Pick a role and Auto Setup removes all of its permissions and denies it in every channel and category."],
        ..boxed(card("Quarantine", "Do you want the quarantine system that keeps your server safe from being destroyed?", &[
        ("antinuke.auto_quarantine", "Use the auto quarantine module"),
        ("antinuke.strict_mode", "Punish unauthorized admins giving dangerous permissions to any role"),
        ("antinuke.strict_member_roles", "Punish unauthorized admins giving dangerous permissions to other members"),
        ("antinuke.monitor_public_roles", "Protect @everyone and the main roles from getting dangerous permissions"),
        ("antinuke.vanity_guard", "Guard the server's vanity url"),
        ("antinuke.quarantine_role", "Quarantine role"),
    ], |c, dc| view! {
        <p>{format!("Quarantined members only keep {}.", name_of(dc, "roles", c["antinuke"]["quarantine_role"].as_str().unwrap_or("the quarantine role")))}</p>
        {show(if on(c, "antinuke.auto_quarantine", true) { "nuke_quarantined" } else { "nuke_no_quarantine" })}
    }.into_any()))
    },
];

const AUTO_WL: &str = "Owners, Trusted admins and members with dangerous Discord/TommyBot permissions are automatically whitelisted.";
const STAFF_WL: &str = "ONLY owners and trusted admins are whitelisted automatically, your other staff members are targeted if not whitelisted.";

macro_rules! wl {
    ($p:literal) => {
        &[
            (concat!("whitelist.", $p, "users"), "Members (IDs)"),
            (concat!("whitelist.", $p, "roles"), "Roles"),
            (concat!("whitelist.", $p, "channels"), "Channels"),
            (concat!("whitelist.", $p, "categories"), "Categories"),
            (concat!("whitelist.", $p, "webhooks"), "Webhooks (IDs)"),
        ]
    };
}

pub const WHITELIST: &[Card] = &[
    Card {
        notes: &[AUTO_WL, "This whitelist targets these filters: Regular Spam, Similar Spam, Characters/Walls Of Text, Emoji Spam, Zalgo Spam, New Lines."],
        ..card("Spam whitelist", "Pick the users, roles, channels, categories and webhooks you want to whitelist when it comes to spam.", wl!(""), |_, _| {
            view! { {chat("member", text("HAHAHA IM SPAMMING WOOO"))}{chat("member", text("HAHAHA IM SPAMMING WOOO"))} }.into_any()
        })
    },
    Card {
        notes: &[AUTO_WL, "Your own server's invite links are automatically whitelisted."],
        ..card("Invite link whitelist", "Pick the users, roles, channels, categories and webhooks you want to whitelist when it comes to posting invite links.", wl!("invites."), |_, _| chat("member", text("Join my server everyone discord.gg/example")))
    },
    Card {
        notes: &[STAFF_WL, "This whitelist is related to @user and @role mentions only."],
        ..card("@ping whitelist", "Pick the users, roles, channels, categories and webhooks you want to whitelist when it comes to spamming @user and @role mentions.", wl!("mentions."), |_, _| chat("member", text("@ark @Squeezy @REGEX @Revin you suck!")))
    },
    Card {
        notes: &[STAFF_WL, "This whitelist is related to @everyone and main role mentions only."],
        ..card("@everyone whitelist", "Pick the users, roles, channels, categories and webhooks you want to whitelist when it comes to sending @everyone mentions (or main roles).", wl!("everyone."), |_, _| chat("member", text("@everyone you suck!")))
    },
    Card {
        notes: &["You do not need this step if you use TommyBot's own tickets: they are always exempt.", "If you use another ticket bot, you should set this whitelist, or its new ticket channels count towards the antinuke limits."],
        ..card("Channel creation whitelist (other ticket bots)", "Using another ticket bot? Pick the categories it opens ticket channels in, so antinuke leaves them alone.", &[("antinuke.ticket_categories", "Categories")], |c, dc| {
            let cats: Vec<String> = strs(&c["antinuke"]["ticket_categories"]).iter().map(|id| name_of(dc, "categories", id)).collect();
            text(&if cats.is_empty() { "Every channel creation counts towards the antinuke limits.".to_string() } else { format!("Channels made or deleted under {} never count.", cats.join(", ")) })
        })
    },
    Card {
        notes: &[
            "ONLY owners and trusted admins are whitelisted automatically.",
            "This whitelist lets a member or bot edit the roles of someone in Quarantine. It does not stop them getting quarantined, that is Immunity in Permits.",
            "Autorole bots can be added here.",
        ],
        ..card("Quarantine whitelist", "Pick the users that can touch quarantined members and bots.", &[("antinuke.quarantine_whitelist", "Users")], |_, _| {
            text("Timeout, kick, ban and role changes on a quarantined member are blocked for everyone else.")
        })
    },
];

pub const JOINGATE: &[Card] = &[Card {
    when: &[("joingate.min_age_override_ms", "joingate.account_age.enabled"), ("joingate.username_strict", "joingate.username_filter.enabled"), ("joingate.username_terms", "joingate.username_filter.enabled")],
    ..boxed(card("Join Gate status", "If you want to use instant detection filters for new members joining your server, this is the module to tweak.", &[
    ("joingate.enabled", "Enable Join Gate"),
    ("joingate.unauthorized_bot.enabled", "Bots added by unauthorized members"),
    ("joingate.account_age.enabled", "Accounts younger than the minimum age"),
    ("joingate.min_age_override_ms", "Minimum account age"),
    ("joingate.no_avatar.enabled", "Accounts that have no profile picture"),
    ("joingate.unverified_bot.enabled", "Discord unverified bots added by anyone"),
    ("joingate.invite_in_name.enabled", "Accounts with invite links in usernames"),
    ("joingate.suspicious.enabled", "Accounts TommyBot deems suspicious"),
    ("joingate.spammer.enabled", "Accounts Discord flags as likely spammers"),
    ("joingate.username_filter.enabled", "Accounts that have certain names"),
    ("joingate.username_strict", "Strict words"),
    ("joingate.username_terms", "Wildcard words"),
], |c, _| view! { {text(if on(c, "joingate.enabled", false) { "New members are checked the moment they join." } else { "Join Gate is off." })}{show("joingate_kick")} }.into_any()))
}];

pub const JOINRAID: &[Card] = &[Card {
    alert: Some("Anti raid is more than one switch! Fine-tune it after the wizard on the Join Raid page in the menu on the left."),
    ..boxed(card("Anti raid status", "If you want to keep your server safe and secure from bad raids, this is your saviour.", &[
    ("joinraid.enabled", "Enable Join Raid"),
    ("joinraid.count", "Joins that count as a raid"),
    ("joinraid.span_ms", "Within (milliseconds)"),
    ("joinraid.lock", "Lock when a raid starts"),
], |_, _| view! { {show("raid")}{show("raid_report")} }.into_any()))
}];

pub const VERIFICATION: &[Card] = &[Card {
    notes: &["Turn it on and save: TommyBot makes the #verify-here channel with its panel."],
    action: Some(("Set up channel", "wizard/verify-channel")),
    ..card(
        "Verification status",
        "Do you want to filter in only genuine members? Use the verification and fend off bots!",
        &[
            ("verification.enabled", "Enable Verification"),
            ("verification.mode", "Verification mode"),
        ],
        |c, _| {
            // The panel, then what a member sees after pressing Verify.
            let (chip, after) = match c["verification"]["mode"].as_str().unwrap_or("Captcha") {
                "Web" => (Some(("ok", "This is recommended!")), show("verify_web")),
                "Dm" => {
                    let mut dm = sample("verify_captcha");
                    dm["flags"] = Value::Null;
                    (
                        None,
                        view! { {show("verify_dm")}<p class="muted">"In DMs:"</p>{bot(&dm)} }
                            .into_any(),
                    )
                }
                "Button" => (None, show("verify_button")),
                "SimpleCaptcha" => (Some(("bad", "This is very weak!")), show("verify_simple")),
                _ => (None, show("verify_captcha")),
            };
            view! { {chip.map(|(k, t)| view! { <span class=format!("chip {k}")>{t}</span> })}{show("verify_panel")}<p class="muted">"After pressing Verify:"</p>{after}<p class="muted">"After pressing Help:"</p>{show("verify_help")} }.into_any()
        },
    )
}];

/// A sample with `{key}` placeholders filled in; null while the samples load.
fn filled(key: &str, with: &[(&str, &str)]) -> Value {
    fill(&sample(key), with)
}

fn fill(m: &Value, with: &[(&str, &str)]) -> Value {
    let mut s = m.to_string();
    for (k, v) in with {
        let v = serde_json::to_string(v).unwrap_or_default();
        s = s.replace(&format!("{{{k}}}"), &v[1..v.len() - 1]);
    }
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// The member's DMs and the staff channel side by side, as the ticket settings make them.
fn ticket_preview(c: &Value, dc: &Value) -> AnyView {
    let t = &c["tickets"];
    let anon = if on(c, "tickets.anonymous", false) {
        "_anon"
    } else {
        ""
    };
    let tagged = |k: &str| {
        t[k].as_str()
            .unwrap_or("")
            .replace("{username}", "member")
            .replace("{userid}", "1001")
            .replace("{usermention}", "@member")
    };
    let (greeting, closing) = (tagged("greeting"), tagged("closing"));
    let mut pings: Vec<String> = strs(&t["ping_roles"])
        .iter()
        .map(|r| name_of(dc, "roles", r))
        .map(|n| {
            if n.starts_with('@') {
                n
            } else {
                format!("@{n}")
            }
        })
        .collect();
    if on(c, "tickets.ping_here", false) {
        pings.push("@here".into());
    }
    let mut intro = filled(
        if on(c, "tickets.command_only", false) {
            "ticket_intro_cmd"
        } else {
            "ticket_intro"
        },
        &[("prefix", c["misc"]["prefix"].as_str().unwrap_or("t!"))],
    );
    if !intro.is_null() && !pings.is_empty() {
        intro["content"] = Value::String(pings.join(" "));
    }
    view! {
        <p class="muted">"In the member's DMs:"</p>
        {chat("member", text("Hi, someone in the server keeps sending me scam links."))}
        {show("ticket_confirm")}{show("ticket_sent")}
        {(!greeting.is_empty()).then(|| show_msg(filled("ticket_greeting", &[("text", &greeting)])))}
        {show(&format!("ticket_reply{anon}"))}
        {(!closing.is_empty()).then(|| show_msg(filled("ticket_closing", &[("text", &closing)])))}
        {show(&format!("ticket_closed{anon}"))}
        <p class="muted">"In the ticket channel, for staff:"</p>
        {show_msg(intro)}
        {show("ticket_received")}{show(&format!("ticket_reply_copy{anon}"))}
    }.into_any()
}

pub const TICKETS: &[Card] = &[Card {
    notes: &["Members open a ticket by sending TommyBot a DM. Turn it on and save: TommyBot makes the Tickets category and #ticket-logs for you."],
    ..boxed(card("Tickets status", "Do you want members to reach your staff by sending TommyBot a DM?", &[
        ("tickets.enabled", "Enable Tickets"),
        ("tickets.category", ""),
        ("tickets.log_channel", ""),
        ("tickets.ping_roles", ""),
        ("tickets.ping_here", ""),
        ("tickets.greeting", ""),
        ("tickets.closing", ""),
        ("tickets.anonymous", ""),
        ("tickets.command_only", ""),
    ], ticket_preview))
}];

/// Setup score in percent: 17 points over the steps, as the wizard ring shows it.
/// `c` is the config, `dc` the server's roles with `bot_roles`, `guild` the id of @everyone.
pub fn score(c: &Value, dc: &Value, guild: &str) -> usize {
    let set = |v: &Value| {
        v.as_str().is_some_and(|s| !s.is_empty()) || v.as_array().is_some_and(|a| !a.is_empty())
    };
    let on = |p: &str| on(c, p, false);
    // Admins/Moderators permits only count once they have a role.
    let permits = !c["permits"]["custom"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|p| matches!(p["name"].as_str(), Some("Admins" | "Moderators")) && !set(&p["roles"]));
    let wl = &c["whitelist"];
    let whitelisted = [wl, &wl["invites"], &wl["mentions"], &wl["everyone"]]
        .iter()
        .any(|w| {
            ["users", "roles", "channels", "categories", "webhooks"]
                .iter()
                .any(|k| set(&w[*k]))
        });
    let roles = dc["roles"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mine: Vec<&str> = dc["bot_roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .chain([guild])
        .collect();
    let perms = roles
        .iter()
        .filter(|r| r["id"].as_str().is_some_and(|id| mine.contains(&id)))
        .filter_map(|r| r["permissions"].as_str()?.parse::<u64>().ok())
        .fold(0, |a, p| a | p);
    let pos = |r: &Value| r["position"].as_i64().unwrap_or(0);
    let top = roles
        .iter()
        .filter(|r| {
            r["id"]
                .as_str()
                .is_some_and(|id| id != guild && mine.contains(&id))
        })
        .map(pos)
        .max();
    // The bot's highest role is among the server's top 8 roles.
    let high = top.is_some_and(|t| roles.iter().filter(|r| pos(r) > t).count() < 8);
    let points = [
        2,
        set(&c["logging"]["fallback"]) as usize,
        set(&c["logging"]["routes"]["mod_command"]) as usize,
        set(&c["lockdown"]["main_roles"]) as usize,
        permits as usize,
        4,
        whitelisted as usize,
        on("antinuke.enabled") as usize,
        on("joingate.enabled") as usize,
        on("joinraid.enabled") as usize,
        on("verification.enabled") as usize,
        // Administrator alone (bit 3), counting @everyone's permissions too.
        (perms == 8) as usize,
        high as usize,
    ];
    (points.iter().sum::<usize>() * 100 + 17 / 2) / 17
}

#[cfg(test)]
mod tests {
    #[test]
    fn score_counts_the_seventeen_points() {
        use serde_json::json;
        let empty = json!({});
        // The six free points, and the permits point: no Admins/Moderators permit lacks a role.
        assert_eq!(super::score(&empty, &empty, "1"), 41);
        let c = json!({
            "logging": {"fallback": "5", "routes": {"mod_command": "6"}},
            "lockdown": {"main_roles": ["7"]},
            "permits": {"custom": [{"name": "Admins", "roles": ["8"]}]},
            "whitelist": {"mentions": {"users": ["9"]}},
            "antinuke": {"enabled": true}, "joingate": {"enabled": true}, "joinraid": {"enabled": true}, "verification": {"enabled": true},
        });
        let dc = json!({"bot_roles": ["2"], "roles": [
            {"id": "1", "position": 0, "permissions": "0"},
            {"id": "2", "position": 3, "permissions": "8"},
        ]});
        assert_eq!(super::score(&c, &dc, "1"), 100);
        // @everyone with permissions of its own, and a permit without a role.
        let dc = json!({"bot_roles": ["2"], "roles": [{"id": "1", "position": 0, "permissions": "1024"}, {"id": "2", "position": 3, "permissions": "8"}]});
        let mut c2 = c.clone();
        c2["permits"]["custom"][0]["roles"] = json!([]);
        assert_eq!(super::score(&c2, &dc, "1"), 88);
    }

    #[test]
    fn placeholders_take_text_that_needs_escaping() {
        let m = serde_json::json!({"embeds": [{"description": "Hi {text} via `{prefix}reply`"}]});
        let out = super::fill(&m, &[("text", "say \"hi\"\nnow"), ("prefix", "!")]);
        assert_eq!(
            out["embeds"][0]["description"],
            "Hi say \"hi\"\nnow via `!reply`"
        );
    }
}
