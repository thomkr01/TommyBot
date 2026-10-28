//! Wick's wizard cards: a few settings at a time with a live preview of what they change.
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
}

const fn card(title: &'static str, desc: &'static str, fields: &'static [(&'static str, &'static str)], preview: fn(&Value, &Value) -> AnyView) -> Card {
    Card { title, desc, notes: &[], fields, preview, action: None }
}

/// A fake Discord message.
fn chat(author: &str, body: AnyView) -> AnyView {
    view! { <div class="dmsg"><b>{author.to_string()}</b>" "<span class="muted">"Today at 09:11"</span><div>{body}</div></div> }.into_any()
}

fn bot(body: AnyView) -> AnyView {
    chat("TommyBot", body)
}

/// A fake embed: colour bar, bold title, lines.
fn embed(color: &str, title: &str, lines: Vec<String>) -> AnyView {
    view! { <div class="dembed" style=format!("border-color:{color}")><b>{title.to_string()}</b>{lines.into_iter().map(|l| view! { <div>{l}</div> }).collect_view()}</div> }.into_any()
}

/// The message box with something typed in it.
fn typed(text: String) -> AnyView {
    view! { <div class="dinput">{text}</div> }.into_any()
}

fn text(t: &str) -> AnyView {
    t.to_string().into_any()
}

fn on(c: &Value, path: &str, default: bool) -> bool {
    path.split('.').fold(c, |v, k| &v[k]).as_bool().unwrap_or(default)
}

