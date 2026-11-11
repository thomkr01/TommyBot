//! Typed form fields per config section, served to the dashboard UI.
use serde_json::{json, Value};
use tommybot_antinuke::Action as NukeAction;
use tommybot_automod::{
    heat::HeatCfg,
    join::{Action as GateAction, Charset, Filter as GateFilter, GateCfg},
    strike::StrikeCfg,
};
use tommybot_core::{limits, sections::FILTERS_OFF_BY_DEFAULT};
use tommybot_logging::LogEvent;

const LOG_KINDS: &[&str] = &[
    "message",
    "member",
    "role",
    "channel",
    "server",
    "invite",
    "webhook",
    "emoji",
    "sticker",
    "scheduled_event",
    "thread",
    "report",
    "auto_mod",
    "anti_nuke",
    "verification",
    "join_gate",
    "join_raid",
    "panic",
    "mod_command",
    "appeal",
    "voice",
];

fn f(path: &str, kind: &str) -> Value {
    json!({"path": path, "label": path.replace(['.', '_'], " "), "kind": kind})
}

/// `f` placed in a named card instead of the one its path prefix would give.
fn fg(group: &str, path: &str, kind: &str) -> Value {
    let mut v = f(path, kind);
    v["group"] = json!(group);
    v
}

/// Shows `default` for a field the stored config leaves out (matches the Rust `Default`).
fn dv(mut v: Value, default: impl Into<Value>) -> Value {
    v["default"] = default.into();
    v
}

/// Puts a field on a dashboard page of its section (no page: the General page).
fn pg(mut v: Value, page: &str) -> Value {
    v["page"] = json!(page);
    v
}

/// Overrides the label the field shows (default: the last path segment).
fn ti(mut v: Value, title: &str) -> Value {
    v["title"] = json!(title);
    v
}

/// Help text under the field.
fn hp(mut v: Value, help: &str) -> Value {
    v["help"] = json!(help);
    v
}

/// A field stored in another section, shown on this section's page (saved with it).
fn sx(section: &str, mut v: Value) -> Value {
    v["section"] = json!(section);
    v
}

/// Verification modes that use the captcha card (Instant Access only during a join raid, so it rings red).
const IMAGE_MODES: &[&str] = &["Captcha", "Dm"];
/// Verification modes that show the helpful-notes switch (hidden for Website and Instant Access).
const NOTE_MODES: &[&str] = &["Captcha", "Dm", "SimpleCaptcha"];

/// `key` "only" hides the field and "used" rings its card red unless the value at `path` is one of `vals`.
fn unless(mut v: Value, key: &str, path: &str, vals: &[&str]) -> Value {
    v[key] = json!({"path": path, "in": vals});
    v
}

/// Log types as one channel picker each (true) or as cards added through a Customize dialog (false).
const LOG_TYPE_PICKERS: bool = false;

/// "auto_mod" -> "Auto Mod".
fn title_case(k: &str) -> String {
    k.split('_')
        .map(|w| w[..1].to_uppercase() + &w[1..])
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Customize dialog writing `{key: channel}` into the object at `path`: pick one option (or tick
/// several when `multi`), then one channel.
fn route_pick(group: &str, path: &str, opts: Vec<Value>, multi: bool) -> Value {
    let mut v = fg(group, path, "route_pick");
    v["options"] = json!(opts);
    v["multi"] = json!(multi);
    v
}

/// A choice field with its options; "" is the unset option.
fn choice(group: &str, path: &str, opts: &[&str]) -> Value {
    let mut v = fg(group, path, "choice");
    v["options"] = json!(opts);
    v
}

/// Ticks several of `opts`; stored as an array of the ticked ones.
fn multi(group: &str, path: &str, opts: &[&str]) -> Value {
    let mut v = fg(group, path, "multi");
    v["options"] = json!(opts);
    v
}

/// A heat weight with its default and the "N messages before a timeout" helper.
fn heat_field(group: &str, w: &str, page: &str, noun: &str) -> Value {
    let heat = serde_json::to_value(HeatCfg::default()).unwrap_or_default();
    let mut v = pg(
        dv(fg(group, &format!("heat.{w}"), "float"), heat[w].clone()),
        page,
    );
    v["helper"] = json!({
        "noun": noun,
        "max": heat["max"],
        "decay": heat["decay_per_sec"],
        "strike_ms": StrikeCfg::default().strike_timeout_ms,
    });
    v
}

/// (filter key, dashboard page, card title).
const FILTERS: &[(&str, &str, &str)] = &[
    ("regular", "spam_text", "Regular message"),
    ("similar", "spam_text", "Similar message"),
    ("emojis", "spam_text", "Emojis"),
    ("newlines", "spam_text", "New lines"),
    ("chars_upper", "spam_text", "Characters (uppercase)"),
    ("chars_lower", "spam_text", "Characters (lowercase)"),
    ("zalgo", "spam_text", "Zalgo"),
    ("suspicion", "spam_text", "Suspicion"),
    ("inactive_channel", "spam_text", "Inactive channel"),
    ("invites", "invites", "Invite links"),
    ("bad_sites", "malicious", "Bad websites"),
    ("scam_links", "malicious", "Scam links and login QR codes"),
    ("nsfw_links", "nsfw", "NSFW links"),
    ("words.filter", "words", "Word blacklist"),
    ("links", "links", "Link blacklist"),
    ("mentions", "spam_mentions", "General"),
    ("attachments", "spam_attach", "General"),
];
/// Heat weights shown on the card of the filter they feed, with the helper's noun.
const HEAT_FOR: &[(&str, &str, &str)] = &[
    ("regular", "base", "messages"),
    ("similar", "similarity", "similar messages"),
    ("emojis", "per_emoji", "emojis"),
    ("newlines", "per_newline", "new lines"),
    ("chars_upper", "per_upper", "uppercase characters"),
    ("chars_lower", "per_lower", "lowercase characters"),
    ("chars_lower", "per_char", "characters past the free ones"),
    ("zalgo", "zalgo", "zalgo messages"),
    ("suspicion", "suspicious_account", "messages"),
    ("inactive_channel", "inactive_channel", "messages"),
    ("invites", "invite", "invites"),
    ("scam_links", "malicious", "scam links"),
    ("nsfw_links", "nsfw", "NSFW links"),
    ("words.filter", "per_blacklisted_word", "blacklisted words"),
    ("links", "blacklisted_link", "blacklisted links"),
];
/// Mention and attachment kinds: (filter key, page, card title, heat weight, helper noun).
/// Their action lives on the tab's General card.
const SUBS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "mention_users",
        "spam_mentions",
        "@user mentions",
        "per_mention",
        "mentions",
    ),
    (
        "mention_roles",
        "spam_mentions",
        "@role mentions",
        "per_role_mention",
        "role mentions",
    ),
    (
        "mention_everyone",
        "spam_mentions",
        "@everyone mentions",
        "per_everyone",
        "@everyone mentions",
    ),
    ("embeds", "spam_attach", "Embeds", "per_embed", "embeds"),
    ("images", "spam_attach", "Images", "per_image", "images"),
    ("files", "spam_attach", "Files", "per_file", "files"),
    ("attach_links", "spam_attach", "Links", "per_link", "links"),
    (
        "stickers",
        "spam_attach",
        "Stickers",
        "per_sticker",
        "stickers",
    ),
];
/// "" leaves the decision to the strike system.
const FILTER_ACTIONS: &[&str] = &["delete", "warn", "timeout", "kick", "ban", ""];
/// Anti-nuke filter cards: (action, card title). Kicks count on the Ban card.
const NUKE_CARDS: &[(&str, &str)] = &[
    ("Ban", "Kicks & Bans"),
    ("RoleCreate", "Role creations"),
    ("RoleDelete", "Role deletions"),
    ("ChannelCreate", "Channel creations"),
    ("ChannelDelete", "Channel deletions"),
    ("WebhookCreate", "Webhook creations"),
    ("WebhookDelete", "Webhook deletions"),
    ("Timeout", "Member timeouts"),
    ("ChannelUpdate", "Channel updates"),
    ("RoleUpdate", "Role renames"),
    ("EmojiDelete", "Emoji deletions"),
    ("StickerDelete", "Sticker deletions"),
    ("AutoModRuleDelete", "AutoMod rule deletions"),
    ("AutoModRuleUpdate", "AutoMod rule updates"),
    ("Unban", "Unbans"),
];
const GATE_ACTIONS: &[&str] = &["Log", "Warn", "Timeout", "Kick", "Ban"];

