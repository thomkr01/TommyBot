//! Typed form fields per config section, served to the dashboard UI.
use serde_json::{json, Value};
use tommybot_antinuke::Action as NukeAction;
use tommybot_automod::{heat::HeatCfg, strike::StrikeCfg};
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

/// A choice field with its options; "" is the unset option.
fn choice(group: &str, path: &str, opts: &[&str]) -> Value {
    let mut v = fg(group, path, "choice");
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
    ("scam_links", "malicious", "Nitro and Steam scam links"),
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
];
const GATE_ACTIONS: &[&str] = &["Log", "Warn", "Timeout", "Kick", "Ban"];

/// Typed form fields per section; paths mirror the serde types validated in `api::validate`.
pub fn schema() -> Vec<Value> {
    let mut log = vec![
        ti(
            pg(f("fallback", "channel"), "wick_essential"),
            "TommyBot logs",
        ),
        ti(
            pg(
                fg("Channels", "routes.mod_command", "channel"),
                "wick_essential",
            ),
            "Mod logs",
        ),
        pg(
            fg("Ignored", "ignored_channels", "channels"),
            "wick_customize",
        ),
    ];
    log.extend(LOG_KINDS.iter().filter(|k| **k != "mod_command").map(|k| {
        pg(
            fg("Log types", &format!("routes.{k}"), "channel"),
            "wick_customize",
        )
    }));
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
        ti(dv(fg("Status", "enabled", "bool"), true), "Use the antinuke module"),
        ti(fg("Status", "prune", "bool"), "Detect and react to member prunes"),
        ti(fg("Status", "vanity_guard", "bool"), "Have TommyBot guard your server's vanity url"),
        ti(dv(fg("Auto quarantine", "auto_quarantine", "bool"), true), "Use the auto quarantine module"),
        ti(fg("Auto quarantine", "quarantine_role", "role"), "Quarantine role"),
        ti(fg("Auto quarantine", "strict_mode", "bool"), "Strict mode: punish unauthorized admins giving dangerous perms to any role in the server"),
        ti(fg("Auto quarantine", "strict_member_roles", "bool"), "Strict member role addition: punish unauthorized admins giving dangerous perms to other members"),
        ti(fg("Auto quarantine", "monitor_public_roles", "bool"), "Monitor public roles: protect @everyone and main roles from getting dangerous permissions"),
        ti(fg("Auto quarantine", "monitor_channel_perms", "bool"), "Monitor channel permissions: protect @everyone and main roles from getting dangerous CHANNEL permissions"),
        hp(
            ti(fg("Auto quarantine", "quarantine_whitelist", "users"), "Quarantine whitelist"),
            "Members that may change the roles of quarantined members.",
        ),
    ];
    for (a, title) in NUKE_CARDS {
        let (m, h, heat) = NukeAction::ALL
            .into_iter()
            .find(|x| format!("{x:?}") == *a)
            .and_then(NukeAction::default_limit)
            .unwrap_or_default();
        let card = *title;
        let at = |k: &str, kind: &str, title: &str| {
            ti(
                pg(fg(card, &format!("limits.{a}.{k}"), kind), "limits"),
                title,
            )
        };
        let mut first = dv(at("enabled", "bool", "Monitor this action"), true);
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
        ti(dv(fg("Auto timeouts", "strike.multiply", "bool"), true), "Multiply timeouts"),
        ti(dv(fg("Auto timeouts", "strike.multiplier_pct", "int"), 200), "Multiplier percentage"),
        ti(dv(fg("Heat settings", "heat.max", "float"), 100.0), "Max heat"),
        ti(dv(fg("Heat settings", "heat.decay_per_sec", "float"), 0.05), "Heat degradation (every second)"),
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
                    true,
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
        ti(
            dv(fg("General", "enabled", "bool"), true),
            "Use the joingate module",
        ),
        ti(
            fg("General", "dm_punished", "bool"),
            "DM members when punished",
        ),
        ti(
            fg("General", "require_all", "bool"),
            "Act only when every enabled filter matches",
        ),
    ];
    // (key, card, status label, Wick's default action; `join::gate` falls back to the same).
    for (k, card, status, action) in [
        (
            "unauthorized_bot",
            "Bot additions",
            "Target bots added by unauthorized members",
            "Kick",
        ),
        (
            "unverified_bot",
            "Unverified bot additions",
            "Target Discord unverified bots added by anyone",
            "Kick",
        ),
        (
            "no_avatar",
            "No avatar",
            "Target accounts that have no profile pictures",
            "Kick",
        ),
        (
            "suspicious",
            "Suspicious account",
            "Target accounts TommyBot deems suspicious",
            "Kick",
        ),
        (
            "invite_in_name",
            "Advertising name",
            "Target accounts with invite links in usernames",
            "Timeout",
        ),
        (
            "account_age",
            "Account age",
            "Target accounts younger than the minimum account age",
            "Kick",
        ),
        (
            "username_filter",
            "Username",
            "Target accounts whose username matches your list",
            "Ban",
        ),
    ] {
        gate.extend([
            ti(fg(card, &format!("{k}.enabled"), "bool"), status),
            hp(
                ti(
                    dv(choice(card, &format!("{k}.action"), GATE_ACTIONS), action),
                    "Trigger action",
                ),
                "Kick is Discord's native kick feature, Timeout its native timeout.",
            ),
        ]);
    }
    gate.extend([
        ti(
            dv(fg("Account age", "min_age_days", "int"), 7),
            "Minimum account age",
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
        dv(fg("General", "enabled", "bool"), true),
        f("count", "int"),
        f("span_ms", "int"),
        f("suspicious_only", "bool"),
        f("active_ms", "int"),
        f("string_patterns", "list"),
        f("flag_younger_than_ms", "int"),
        f("flag_no_avatar", "bool"),
        f("id_flag_min", "int"),
        dv(
            choice(
                "Response",
                "action",
                &["Log", "Warn", "Timeout", "Kick", "Ban"],
            ),
            "Kick",
        ),
        choice("Response", "lock", &["", "Server", "Blind", "Role", "All"]),
        fg("Response", "warned_roles", "roles"),
    ];
    // "heat" is the Auto Mod page, "nuke" the Anti Nuke page; the fields without a page are shared.
    let panic = vec![
        ti(
            pg(fg("Trigger", "executors", "int"), "nuke"),
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
        dv(fg("Response", "duration_secs", "int"), 600),
        fg("Response", "lock_mod_commands", "bool"),
        fg("Response", "unlock_when_ending", "bool"),
        fg("Response", "warned_roles", "roles"),
        fg("Response", "whitelisted_categories", "categories"),
    ];
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
    let misc = vec![
        ti(dv(fg("General", "prefix", "text"), "w!"), "Prefix"),
        ti(dv(fg("General", "use_slash", "bool"), true), "Use slash commands"),
        ti(fg("General", "compact_replies", "bool"), "Minify TommyBot's message replies to compact embeds"),
        ti(fg("General", "delete_replies_secs", "int"), "Delete TommyBot's auto messages after few seconds"),
        ti(fg("General", "enforce_quarantine_perms", "bool"), "Always enforce the proper permissions of Quarantine role in channels"),
        ti(fg("General", "ignore_no_permit", "bool"), "Always ignore commands issued by members without any permits"),
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
        ti(dv(fg("Moderation commands", "confirm", "bool"), true), "Display a confirmation message before executing the moderation action a moderator wants"),
        ti(fg("Moderation commands", "always_dm", "bool"), "Always DM the members punished regardless of the presence of a ?dm parameter"),
        ti(fg("Moderation commands", "delete_invocation", "bool"), "Always delete the moderation execution messages for regular prefix commands"),
        ti(fg("Moderation commands", "hide_staff_in_dms", "bool"), "Hide the staff member responsible when DMing the punished member"),
        hp(
            ti(
                dv(fg("Moderation commands", "default_timeout_secs", "int"), 600),
                "Default timeout duration",
            ),
            "0 means 28 days",
        ),
        ti(dv(fg("Moderation commands", "ban_purge_days", "int"), 7), "Days purged on bans"),
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
                dv(fg("General", "replace_roles", "bool"), true),
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
        dv(
            ti(
                choice(
                    "General",
                    "mode",
                    &[
                        "Button",
                        "Captcha",
                        "SimpleCaptcha",
                        "Dm",
                        "Web",
                        "ThirdParty",
                    ],
                ),
                "Verification mode",
            ),
            "Captcha",
        ),
        ti(
            dv(fg("General", "show_notes", "bool"), true),
            "Display helpful notes alongside the captcha",
        ),
        dv(
            ti(
                fg("General", "time_limit_ms", "int"),
                "Verification duration",
            ),
            120_000,
        ),
        ti(fg("General", "channel", "channel"), "Verification channel"),
        ti(fg("General", "roles", "roles"), "Verification roles"),
        ti(
            dv(fg("Captcha", "captcha.height", "int"), 100),
            "Captcha height",
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
            dv(fg("Captcha", "captcha.font_color", "color"), "#000000"),
            "Font color",
        ),
        ti(
            dv(fg("Captcha", "captcha.length", "int"), 6),
            "Captcha answer length",
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
            dv(fg("Captcha", "captcha.decoy_color", "color"), "#000000"),
            "Decoy color",
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
            ],
        ),
        ("panic", "Panic mode", panic),
        ("backups", "Backups", vec![dv(f("auto", "bool"), true)]),
        ("appeals", "Appeals", appeals),
        ("verification", "Verification", verification),
        ("whitelist", "Whitelist", whitelist),
        ("misc", "Miscellaneous", misc),
    ]
    .into_iter()
    .map(|(n, t, fields)| json!({"name": n, "title": t, "fields": fields}))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;

    #[test]
    fn every_schema_path_deserializes() {
        let mut objs: std::collections::BTreeMap<String, Value> = Default::default();
        for sec in schema() {
            for fd in sec["fields"].as_array().unwrap() {
                let sample = match fd["kind"].as_str().unwrap() {
                    "bool" => json!(true),
                    "int" => json!(5),
                    "float" => json!(1.5),
                    "text" | "color" => json!("x"),
                    "list" => json!(["1"]),
                    "choice" => fd["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|o| o.as_str() != Some(""))
                        .unwrap()
                        .clone(),
                    "channel" | "role" => json!("1"),
                    "form" => json!([{"label": "x"}]),
                    "customs" => json!([{"events": [fd["options"][0]], "channel": "1"}]),
                    "warn_actions" => json!([{"points": 5, "action": "timeout", "secs": 60}]),
                    _ => json!(["1"]),
                };
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
}