pub const MISC: &[Card] = &[
    card("Prefix", "Choose a prefix that your staff members can memorize easily, the prefix . is usually recommended.", &[("misc.prefix", "Prefix")], |c, _| {
        let p = c["misc"]["prefix"].as_str().unwrap_or("w!");
        view! { {chat("you", text("Your mom gay"))}{typed(format!("{p}ban @member ?r toxic"))} }.into_any()
    }),
    card("Slash commands", "Enable TommyBot's slash commands.", &[("misc.use_slash", "Enable slash commands")], |c, _| {
        if on(c, "misc.use_slash", true) { typed("/ban user: @member reason: toxic".into()) } else { text("Slash commands are off, only the prefix works.") }
    }),
    card("Compact mode", "Do you want TommyBot's messages to have a compact style?", &[("misc.compact_replies", "Minify TommyBot's message replies")], |c, _| {
        let lines = if on(c, "misc.compact_replies", false) {
            vec!["✅ Successful bans: 1".into(), "❌ Unsuccessful bans: 0".into(), "Reason: toxic".into(), "Moderator: @you".into()]
        } else {
            vec!["Reason: toxic".into(), "Moderator: @you".into(), "Details:".into(), "▸ Duration: ❌".into(), "▸ Soft Ban: ❌".into(), "▸ Hack Ban: ✅".into(), "▸ DM Members: ❌".into(), "✅ Successful bans: member".into(), "❌ Unsuccessful bans: All users were banned!".into()]
        };
        bot(embed("var(--ok)", if on(c, "misc.compact_replies", false) { "" } else { "Ban Result:" }, lines))
    }),
    Card {
        notes: &["If you want TommyBot to make the log channel for you, leave it empty and run Auto Setup."],
        ..card("Logging channel", "Where do you want TommyBot to log its actions?", &[("logging.fallback", "Logs channel")], |c, dc| {
            let ch = name_of(dc, "channels", c["logging"]["fallback"].as_str().unwrap_or(""));
            view! { <p class="muted">{format!("Logs go to {}", if ch.is_empty() { "nowhere yet".into() } else { ch })}</p>{bot(embed("var(--warn)", "member has been warned!", vec!["▸ Factor: Use of blacklisted words".into(), "▸ Channel: #general".into(), "▸ Member: @member".into(), "▸ Warns: 1/5".into()]))} }.into_any()
        })
    },
    Card {
        notes: &["If you want TommyBot to make the modlog channel for you, leave it empty and run Auto Setup."],
        ..card("Moderation logs channel", "Where do you want TommyBot to log moderation actions made by your staff members?", &[("logging.routes.mod_command", "Modlogs channel")], |_, _| {
            bot(embed("var(--bad)", "Case #4151 | Ban", vec!["▸ Target: @member".into(), "▸ Moderator: @you".into(), "▸ Reason: DM ads".into()]))
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
    card("Automod status", "Do you want TommyBot to monitor messages?", &[
        ("automod.enabled", "Enable Automod"),
        ("automod.filters.invites.enabled", "Moderate invite links posted"),
        ("automod.filters.nsfw_links.enabled", "Filter out NSFW links"),
        ("automod.filters.bad_sites.enabled", "Filter out malicious links"),
        ("automod.filters.scam_links.enabled", "Filter out Nitro and Steam scam links"),
    ], |_, _| bot(embed("var(--bad)", "member has been kicked!", vec!["▸ Reason: Posting an invite link".into()]))),
    card("Antispam module", "Do you want TommyBot to also monitor and fight off spam in your server?", &[
        ("automod.anti_spam", "Enable Anti Spam"),
        ("automod.filters.mentions.enabled", "Track and stop @mention spam"),
        ("automod.filters.attachments.enabled", "Track and stop attachment (image, embed, file) spam"),
        ("automod.filters.similar.enabled", "Monitor repetitive message spam"),
        ("automod.filters.chars_lower.enabled", "Monitor long messages"),
        ("automod.filters.emojis.enabled", "Monitor emoji spam"),
        ("automod.filters.newlines.enabled", "Monitor new lines in messages"),
        ("automod.filters.zalgo.enabled", "Monitor zalgo in messages"),
    ], |_, _| bot(embed("var(--warn)", "member has been timed out!", vec!["▸ Reason: Spamming the same message".into()]))),
    card("Webhook coverage", "Do you want TommyBot to also monitor webhooks sending messages in your server?", &[("automod.monitor_webhooks", "Monitor unwhitelisted webhooks")], |c, _| {
        text(if on(c, "automod.monitor_webhooks", true) { "Webhook messages count towards spam like members' messages do." } else { "Webhook messages are ignored." })
    }),
];

pub const ANTINUKE: &[Card] = &[
    card("Antinuke status", "Do you want TommyBot to watch your staff members and stop anyone nuking the server?", &[
        ("antinuke.enabled", "Enable Antinuke"),
        ("antinuke.prune", "Detect and react to member prunes"),
        ("antinuke.vanity_guard", "Guard the server's vanity url"),
    ], |_, _| bot(embed("var(--bad)", "Anti Nuke", vec!["▸ Responsible: @rogue-admin".into(), "▸ Action: deleted 5 channels in 10 seconds".into(), "✅ Quarantined".into()]))),
    card("Auto quarantine", "Quarantine anyone who gives dangerous permissions without being allowed to.", &[
        ("antinuke.auto_quarantine", "Use the auto quarantine"),
        ("antinuke.quarantine_role", "Quarantine role"),
        ("antinuke.strict_mode", ""),
        ("antinuke.monitor_public_roles", ""),
        ("antinuke.monitor_channel_perms", ""),
    ], |c, dc| text(&format!("Quarantined members only keep {}.", name_of(dc, "roles", c["antinuke"]["quarantine_role"].as_str().unwrap_or("the quarantine role"))))),
    Card {
        notes: &[
            "ONLY owners and trusted admins are whitelisted automatically.",
            "This whitelist lets a member or bot edit the roles of someone in Quarantine. It does not stop them getting quarantined, that is Immunity in Permits.",
            "Autorole bots can be added here.",
        ],
        ..card("Quarantine whitelist", "Pick the users that can touch quarantined members and bots.", &[("antinuke.quarantine_whitelist", "Users (IDs)")], |_, _| {
            text("Timeout, kick, ban and role changes on a quarantined member are blocked for everyone else.")
        })
    },
    card("Ticket categories", "Do you use a ticket bot? Pick the categories it opens and closes ticket channels in, so the antinuke does not punish it.", &[("antinuke.ticket_categories", "Ticket categories")], |c, dc| {
        let picked: Vec<String> = strs(&c["antinuke"]["ticket_categories"]).iter().map(|id| name_of(dc, "categories", id)).collect();
        if picked.is_empty() { text("No ticket categories: every channel a bot creates or deletes counts.") } else { text(&format!("Channels created or deleted in {} never count.", picked.join(", "))) }
    }),
];

const AUTO_WL: &str = "Owners, Trusted admins and members with dangerous Discord/TommyBot permissions are automatically whitelisted.";
const STAFF_WL: &str = "ONLY owners and trusted admins are whitelisted automatically, your other staff members are targeted if not whitelisted.";

macro_rules! wl {
    ($p:literal) => {
        &[(concat!("whitelist.", $p, "users"), "Members (IDs)"), (concat!("whitelist.", $p, "roles"), "Roles"), (concat!("whitelist.", $p, "channels"), "Channels"), (concat!("whitelist.", $p, "categories"), "Categories"), (concat!("whitelist.", $p, "webhooks"), "Webhooks (IDs)")]
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
];

pub const JOINGATE: &[Card] = &[card("Join Gate status", "If you want to use instant detection filters for new members joining your server, this is the module to tweak.", &[
    ("joingate.enabled", "Enable Join Gate"),
    ("joingate.unauthorized_bot.enabled", "Bots added by unauthorized members"),
    ("joingate.account_age.enabled", "Accounts younger than the minimum age"),
    ("joingate.no_avatar.enabled", "Accounts that have no profile picture"),
    ("joingate.unverified_bot.enabled", "Discord unverified bots added by anyone"),
    ("joingate.invite_in_name.enabled", "Accounts with invite links in usernames"),
    ("joingate.suspicious.enabled", "Accounts TommyBot deems suspicious"),
    ("joingate.username_filter.enabled", "Accounts that have certain names"),
], |c, _| text(if on(c, "joingate.enabled", true) { "New members are checked the moment they join." } else { "These don't matter now! Join Gate is off." }))];

pub const JOINRAID: &[Card] = &[card("Anti raid status", "If you want to keep your server safe and secure from bad raids, this is your saviour.", &[
    ("joinraid.enabled", "Enable Join Raid"),
    ("joinraid.executors", "Joins that count as a raid"),
    ("joinraid.window_secs", "Within"),
    ("joinraid.lock", "Lock when a raid starts"),
], |_, _| bot(embed("var(--bad)", "Join raid detected!", vec!["▸ 30 accounts joined in 10 seconds".into(), "✅ Raiders removed".into()])))];

pub const VERIFICATION: &[Card] = &[Card {
    notes: &["Save your changes first, then set up the channel."],
    action: Some(("Set up channel", "wizard/verify-channel")),
    ..card("Verification status", "Do you want to filter in only genuine members? Use the verification and fend off bots!", &[("verification.enabled", "Enable Verification"), ("verification.mode", "Verification mode")], |c, _| {
        let panel = |button: &'static str| bot(view! { {embed("var(--primary)", "Verification Required!", vec!["To access the server, you need to pass the verification first.".into()])}<button class="go">{button}</button> }.into_any());
        match c["verification"]["mode"].as_str().unwrap_or("Captcha") {
            "Web" | "ThirdParty" => view! { <span class="chip ok">"This is recommended!"</span><div class="dembed"><b>"VERIFICATION"</b><div>"☐ I am human"</div></div> }.into_any(),
            "Dm" => view! { <p class="muted">"The captcha is sent in DMs."</p>{panel("Verify")} }.into_any(),
            "Button" => view! { <span class="chip warn">"This is very weak!"</span>{panel("Instant Access")} }.into_any(),
            _ => view! { <p class="muted">"# verify-here"</p>{panel("Verify")} }.into_any(),
        }
    })
}];