/// Typed form fields per section; paths mirror the serde types validated in `api::validate`.
/// Plain-language hover text for settings that are hard to read from their name: (section, path or
/// last path part, text). Shown by the info icon next to the field title.
const TIPS: &[(&str, &str, &str)] = &[
    ("joingate", "require_all", "Normally one matching filter is enough to stop a new member. With this on, a member is only stopped when ALL the filters you turned on match."),
    ("joingate", "exclude_trusted_bots", "Bots you or an Extra Owner add skip the checks. Bots added by anyone else are still checked."),
    ("joingate", "unauthorized_bot.enabled", "Someone without permission adds a bot to the server. TommyBot removes the bot right away."),
    ("joingate", "min_age_override_ms", "New accounts are often made for raids. Accounts younger than this are stopped at the door."),
    ("automod", "heat.max", "Every message adds \"heat\" to a member. When the heat reaches this number, the member gets a strike."),
    ("automod", "heat.decay_per_sec", "How fast heat cools down again. Higher means members can chat faster before they get a strike."),
    ("automod", "strike.multiplier_pct", "Each new strike in a short time makes the next timeout longer by this much. 200 % means it doubles."),
    ("automod", "strike.cap", "After this many strikes the member gets the longer CAP timeout instead of the normal one."),
    ("automod", "strike.reoffend_window_ms", "Strikes inside this time count together. After it, the count starts over."),
    ("automod", "strike.strike_expiry_ms", "How long a strike is remembered before it is forgotten."),
    ("automod", "heat.webhook_multiplier", "Messages from webhooks get this much more heat, because spam bots often use webhooks."),
    ("panic", "executors", "If this many accounts are quarantined in a short time, TommyBot treats it as an attack and starts Panic Mode. 0 turns this off."),
    ("panic", "heat_executors", "Same, but counted from members Auto Mod punished."),
    ("antinuke", "per_minute", "How many times one account may do this action (for example ban or delete a channel) before TommyBot stops it."),
    ("antinuke", "per_hour", "How many times one account may do this action (for example ban or delete a channel) before TommyBot stops it."),
    ("antinuke", "heat", "How much of the limit one action uses. 100 % means one action counts as reaching the limit."),
    ("joinraid", "count", "If this many members join within this time, TommyBot treats it as a raid."),
    ("joinraid", "span_ms", "If this many members join within this time, TommyBot treats it as a raid."),
    ("joinraid", "id_flag_min", "Raid accounts are often made at the same moment. TommyBot looks for many joins whose accounts were created close together."),
    ("lockdown", "main_roles", "The roles normal members have. Lockdown takes the chat permissions away from these roles."),
    ("whitelist", "partner_channels", "Channels where invite links from other servers are allowed."),
    ("ban_sync", "share_joinraid", "Members Join Raid bans here are banned in your other servers too."),
    ("ban_sync", "share_automod", "Members Auto Mod bans here are banned in your other servers too."),
    ("ban_sync", "share_joingate", "Join Gate filters are made for this server, so they may not fit your other servers. Off by default."),
    ("ban_sync", "share_verify", "These are usually real people who were too slow to verify. Off by default."),
    ("ban_sync", "receive", "What happens here when a member is banned in one of your other servers. Alert posts it in the mod log with a Ban here button. Ban bans them here as well."),
    ("raider_list", "enabled", "The Raider List is a list of raider accounts TommyBot caught across many servers. The bot owner checks every entry before it is used. Your server sees only the category and a short note, never other servers' evidence."),
    ("raider_list", "action", "What TommyBot does when a listed account joins. Alert posts it in the mod log with Ban, Kick and Quarantine buttons."),
    ("raider_list", "lift_on_removal", "When the bot owner takes someone off the list, for example after an appeal, TommyBot lifts the ban it made here."),
    ("raider_list", "no_collect", "Normally TommyBot also learns about raiders from what it catches here. Turn this on to keep this server out of that."),
];

pub fn schema() -> Vec<Value> {
    let mut log = vec![
        ti(
            pg(f("fallback", "channel"), "tommybot_essential"),
            "TommyBot logs",
        ),
        ti(
            pg(
                fg("Channels", "routes.mod_command", "channel"),
                "tommybot_essential",
            ),
            "Mod logs",
        ),
        pg(
            fg("Ignored", "ignored_channels", "channels"),
            "tommybot_customize",
        ),
    ];
    let kinds = LOG_KINDS.iter().filter(|k| **k != "mod_command").map(|k| {
        let help = match *k {
            "report" => "Message reports by your members will go to this channel where you and your moderators can interact and deal with them.",
            "auto_mod" => "All automod actions from TommyBot will be sent to this channel.",
            "anti_nuke" => "All antinuke actions from TommyBot will be sent to this channel.",
            "verification" => "All verification results from TommyBot will be sent to this channel.",
            "join_gate" => "Join Gate logs will go to this channel.",
            "join_raid" => "Join raid triggers and results will be sent to this channel.",
            "panic" => "Panic mode triggers and results will be sent to this channel.",
            "appeal" => "New ban and timeout appeals will be sent to this channel.",
            "voice" => "Voice joins, leaves and moves. Only logged once you pick a channel here.",
            _ => "Logs of this type will go to this channel instead of the TommyBot logs channel.",
        };
        (*k, help)
    });
    if LOG_TYPE_PICKERS {
        log.extend(kinds.map(|(k, help)| {
            pg(
                hp(fg("Log types", &format!("routes.{k}"), "channel"), help),
                "tommybot_customize",
            )
        }));
    } else {
        let opts: Vec<Value> = kinds
            .map(|(k, help)| json!({"key": k, "title": title_case(k), "help": help}))
            .collect();
        log.push(pg(
            ti(
                route_pick("Log types", "routes", opts, false),
                "Custom log channels",
            ),
            "tommybot_customize",
        ));
    }
    let events: Vec<Value> = LogEvent::ALL
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok()?.as_str().map(String::from))
        .map(|k| json!({"key": k, "title": title_case(&k)}))
        .collect();
    log.push(pg(
        ti(
            route_pick("Customize", "events", events, true),
            "Send several events to one channel",
        ),
        "discord_essential",
    ));
    log.extend(LogEvent::ALL.iter().map(|e| {
        let name = serde_json::to_value(e).unwrap_or(Value::Null);
        let key = name.as_str().unwrap_or_default();
        let group = match key.split('_').next().unwrap_or_default() {
            "channel" | "channels" => "Channels",
            "message" => "Messages",
            "member" | "bot" => "Members",
            "role" => "Roles",
            "invite" => "Invites",
            "webhook" => "Webhooks",
            "emoji" => "Emojis",
            "sticker" => "Stickers",
            "thread" => "Threads",
            "guild" if key == "guild_update" => "Server",
            "guild" => "Scheduled events",
            _ => "Other",
        };
        pg(
            fg(group, &format!("events.{key}"), "channel"),
            "discord_essential",
        )
    }));
    let mut custom = fg("Logging types", "custom", "customs");
    custom["options"] = json!(LogEvent::ALL
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect::<Vec<_>>());
    log.push(pg(custom, "discord_customize"));
    let mut nuke = vec![
        ti(fg("Status", "enabled", "bool"), "Use the antinuke module"),
        ti(fg("Status", "prune", "bool"), "Detect and react to member prunes"),
        ti(dv(fg("Status", "quarantine_hold", "bool"), true), "Punish anyone who touches a quarantined member"),
        ti(fg("Status", "vanity_guard", "bool"), "Have TommyBot guard your server's vanity url"),
        ti(dv(fg("Auto quarantine", "auto_quarantine", "bool"), true), "Use the auto quarantine module. A nuking bot stays in the server: its own role loses all permissions and it is quarantined. It is kicked only if that fails."),
        hp(
            ti(fg("Auto quarantine", "quarantine_role", "role"), "Quarantine role"),
            "Leave it empty and Auto Setup makes @Quarantine for you, deleting old Quarantine roles. Pick a role and Auto Setup removes all of its permissions and denies it in every channel.",
        ),
        ti(fg("Auto quarantine", "strict_mode", "bool"), "Strict mode: punish unauthorized admins giving dangerous perms to any role in the server"),
        ti(fg("Auto quarantine", "strict_member_roles", "bool"), "Strict member role addition: punish unauthorized admins giving dangerous perms to other members"),
        ti(fg("Auto quarantine", "monitor_public_roles", "bool"), "Monitor public roles: protect @everyone and main roles from getting dangerous permissions"),
        ti(fg("Auto quarantine", "monitor_channel_perms", "bool"), "Monitor channel permissions: protect @everyone and main roles from getting dangerous CHANNEL permissions"),
        hp(
            ti(fg("Auto quarantine", "quarantine_whitelist", "users"), "Quarantine whitelist"),
            "Members that may change the roles of quarantined members.",
        ),
        hp(
            ti(fg("Ticket categories", "ticket_categories", "categories"), "Ticket categories"),
            "Channels a ticket bot creates or deletes in these categories never count.",
        ),
    ];
    for (a, title) in NUKE_CARDS {
        let action = NukeAction::ALL.into_iter().find(|x| format!("{x:?}") == *a);
        let (m, h, heat) = action
            .and_then(NukeAction::default_limit)
            .unwrap_or_default();
        let card = *title;
        let at = |k: &str, kind: &str, title: &str| {
            ti(
                pg(fg(card, &format!("limits.{a}.{k}"), kind), "limits"),
                title,
            )
        };
        let opt_in = action.is_some_and(NukeAction::opt_in);
        let mut first = dv(at("enabled", "bool", "Monitor this action"), !opt_in);
        if *a == "Ban" {
            first = hp(first, "We absolutely recommend not giving your staff members native Discord Ban/Kick permissions, force them to use the bot's kick and ban commands that are supervised by the antinuke.");
        }
        if *a == "WebhookCreate" {
            first = hp(
                first,
                "Tracking webhooks created by people is very important.",
            );
        }
        nuke.extend([
            first,
            dv(at("per_minute", "int", "Minute limit"), m),
            dv(at("per_hour", "int", "Hour limit"), h),
            hp(
                dv(at("heat", "int", "Heat per action"), heat),
                "Only counts when Panic Mode and its heat option are on.",
            ),
        ]);
    }
    let mut auto = vec![
        ti(fg("Status", "enabled", "bool"), "Use the automod module"),
        ti(dv(fg("Status", "anti_spam", "bool"), true), "Use the anti spam"),
        ti(dv(fg("Status", "monitor_webhooks", "bool"), true), "Monitor unwhitelisted webhooks"),
        // Display only: the heat system is the one mode, the stored value is ignored.
        ti(dv(choice("Status", "mode", &["Heat System"]), "Heat System"), "AutoMod mode"),
        ti(dv(fg("Auto timeouts", "strike.enabled", "bool"), true), "Use the auto timeouts"),
        ti(dv(fg("Auto timeouts", "strike.strike_timeout_ms", "int"), 60_000), "Regular strike duration"),
        ti(dv(fg("Auto timeouts", "strike.cap_timeout_ms", "int"), 660_000), "CAP strike duration"),
        ti(dv(fg("Auto timeouts", "strike.cap", "int"), 3), "Strikes before the CAP strike"),
        ti(dv(fg("Auto timeouts", "strike.reoffend_window_ms", "int"), 3_600_000), "Reoffend window"),
        ti(dv(fg("Auto timeouts", "strike.strike_expiry_ms", "int"), 3_600_000), "Strike expiry"),
        ti(dv(fg("Auto timeouts", "strike.multiply", "bool"), false), "Multiply timeouts"),
        ti(dv(fg("Auto timeouts", "strike.multiplier_pct", "int"), 200), "Multiplier percentage"),
        ti(dv(fg("Heat settings", "heat.max", "float"), 100.0), "Max heat"),
        ti(dv(fg("Heat settings", "heat.decay_per_sec", "pct"), HeatCfg::default().decay_per_sec), "Heat degradation (every second)"),
        dv(fg("Heat settings", "heat.webhook_multiplier", "float"), 1.5),
        hp(
            ti(dv(fg("Heat settings", "strike.reset_heat_on_timeout", "bool"), false), "Set current heat to 0 after punishment"),
            "The filter that generated the most heat is picked as the reason, and its action is used.",
        ),
        ti(fg("Auto lockdown", "auto_lockdown.enabled", "bool"), "Use the auto lockdown module"),
        ti(dv(fg("Mentions lockdown", "auto_lockdown.mentions", "bool"), true), "Use the mentions auto lockdown trigger"),
        ti(dv(fg("Mentions lockdown", "auto_lockdown.mention_count", "int"), 50), "Trigger count (mentions)"),
        ti(dv(fg("Mentions lockdown", "auto_lockdown.mention_window_ms", "int"), 3_000), "Trigger count history"),
        hp(
            ti(fg("Strikes lockdown", "auto_lockdown.strikes", "int"), "Lock at this strike number"),
            "Empty: off.",
        ),
        sx(
            "joinraid",
            ti(choice("JoinRaid lockdown", "lock", &["", "Server", "Blind", "Role", "All"]), "Lock when a join raid starts"),
        ),
    ];
    for (k, page, title) in FILTERS {
        let mut action = pg(
            ti(
                choice(title, &format!("filters.{k}.action"), FILTER_ACTIONS),
                "Trigger action",
            ),
            page,
        );
        action["none_label"] = json!("strike system");
        let use_it = if *k == "mentions" {
            "Consider mentions found in messages"
        } else if *k == "attachments" {
            "Consider attachments found in messages"
        } else {
            "Use this filter"
        };
        auto.extend([
            pg(
                dv(
                    ti(fg(title, &format!("filters.{k}.enabled"), "bool"), use_it),
                    !FILTERS_OFF_BY_DEFAULT.contains(k),
                ),
                page,
            ),
            action,
            hp(
                pg(
                    ti(
                        fg(title, &format!("filters.{k}.duration_secs"), "int"),
                        "Action duration",
                    ),
                    page,
                ),
                "Empty uses the default timeout from Miscellaneous.",
            ),
        ]);
        auto.extend(
            HEAT_FOR
                .iter()
                .filter(|(f, ..)| f == k)
                .map(|(_, h, noun)| ti(heat_field(title, h, page, noun), "Heat added")),
        );
    }
    for (k, page, title, w, noun) in SUBS {
        auto.extend([
            pg(
                dv(
                    ti(
                        fg(title, &format!("filters.{k}.enabled"), "bool"),
                        "Use this filter",
                    ),
                    true,
                ),
                page,
            ),
            ti(heat_field(title, w, page, noun), "Heat added"),
        ]);
    }
    auto.extend([
        pg(
            dv(
                ti(
                    fg("Similar message", "heat.similarity_floor", "float"),
                    "Similarity ratio",
                ),
                0.8,
            ),
            "spam_text",
        ),
        pg(
            dv(
                ti(
                    fg("Characters (lowercase)", "heat.free_chars", "int"),
                    "Free characters",
                ),
                200,
            ),
            "spam_text",
        ),
        pg(
            dv(
                ti(
                    fg("Inactive channel", "filters.inactive_messages", "int"),
                    "Inactive messages",
                ),
                10,
            ),
            "spam_text",
        ),
        pg(
            dv(
                ti(
                    fg("Inactive channel", "filters.inactive_hours", "int"),
                    "Quiet hours",
                ),
                1,
            ),
            "spam_text",
        ),
        pg(
            dv(
                ti(
                    fg("General", "filters.mention_hour_cap", "int"),
                    "Hour cap (pings)",
                ),
                20,
            ),
            "spam_mentions",
        ),
        pg(
            fg("NSFW links", "filters.nsfw_crawl_shorteners", "bool"),
            "nsfw",
        ),
        pg(
            ti(
                fg("Word blacklist", "filters.words.strict", "list"),
                "Strict words",
            ),
            "words",
        ),
        pg(
            ti(
                fg("Word blacklist", "filters.words.wildcard", "list"),
                "Wildcard words",
            ),
            "words",
        ),
        pg(
            hp(
                ti(
                    fg("Word blacklist", "filters.words.regex", "lines"),
                    "Regex patterns",
                ),
                "One pattern per line, case-insensitive. Each matching pattern counts as one word.",
            ),
            "words",
        ),
        pg(
            ti(
                fg("Word blacklist", "filters.words.premade_profane", "bool"),
                "Use premade list of profane words",
            ),
            "words",
        ),
        pg(
            ti(
                fg("Word blacklist", "filters.words.premade_vulgar", "bool"),
                "Use premade list of vulgar words",
            ),
            "words",
        ),
        pg(
            ti(
                fg("Word blacklist", "filters.words.premade_racist", "bool"),
                "Use premade list of racist words",
            ),
            "words",
        ),
        pg(
            ti(
                fg(
                    "Word blacklist",
                    "filters.words.use_discord_automod",
                    "bool",
                ),
                "Use Discord's AutoMod keyword blacklist instead",
            ),
            "words",
        ),
        pg(
            ti(
                fg("Link blacklist", "filters.blacklisted_domains", "list"),
                "Links",
            ),
            "links",
        ),
    ]);
    let mut gate = vec![
        ti(fg("General", "enabled", "bool"), "Use the joingate module"),
        ti(
            fg("General", "dm_punished", "bool"),
            "DM members when punished",
        ),
        ti(
            fg("General", "require_all", "bool"),
            "Act only when every enabled filter matches",
        ),
        hp(
            ti(
                dv(fg("General", "exclude_trusted_bots", "bool"), false),
                "Exclude bots added by the owner or an Extra Owner",
            ),
            "Bots added by the owner or an Extra Owner skip the account filters. Trusted Admins and other admins do not count. If one of those accounts is taken over, a bot added through it is let in. Keep this off unless you need it.",
        ),
    ];
    // (key, card, status label, filter whose default action the select shows).
    for (k, card, status, filter) in [
        (
            "unauthorized_bot",
            "Bot additions",
            "Target bots added by unauthorized members",
            GateFilter::UnauthorizedBot,
        ),
        (
            "unverified_bot",
            "Unverified bot additions",
            "Target Discord unverified bots added by anyone",
            GateFilter::UnverifiedBot,
        ),
        (
            "no_avatar",
            "No avatar",
            "Target accounts that have no profile pictures",
            GateFilter::NoAvatar,
        ),
        (
            "suspicious",
            "Suspicious account",
            "Target accounts TommyBot deems suspicious",
            GateFilter::Suspicious,
        ),
        (
            "invite_in_name",
            "Advertising name",
            "Target accounts with invite links in usernames",
            GateFilter::InviteInName,
        ),
        (
            "account_age",
            "Account age",
            "Target accounts younger than the minimum account age",
            GateFilter::AccountAge,
        ),
        (
            "username_filter",
            "Username",
            "Target accounts whose username matches your list",
            GateFilter::UsernameFilter,
        ),
    ] {
        gate.extend([
            ti(fg(card, &format!("{k}.enabled"), "bool"), status),
            hp(
                ti(
                    dv(
                        choice(card, &format!("{k}.action"), GATE_ACTIONS),
                        match filter.default_action() {
                            GateAction::Timeout(_) => "Timeout".to_string(),
                            a => format!("{a:?}"),
                        },
                    ),
                    "Trigger action",
                ),
                "Kick is Discord's native kick feature, Timeout its native timeout.",
            ),
        ]);
    }
    gate.extend([
        {
            // One duration with a unit; an unset value shows the days setting it replaces.
            let days = GateCfg::default().min_age_days;
            let mut v = ti(
                dv(
                    fg("Account age", "min_age_override_ms", "int"),
                    i64::from(days) * 86_400_000,
                ),
                "Minimum account age",
            );
            v["fallback"] = json!({"path": "min_age_days", "scale": 86_400_000});
            v
        },
        ti(
            dv(fg("Account age", "expose_min_age", "bool"), true),
            "Tell punished members the minimum age in the DM",
        ),
        ti(
            fg("Username", "username_post_join", "bool"),
            "Enable this filter POST join too",
        ),
        hp(
            ti(fg("Username", "username_strict", "list"), "Strict words"),
            "Exact match, case insensitive.",
        ),
        hp(
            ti(fg("Username", "username_terms", "list"), "Wildcard words"),
            "Contains; * matches anything. Names are sanitized before matching.",
        ),
    ]);
    let raid = vec![
        fg("General", "enabled", "bool"),
        ti(dv(f("count", "int"), 10), "Minimum trigger (accounts)"),
        ti(
            dv(f("span_ms", "int"), 3 * 3_600_000),
            "Join history (milliseconds)",
        ),
        f("suspicious_only", "bool"),
        ti(
            dv(f("active_ms", "int"), 1_800_000),
            "Trigger duration (milliseconds)",
        ),
        f("string_patterns", "list"),
        ti(
            f("flag_younger_than_ms", "int"),
            "Flag accounts younger than (milliseconds)",
        ),
        f("flag_no_avatar", "bool"),
        ti(
            fg("ID flag", "id_flag_min", "int"),
            "Minimum matches (empty turns it off)",
        ),
        ti(
            dv(
                choice(
                    "ID flag",
                    "id_granularity",
                    &["Adaptive", "Hour", "Day", "Month"],
                ),
                "Adaptive",
            ),
            "Granularity",
        ),
        // Same as RaidCfg::default; the field is not optional, so emptying it must not save null.
        ti(
            dv(fg("ID flag", "id_margin_ms", "int"), 60 * 60_000),
            "Margin (milliseconds)",
        ),
        dv(
            choice(
                "Response",
                "action",
                &["Log", "Warn", "Timeout", "Kick", "Ban"],
            ),
            "Ban",
        ),
        choice("Response", "lock", &["", "Server", "Blind", "Role", "All"]),
        fg("Response", "warned_roles", "roles"),
    ];
    // "heat" is the Auto Mod page, "nuke" the Anti Nuke page; the fields without a page are shared.
    let panic = vec![
        ti(
            pg(dv(fg("Trigger", "executors", "int"), 0), "nuke"),
            "Quarantined accounts to trip",
        ),
        ti(
            pg(fg("Trigger", "heat_algorithm", "bool"), "heat"),
            "Panic on heat",
        ),
        ti(
            pg(dv(fg("Trigger", "heat_executors", "int"), 3), "heat"),
            "Punished accounts to trip",
        ),
        ti(
            pg(dv(fg("Trigger", "heat_decay", "float"), 0.01), "nuke"),
            "Heat degradation (every second)",
        ),
        dv(fg("Trigger", "window_secs", "int"), 600),
        dv(fg("Response", "duration_secs", "int"), 60),
        dv(fg("Response", "lock_mod_commands", "bool"), true),
        dv(fg("Response", "unlock_when_ending", "bool"), true),
        ti(
            dv(fg("Response", "lock_on_trigger", "bool"), true),
            "Lock the server on trigger",
        ),
        fg("Response", "warned_roles", "roles"),
        fg("Response", "whitelisted_categories", "categories"),
    ];
    let mut backups = vec![
        ti(
            dv(f("enabled", "bool"), false),
            "Backups status: allow backups to be made manually and automatically",
        ),
        ti(dv(f("auto", "bool"), true), "Automatic backups"),
        dv(f("max_images", "int"), 10),
        dv(f("interval_hours", "int"), 3),
    ];
    for (path, title) in [
        ("restore", "Restore after a nuke"),
        ("restore_roles", "Restore deleted roles"),
        ("delete_roles", "Delete created roles"),
        ("restore_channels", "Restore deleted channels"),
        ("delete_channels", "Delete created channels"),
        ("delete_webhooks", "Delete created webhooks"),
    ] {
        backups.push(ti(dv(fg("Restore", path, "bool"), true), title));
    }
    let mut whitelist = vec![];
    for (tab, prefix) in [
        ("", ""),
        ("invites", "invites."),
        ("mentions", "mentions."),
        ("everyone", "everyone."),
    ] {
        for (key, card, kind) in [
            ("users", "Members", "users"),
            ("roles", "Roles", "roles"),
            ("channels", "Channels", "channels"),
            ("categories", "Categories", "categories"),
            ("webhooks", "Webhooks", "list"),
        ] {
            let v = ti(fg(card, &format!("{prefix}{key}"), kind), card);
            whitelist.push(if tab.is_empty() { v } else { pg(v, tab) });
        }
    }
    // The wizard's whitelist step also holds these anti-nuke lists; page "wizard" keeps them off the whitelist tabs.
    for (path, kind, title) in [
        ("ticket_categories", "categories", "Categories"),
        ("quarantine_whitelist", "users", "Users"),
    ] {
        whitelist.push(sx("antinuke", pg(ti(f(path, kind), title), "wizard")));
    }
    let misc = vec![
        ti(dv(fg("General", "prefix", "text"), "t!"), "Prefix"),
        ti(dv(fg("General", "use_slash", "bool"), true), "Use slash commands"),
        ti(fg("General", "compact_replies", "bool"), "Minify TommyBot's message replies to compact embeds"),
        ti(fg("General", "delete_replies_secs", "int"), "Delete TommyBot's auto messages after few seconds"),
        ti(dv(fg("General", "enforce_quarantine_perms", "bool"), true), "Always enforce the proper permissions of Quarantine role in channels"),
        ti(fg("General", "ignore_no_permit", "bool"), "Always ignore commands issued by members without any permits"),
        ti(fg("General", "ban_appeal_message", "text"), "Message shown to banned members with the appeal link"),
        ti(fg("General", "enforce_permits", "bool"), "Remove roles with dangerous permissions from members without any permits"),
        sx(
            "logging",
            hp(
                ti(fg("Statics", "fallback", "channel"), "Logs channel"),
                "All logs that have no channel of their own go here.",
            ),
        ),
        sx(
            "logging",
            hp(
                ti(
                    fg("Statics", "routes.mod_command", "channel"),
                    "Modlogs channel",
                ),
                "Moderation actions done by staff with the bot go here as cases.",
            ),
        ),
        sx(
            "antinuke",
            hp(
                ti(fg("Statics", "quarantine_role", "role"), "Quarantine role"),
                "Quarantined members are moved to this role.",
            ),
        ),
        sx(
            "lockdown",
            hp(
                ti(fg("Statics", "main_roles", "roles"), "Main roles"),
                "The roles your members have; lockdown and verification use them.",
            ),
        ),
        sx(
            "whitelist",
            hp(
                ti(fg("Statics", "partner_channels", "channels"), "Partnering channels"),
                "Spam, @everyone pings and invites are allowed here.",
            ),
        ),
        hp(
            ti(dv(fg("Moderation commands", "confirm", "bool"), true), "Ask for a Yes/No confirmation when the target is given as an ID or a name"),
            "A @mention is never asked about. Only the moderator who ran the command can press Yes or No, and the question closes after 10 seconds. Purge, hackbans and note sweeps ask too.",
        ),
        hp(
            ti(dv(fg("Moderation commands", "always_dm", "bool"), true), "DM members when they are punished or when a punishment is removed"),
            "Punishment DMs are sent before the action, removal DMs (unquarantine, untimeout, unban) once it worked. If the member has closed DMs, the action still happens and the result shows DM-Members ❎. Unban DMs usually fail because the member no longer shares a server with TommyBot. Off: only ?dm sends one.",
        ),
        ti(fg("Moderation commands", "delete_invocation", "bool"), "Always delete the moderation execution messages for regular prefix commands"),
        ti(fg("Moderation commands", "hide_staff_in_dms", "bool"), "Hide the staff member responsible when DMing the punished member"),
        hp(
            ti(
                dv(fg("Moderation commands", "default_timeout_secs", "int"), 0),
                "Default timeout duration",
            ),
            "0 means 28 days",
        ),
        ti(dv(fg("Moderation commands", "ban_purge_days", "int"), 7), "Days purged on bans"),
        ti(dv(fg("Moderation commands", "remove_main_roles_on_timeout", "bool"), true), "Remove the main roles from the target on timeout"),
        ti(dv(fg("Auto moderation", "show_reason_on_auto", "bool"), true), "Display the reason behind the auto punishment in the message TommyBot sends"),
        ti(fg("Auto moderation", "purge_last_100", "bool"), "Always purge the messages sent by the punished member found in the recent 100 messages"),
        ti(fg("Auto moderation", "staff_only_permitted", "bool"), "Consider only owners, trusted admins and any member with the flags (IMMUNITY|AUTOMOD IMMUNITY) to be considered staff members. Punish anyone else triggering the auto mod"),
        ti(dv(fg("Auto warn actions", "auto_warn_actions", "bool"), true), "Issue an action once a member reaches certain points"),
        ti(dv(fg("Auto warn actions", "warn_base_points", "int"), 5), "Base points"),
        ti(fg("Auto warn actions", "warn_actions", "warn_actions"), "Custom actions"),
    ];
    let verification = vec![
        ti(
            fg("General", "enabled", "bool"),
            "Use the verification module",
        ),
        hp(
            ti(
                fg("General", "quarantine_on_join", "bool"),
                "Quarantine user on join (not recommended)",
            ),
            "Legacy: every joiner sits in quarantine until they verify.",
        ),
        hp(
            ti(
                dv(fg("General", "replace_roles", "bool"), false),
                "Set autoroles instead of adding them",
            ),
            "On a pass the member's roles are replaced by the verification roles.",
        ),
        dv(
            ti(
                choice("General", "target", &["Everyone", "SuspiciousOnly"]),
                "Verification targets",
            ),
            "Everyone",
        ),
        hp(
            dv(
                ti(
                    choice(
                        "General",
                        "on_fail",
                        &["Nothing", "Kick", "Ban", "Quarantine"],
                    ),
                    "Fail action",
                ),
                "Nothing",
            ),
            "Kick, Ban and Quarantine run when the verification duration ends after the join.",
        ),
        hp(
            ti(
                dv(fg("General", "audit_reason", "bool"), true),
                "Add a reason to the audit log when the time limit runs out",
            ),
            "Kicks, bans and quarantines at the time limit show \"Did not verify within the time limit\" in the server's audit log.",
        ),
        dv(
            ti(
                choice(
                    "General",
                    "mode",
                    &["Captcha", "Web", "Dm", "SimpleCaptcha", "Button"],
                ),
                "Verification mode",
            ),
            "Captcha",
        ),
        unless(
            ti(
                dv(fg("General", "show_notes", "bool"), true),
                "Display helpful notes alongside the captcha",
            ),
            "only",
            "verification.mode",
            NOTE_MODES,
        ),
        ti(
            fg("General", "regen_ms", "int"),
            "New captcha image every (milliseconds, empty is off)",
        ),
        hp(
            dv(
                ti(
                    fg("General", "time_limit_ms", "int"),
                    "Verification duration",
                ),
                120_000,
            ),
            "The time limit pauses while a member is timed out.",
        ),
        hp(
            ti(fg("General", "channel", "channel"), "Verification channel"),
            "If the verification channel is deleted, TommyBot makes a new one (up to 3 in 10 minutes), tells the owner, and quarantines whoever deleted it. Owners, Extra Owners and Trusted Admins only get a warning the first time.",
        ),
        ti(fg("General", "roles", "roles"), "Verification roles"),
        unless(
            ti(
                dv(fg("Captcha", "captcha.height", "int"), 100),
                "Captcha height",
            ),
            "used",
            "verification.mode",
            IMAGE_MODES,
        ),
        ti(
            dv(fg("Captcha", "captcha.width", "int"), 300),
            "Captcha width",
        ),
        ti(
            dv(fg("Captcha", "captcha.font_size", "int"), 40),
            "Font size",
        ),
        ti(
            dv(fg("Captcha", "captcha.font_color", "color"), "#32cf7e"),
            "Font color",
        ),
        ti(
            dv(fg("Captcha", "captcha.length", "int"), 6),
            "Captcha answer length",
        ),
        ti(
            dv(choice("Captcha", "captcha.charset", &Charset::NAMES), "AZ"),
            "Captcha characters",
        ),
        ti(
            dv(fg("Captcha", "captcha.trace", "bool"), true),
            "Show a colored trace that follows the captcha characters",
        ),
        ti(
            dv(fg("Captcha", "captcha.mixed", "bool"), true),
            "Add mixed noise over the captcha characters",
        ),
        ti(
            dv(fg("Captcha", "captcha.spread", "bool"), true),
            "Show small spread-around dots to trick scripts",
        ),
        ti(
            dv(fg("Captcha", "captcha.decoy_color", "color"), "#646566"),
            "Decoy color",
        ),
    ];
    let tickets =
        vec![
        ti(fg("General", "enabled", "bool"), "Members can DM TommyBot to open a ticket"),
        hp(
            ti(fg("General", "category", "category"), "Ticket category"),
            "Staff with the Manage Tickets permit or an access role see and answer every ticket.",
        ),
        ti(fg("General", "log_channel", "channel"), "Ticket log channel"),
        ti(
            dv(fg("General", "full_log", "bool"), true),
            "Full conversation in the ticket log",
        ),
        ti(fg("General", "access_roles", "roles"), "Access roles"),
        ti(fg("General", "ping_roles", "roles"), "Roles pinged on a new ticket"),
        ti(fg("General", "ping_here", "bool"), "Ping @here on a new ticket"),
        hp(
            ti(fg("Messages", "greeting", "text"), "Greeting message"),
            "Sent to the member when a ticket opens. Tags: {username} {userid} {usermention}",
        ),
        ti(fg("Messages", "closing", "text"), "Closing message"),
        ti(fg("Replies", "anonymous", "bool"), "Anonymous replies by default"),
        ti(
            fg("Replies", "command_only", "bool"),
            "Only the reply command reaches the member",
        ),
        ti(fg("New tickets", "paused", "bool"), "Turn off new tickets"),
        ti(
            fg("New tickets", "pause_reason", "text"),
            "Reason shown to members",
        ),
        ti(fg("Blacklist", "blocked", "users"), "Members who cannot open tickets"),
        hp(
            ti(fg("Snippets", "snippets", "snippets"), "Saved replies"),
            "Sent with the snippet command in a ticket. Tags: {username} {userid} {usermention}",
        ),
    ];
    let appeals = vec![
        ti(fg("General", "enabled", "bool"), "Appeals System Status"),
        ti(fg("General", "channel", "channel"), "Appeals channel"),
        fg("Appeal Form", "fields", "form"),
    ];
    [
        ("logging", "Logging", log),
        ("antinuke", "Anti-nuke", nuke),
        ("automod", "Automod", auto),
        ("joingate", "Join gate", gate),
        ("joinraid", "Join raid", raid),
        (
            "lockdown",
            "Lockdown",
            vec![
                f("main_roles", "roles"),
                ti(
                    dv(fg("Lock scope", "channels", "bool"), true),
                    "Lock server channels",
                ),
                ti(
                    dv(fg("Lock scope", "roles", "bool"), false),
                    "Lock staff roles",
                ),
                ti(
                    dv(fg("Lock scope", "joins", "bool"), false),
                    "Lock server joins",
                ),
                ti(fg("Lock scope", "updates", "bool"), "Make lockdown updates"),
                ti(
                    dv(fg("Lock scope", "pause_invites", "bool"), true),
                    "Pause invites and member DMs when kicking new members",
                ),
                ti(
                    fg("Lock scope", "voice", "bool"),
                    "Lock voice too (connect, speak, soundboard, stream)",
                ),
            ],
        ),
        (
            "catch",
            "Catch channel",
            vec![
                ti(fg("General", "enabled", "bool"), "Use the catch channel"),
                hp(
                    ti(fg("General", "channel", "channel"), "Catch channel"),
                    "Anyone without a permit who writes here is punished. Hacked accounts spam every channel, so they land here first.",
                ),
                hp(
                    dv(
                        ti(
                            choice("General", "action", &["Ban", "Softban", "Kick", "Timeout", "Log"]),
                            "Action",
                        ),
                        "Ban",
                    ),
                    "Log is a test mode: it only logs. Bans delete messages for the days set under Miscellaneous.",
                ),
                ti(fg("General", "dm", "bool"), "DM the member first, with the appeal link"),
                ti(fg("General", "keep_top", "bool"), "Keep the channel at the top of the server"),
                ti(fg("Whitelist", "roles", "roles"), "Whitelisted roles"),
                ti(fg("Whitelist", "users", "users"), "Whitelisted members"),
            ],
        ),
        ("panic", "Panic mode", panic),
        ("backups", "Backups", backups),
        ("appeals", "Appeals", appeals),
        ("verification", "Verification", verification),
        ("tickets", "Tickets", tickets),
        ("whitelist", "Whitelist", whitelist),
        ("misc", "Miscellaneous", misc),
        (
            "ban_sync",
            "Ban Sync",
            vec![
                hp(
                    ti(fg("General", "enabled", "bool"), "Use Ban Sync"),
                    "Copies bans between servers you own. Only servers with the same Discord owner are in the group.",
                ),
                ti(
                    dv(fg("General", "share", "bool"), true),
                    "Send this server's bans to my other servers",
                ),
                ti(
                    dv(choice("General", "receive", &["off", "alert", "ban"]), "alert"),
                    "When a member is banned in my other servers",
                ),
                ti(
                    dv(fg("General", "sync_unbans", "bool"), true),
                    "Lift the copies when the ban is lifted",
                ),
                ti(
                    dv(fg("Automatic bans to send", "share_joinraid", "bool"), true),
                    "Join Raid bans",
                ),
                ti(
                    dv(fg("Automatic bans to send", "share_automod", "bool"), true),
                    "Auto Mod bans",
                ),
                ti(
                    fg("Automatic bans to send", "share_joingate", "bool"),
                    "Join Gate filter bans",
                ),
                ti(
                    fg("Automatic bans to send", "share_verify", "bool"),
                    "Verification timeout bans",
                ),
            ],
        ),
        (
            "raider_list",
            "Raider List",
            vec![
                ti(fg("General", "enabled", "bool"), "Use the Raider List"),
                ti(
                    dv(
                        choice("General", "action", &["alert", "quarantine", "kick", "ban"]),
                        "ban",
                    ),
                    "When a listed account joins",
                ),
                ti(
                    dv(
                        multi("General", "categories", &["nuke", "raid", "scam", "spam"]),
                        json!(["nuke", "raid", "scam", "spam"]),
                    ),
                    "Act on these categories",
                ),
                hp(
                    ti(
                        dv(fg("General", "scan_members", "bool"), true),
                        "Check members already in the server",
                    ),
                    "Only alerts in the mod log, it never acts on members who are already here.",
                ),
                ti(
                    dv(fg("General", "lift_on_removal", "bool"), true),
                    "Unban here when someone is taken off the list",
                ),
                ti(
                    fg("General", "no_collect", "bool"),
                    "Do not send what TommyBot catches here to the list",
                ),
            ],
        ),
    ]
    .into_iter()
    .map(|(n, t, mut fields)| {
        // The form keeps numbers inside the same ranges `api::validate` enforces.
        for v in &mut fields {
            let sec = v["section"].as_str().unwrap_or(n).to_string();
            if let Some((.., tip)) = TIPS.iter().find(|(ts, tp, _)| {
                *ts == sec && v["path"].as_str().is_some_and(|p| p == *tp || p.ends_with(&format!(".{tp}")))
            }) {
                v["tip"] = json!(tip);
            }
            if let Some((lo, hi)) = v["path"].as_str().and_then(|p| limits::for_path(&sec, p)) {
                v["min"] = json!(lo);
                v["max"] = json!(hi);
            }
        }
        json!({"name": n, "title": t, "fields": fields})
    })
    .collect()
}

/// Ids held in `v`: a string or number, or an array of them.
fn flat(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Number(n) => out.push(n.to_string()),
        Value::Array(a) => a.iter().for_each(|x| flat(x, out)),
        _ => {}
    }
}

/// Channel and role ids the schema's fields of `section` hold in `v`, as (channels, roles).
pub fn ids(section: &str, v: &Value) -> (Vec<String>, Vec<String>) {
    let (mut ch, mut ro) = (vec![], vec![]);
    for sec in schema() {
        for fd in sec["fields"].as_array().into_iter().flatten() {
            if fd["section"].as_str().or(sec["name"].as_str()) != Some(section) {
                continue;
            }
            let path = format!(
                "/{}",
                fd["path"].as_str().unwrap_or_default().replace('.', "/")
            );
            let at = v.pointer(&path).unwrap_or(&Value::Null);
            let items = || at.as_array().into_iter().flatten();
            match fd["kind"].as_str().unwrap_or_default() {
                "channel" | "channels" | "category" | "categories" => flat(at, &mut ch),
                "role" | "roles" => flat(at, &mut ro),
                "customs" => items().for_each(|c| flat(&c["channel"], &mut ch)),
                "route_pick" => at
                    .as_object()
                    .into_iter()
                    .flatten()
                    .for_each(|(_, c)| flat(c, &mut ch)),
                _ => {}
            }
        }
    }
    (ch, ro)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;

    #[test]
    fn limited_numbers_carry_their_range() {
        let all = schema();
        let field = |sec: &str, path: &str| {
            all.iter()
                .find(|s| s["name"] == sec)
                .and_then(|s| {
                    s["fields"]
                        .as_array()?
                        .iter()
                        .find(|f| f["path"] == path)
                        .cloned()
                })
                .unwrap()
        };
        let hour = field("antinuke", "limits.Ban.per_hour");
        assert_eq!((&hour["min"], &hour["max"]), (&json!(2), &json!(500)));
        let heat = field("antinuke", "limits.Unban.heat");
        assert_eq!((&heat["min"], &heat["max"]), (&json!(1), &json!(100)));
        let age = field("joingate", "min_age_override_ms");
        assert_eq!(
            (&age["min"], &age["max"]),
            (&json!(5_000), &json!(8_640_000_000_i64))
        );
        assert!(field("antinuke", "enabled")["min"].is_null());
    }

    #[test]
    fn gate_actions_show_defaults() {
        let gate = schema()
            .into_iter()
            .find(|s| s["name"] == "joingate")
            .unwrap();
        let default = |p: &str| {
            gate["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["path"] == p)
                .unwrap()["default"]
                .clone()
        };
        assert_eq!(default("username_filter.action"), "Ban");
        assert_eq!(default("invite_in_name.action"), "Timeout");
        assert_eq!(default("no_avatar.action"), "Kick");
    }

    #[test]
    fn ids_follow_field_kinds() {
        let v = json!({"fallback": "1", "routes": {"report": 2}, "events": {"member_kick": ""},
            "custom": [{"channel": "3", "events": []}], "ignored_channels": ["4"], "other": "9"});
        let (ch, ro) = ids("logging", &v);
        let mut ch = ch;
        ch.sort();
        ch.dedup();
        assert_eq!(ch, ["1", "2", "3", "4"]);
        assert!(ro.is_empty());
    }

    /// A valid value for a schema field, by its kind.
    fn sample(fd: &Value) -> Value {
        match fd["kind"].as_str().unwrap() {
            "bool" => json!(true),
            "int" => fd.get("min").cloned().unwrap_or(json!(5)),
            "float" | "pct" => json!(0.5),
            "text" | "color" => json!("x"),
            "list" => json!(["1"]),
            "choice" => fd["options"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o.as_str() != Some(""))
                .unwrap()
                .clone(),
            "channel" | "role" | "category" => json!("1"),
            "form" => json!([{"label": "x"}]),
            "customs" => json!([{"events": [fd["options"][0]], "channel": "1"}]),
            "route_pick" => json!({fd["options"][0]["key"].as_str().unwrap(): "1"}),
            "warn_actions" => json!([{"points": 5, "action": "timeout", "secs": 60}]),
            "snippets" => json!([{"name": "x", "content": "y"}]),
            "multi" => json!([fd["options"][0]]),
            _ => json!(["1"]),
        }
    }

    #[test]
    fn every_schema_path_deserializes() {
        let mut objs: std::collections::BTreeMap<String, Value> = Default::default();
        for sec in schema() {
            for fd in sec["fields"].as_array().unwrap() {
                let sample = sample(fd);
                let name = fd["section"].as_str().or(sec["name"].as_str()).unwrap();
                let mut cur = objs.entry(name.to_string()).or_insert_with(|| json!({}));
                let parts: Vec<_> = fd["path"].as_str().unwrap().split('.').collect();
                for p in &parts[..parts.len() - 1] {
                    cur = &mut cur[*p];
                }
                cur[parts[parts.len() - 1]] = sample;
            }
        }
        for (name, obj) in objs {
            api::validate(&name, &obj)
                .unwrap_or_else(|e| panic!("{name} schema does not deserialize: {e:?}"));
        }
    }

    /// The form saves an emptied number field as its default, or null when it has none,
    /// so a field without a default must be optional in the config, or saving fails.
    #[test]
    fn number_fields_without_default_accept_null() {
        let mut bad = vec![];
        for sec in schema() {
            for fd in sec["fields"].as_array().unwrap() {
                if !matches!(fd["kind"].as_str(), Some("int" | "float" | "pct"))
                    || !fd["default"].is_null()
                {
                    continue;
                }
                let name = fd["section"].as_str().or(sec["name"].as_str()).unwrap();
                let mut obj = json!({});
                let mut cur = &mut obj;
                let parts: Vec<_> = fd["path"].as_str().unwrap().split('.').collect();
                for p in &parts[..parts.len() - 1] {
                    cur = &mut cur[*p];
                }
                cur[parts[parts.len() - 1]] = Value::Null;
                if let Err(e) = api::validate(name, &obj) {
                    bad.push(format!("{name}.{}: {e:?}", fd["path"].as_str().unwrap()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "number fields that cannot be emptied:\n{}",
            bad.join("\n")
        );
    }

    /// A section saved with only some fields (a server that never set the rest) still loads:
    /// every config struct needs `#[serde(default)]`, like Join Raid lacked (`suspicious_only`).
    #[test]
    fn one_field_alone_deserializes() {
        let mut bad = vec![];
        for sec in schema() {
            for fd in sec["fields"].as_array().unwrap() {
                let name = fd["section"].as_str().or(sec["name"].as_str()).unwrap();
                let mut obj = json!({});
                let mut cur = &mut obj;
                let parts: Vec<_> = fd["path"].as_str().unwrap().split('.').collect();
                for p in &parts[..parts.len() - 1] {
                    cur = &mut cur[*p];
                }
                cur[parts[parts.len() - 1]] = sample(fd);
                if let Err(e) = api::validate(name, &obj) {
                    bad.push(format!("{name}.{}: {e:?}", fd["path"].as_str().unwrap()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "sections that need every field:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn every_tip_finds_its_field() {
        let all = schema();
        for (sec, path, _) in TIPS {
            let found = all
                .iter()
                .flat_map(|s| {
                    let n = s["name"].clone();
                    s["fields"].as_array().unwrap().iter().map(move |f| {
                        (
                            f["section"]
                                .as_str()
                                .map(str::to_string)
                                .or(n.as_str().map(str::to_string)),
                            f.clone(),
                        )
                    })
                })
                .any(|(s, f)| {
                    s.as_deref() == Some(*sec)
                        && !f["tip"].is_null()
                        && f["path"]
                            .as_str()
                            .is_some_and(|p| p == *path || p.ends_with(&format!(".{path}")))
                });
            assert!(found, "tip for {sec}.{path} matches no field");
        }
    }
}
