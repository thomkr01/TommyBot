//! Security score: risky Discord settings a raider or nuker would use. Shared by `/security` and
//! the dashboard Overview, which runs the `fix` of a row with one click.
use crate::{ChannelImg, OverwriteImg, RoleImg, CATEGORY};
use serde::Serialize;

/// The dangerous list: Administrator, Manage Server/Roles/Channels/Webhooks/Messages, Ban, Kick, Timeout.
const DANGEROUS: u64 = crate::DANGEROUS_PERMS;
const ADMINISTRATOR: u64 = 0x8;
/// What @everyone and main roles must not have: [`DANGEROUS`] plus Mention @everyone.
const EVERYONE_DANGEROUS: u64 = DANGEROUS | (1 << 17);
const VIEW: u64 = 1 << 10;
/// View Channel, Send Messages and Embed Links: what the bot needs in a log channel.
const BOT_LOG: u64 = VIEW | (1 << 11) | (1 << 14);

/// Discord's numbers from the guild object.
pub struct GuildSafety {
    pub id: u64,
    pub mfa: u8,
    pub verification: u8,
    pub content_filter: u8,
    pub notifications: u8,
}

/// What the check needs from TommyBot's own config.
pub struct Setup {
    pub quarantine_role: Option<u64>,
    pub log_fallback: bool,
    /// Staff permits (Admins, Moderators) that no role holds.
    pub roleless_staff: Vec<String>,
    /// Anything is on an automod whitelist (spam, invites, mentions, @everyone, partner channels).
    pub whitelisted: bool,
    /// The bot's own roles.
    pub bot_roles: Vec<u64>,
    pub mod_log: bool,
    /// (module name, switched on) for the protection modules.
    pub modules: Vec<(&'static str, bool)>,
    /// Lockdown main roles; their channel overwrites are held to the @everyone rules.
    pub main_roles: Vec<u64>,
    /// Members whose own channel overwrites may grant anything: owner, extra owners, the bot.
    pub trusted: Vec<u64>,
    /// Every channel a log goes to.
    pub log_channels: Vec<u64>,
}

/// Ordered from fine to worst.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Warn,
    Bad,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Fix {
    pub label: &'static str,
    /// A role id, or `*` for every role [`strip_targets`] picks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// With `role_id`: the overwrite on this channel (and its synced children) whose target is
    /// `role_id`, a role or member id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// A guild setting [`setting_fix`] knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting: Option<&'static str>,
    /// With `role_id`: move that role right below the bot's top role.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub move_under_bot: bool,
}

#[derive(Serialize, Debug)]
pub struct Check {
    pub id: String,
    pub severity: Severity,
    pub title: &'static str,
    pub detail: String,
    pub fix: Option<Fix>,
}

fn strip(role: impl ToString) -> Option<Fix> {
    Some(Fix {
        label: "Strip permissions",
        role_id: Some(role.to_string()),
        channel_id: None,
        setting: None,
        move_under_bot: false,
    })
}

fn set(setting: &'static str) -> Option<Fix> {
    Some(Fix {
        label: "Fix",
        role_id: None,
        channel_id: None,
        setting: Some(setting),
        move_under_bot: false,
    })
}

/// The [`Fix::setting`] that hides a log channel with [`private_overwrites`].
pub const PRIVATE: &str = "private_channel";

/// The safer value a setting fix writes: (Discord field, value).
pub fn setting_fix(setting: &str) -> Option<(&'static str, u8)> {
    match setting {
        "verification_level" => Some(("verification_level", 2)),
        "content_filter" => Some(("explicit_content_filter", 2)),
        "notifications" => Some(("default_message_notifications", 1)),
        _ => None,
    }
}

/// The permissions a role keeps after a strip fix.
pub fn stripped(role_id: u64, guild: u64, perms: u64) -> u64 {
    perms
        & !if role_id == guild {
            EVERYONE_DANGEROUS
        } else {
            DANGEROUS
        }
}

/// Names of the [`EVERYONE_DANGEROUS`] bits a channel overwrite can hold.
fn names(bits: u64) -> String {
    const NAMES: [(u64, &str); 5] = [
        (0x10, "Manage Channels"),
        (0x1000_0000, "Manage Permissions"),
        (0x2000_0000, "Manage Webhooks"),
        (1 << 13, "Manage Messages"),
        (1 << 17, "Mention @everyone"),
    ];
    NAMES
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

fn overwrite_mask(o: &OverwriteImg, guild: u64, setup: &Setup) -> u64 {
    match (
        o.is_role,
        o.target == guild || setup.main_roles.contains(&o.target),
    ) {
        (false, _) if setup.trusted.contains(&o.target) => 0,
        (true, true) => EVERYONE_DANGEROUS,
        _ => DANGEROUS,
    }
}

/// A channel is synced when it has its category's overwrites, in any order.
pub fn synced(c: &ChannelImg, cat: &ChannelImg) -> bool {
    c.overwrites.len() == cat.overwrites.len()
        && c.overwrites.iter().all(|o| cat.overwrites.contains(o))
}

/// Children of `cat` that are synced with it, so a category fix keeps them synced.
pub fn synced_children<'a>(channels: &'a [ChannelImg], cat: &ChannelImg) -> Vec<&'a ChannelImg> {
    channels
        .iter()
        .filter(|c| c.parent == Some(cat.id) && synced(c, cat))
        .collect()
}

/// Channel and category overwrites that grant dangerous permissions to anyone but managed
/// (bot) roles and trusted members. Synced children are left out: their category's row covers
/// them. Private channels count like any other.
pub fn overwrite_targets<'a>(
    channels: &'a [ChannelImg],
    roles: &[RoleImg],
    guild: u64,
    setup: &Setup,
) -> Vec<(&'a ChannelImg, &'a OverwriteImg)> {
    let managed = |id| roles.iter().any(|r| r.id == id && r.managed);
    channels
        .iter()
        .filter(|c| {
            c.parent
                .and_then(|p| channels.iter().find(|x| x.id == p))
                .is_none_or(|cat| !synced(c, cat))
        })
        .flat_map(|c| c.overwrites.iter().map(move |o| (c, o)))
        .filter(|(_, o)| {
            !(o.is_role && managed(o.target)) && o.allow & overwrite_mask(o, guild, setup) != 0
        })
        .collect()
}

/// The overwrite after an overwrite fix: dangerous allows dropped, deny kept.
pub fn stripped_overwrite(o: &OverwriteImg, guild: u64, setup: &Setup) -> OverwriteImg {
    OverwriteImg {
        allow: o.allow & !overwrite_mask(o, guild, setup),
        ..o.clone()
    }
}

/// Discord's role order: higher position first, the lower id first on a tie.
fn rank(r: &RoleImg) -> (i64, std::cmp::Reverse<u64>) {
    (r.position, std::cmp::Reverse(r.id))
}

/// Roles ranked between `q` and the bot's top role, highest first (by Discord's order, as
/// positions can tie).
pub fn between_bot_and<'a>(
    roles: &'a [RoleImg],
    q: &RoleImg,
    bot_top: i64,
    bot_roles: &[u64],
) -> Vec<&'a RoleImg> {
    let bot = roles
        .iter()
        .filter(|r| bot_roles.contains(&r.id))
        .map(rank)
        .max()
        // Without the bot's role list, a role tied with the bot counts as below it.
        .unwrap_or((bot_top, std::cmp::Reverse(0)));
    let mut v: Vec<&RoleImg> = roles
        .iter()
        .filter(|r| rank(r) > rank(q) && rank(r) < bot)
        .collect();
    v.sort_by_key(|r| std::cmp::Reverse(rank(r)));
    v
}

/// New `(role, position)` pairs that put the quarantine role right below the bot: the roles in
/// between and it trade positions, so no role outside that block moves. Empty for any other role,
/// so the fix cannot lift a staff role up the list.
pub fn move_under_bot(
    roles: &[RoleImg],
    target: u64,
    bot_top: i64,
    setup: &Setup,
) -> Vec<(u64, i64)> {
    let Some(q) = roles
        .iter()
        .find(|r| r.id == target && setup.quarantine_role == Some(target))
    else {
        return vec![];
    };
    let mid = between_bot_and(roles, q, bot_top, &setup.bot_roles);
    let Some(top) = mid.first() else {
        return vec![];
    };
    // A role tied with the bot's position may not take that position itself.
    let below = mid.iter().skip(1).map(|r| r.position).chain([q.position]);
    std::iter::once((q.id, top.position.min(bot_top - 1)))
        .chain(mid.iter().map(|r| r.id).zip(below))
        .collect()
}

/// Whether @everyone, or a main role every member gets, can view `c` (Administrator sees all).
pub fn is_public(c: &ChannelImg, roles: &[RoleImg], guild: u64, main_roles: &[u64]) -> bool {
    let perms = |id| {
        roles
            .iter()
            .find(|r| r.id == id)
            .map_or(0, |r| r.permissions)
    };
    let ow = |id| {
        c.overwrites
            .iter()
            .find(|o| o.is_role && o.target == id)
            .map_or((0, 0), |o| (o.allow, o.deny))
    };
    let sees = |base: u64, ows: &[(u64, u64)]| {
        base & ADMINISTRATOR != 0 || ows.iter().fold(base, |p, (a, d)| (p & !d) | a) & VIEW != 0
    };
    sees(perms(guild), &[ow(guild)])
        || main_roles
            .iter()
            .any(|&r| sees(perms(guild) | perms(r), &[ow(guild), ow(r)]))
}

/// The overwrites that make `c` private: @everyone and the main roles are denied View Channel
/// (their other bits kept), the bot member gets View, Send and Embed Links. Empty when @everyone
/// is already denied and no main role is let back in.
pub fn private_overwrites(
    c: &ChannelImg,
    guild: u64,
    bot: u64,
    main_roles: &[u64],
) -> Vec<OverwriteImg> {
    let find = |id, is_role| {
        c.overwrites
            .iter()
            .find(|o| o.target == id && o.is_role == is_role)
            .cloned()
            .unwrap_or(OverwriteImg {
                target: id,
                is_role,
                allow: 0,
                deny: 0,
            })
    };
    let everyone = find(guild, true);
    let mains: Vec<_> = main_roles.iter().map(|&r| find(r, true)).collect();
    if everyone.deny & VIEW != 0 && mains.iter().all(|o| o.allow & VIEW == 0) {
        return vec![];
    }
    let hide = |o: OverwriteImg| OverwriteImg {
        allow: o.allow & !VIEW,
        deny: o.deny | VIEW,
        ..o
    };
    let me = find(bot, false);
    std::iter::once(hide(everyone))
        .chain(mains.into_iter().map(hide))
        .chain([OverwriteImg {
            allow: me.allow | BOT_LOG,
            deny: me.deny & !BOT_LOG,
            ..me
        }])
        .collect()
}

/// Roles the bot can safely strip in one go: not managed, not @everyone, below the bot.
pub fn strip_targets(roles: &[RoleImg], guild: u64, bot_top: i64) -> Vec<&RoleImg> {
    roles
        .iter()
        .filter(|r| {
            !r.managed && r.id != guild && r.position < bot_top && r.permissions & DANGEROUS != 0
        })
        .collect()
}

pub fn checks(
    g: &GuildSafety,
    roles: &[RoleImg],
    channels: &[ChannelImg],
    bot_top: i64,
    setup: &Setup,
) -> Vec<Check> {
    let mut out = vec![];
    let mut row = |id: &str, severity, title, detail: String, fix| {
        out.push(Check {
            id: id.into(),
            severity,
            title,
            detail,
            fix,
        })
    };
    let quarantine = setup
        .quarantine_role
        .and_then(|q| roles.iter().find(|r| r.id == q));
    match (setup.quarantine_role, quarantine) {
        (None, _) => row(
            "quarantine_unset",
            Severity::Warn,
            "No quarantine role",
            "Anti-nuke has no role to quarantine offenders into.".into(),
            None,
        ),
        (Some(_), None) => row(
            "quarantine_missing",
            Severity::Bad,
            "Quarantine role missing",
            "The configured quarantine role no longer exists.".into(),
            None,
        ),
        (Some(_), Some(q)) => {
            if q.position >= bot_top {
                row(
                    "quarantine_above_bot",
                    Severity::Bad,
                    "Quarantine role above the bot",
                    format!(
                        "{} is not below the bot's top role, so the bot cannot assign it.",
                        q.name
                    ),
                    None,
                );
            } else if !between_bot_and(roles, q, bot_top, &setup.bot_roles).is_empty() {
                row(
                    "quarantine_not_right_under",
                    Severity::Warn,
                    "Quarantine role not right below the bot",
                    format!(
                        "{} is not right below the bot's top role. A role in between could give quarantined members permissions.",
                        q.name
                    ),
                    Some(Fix {
                        label: "Move role",
                        role_id: Some(q.id.to_string()),
                        channel_id: None,
                        setting: None,
                        move_under_bot: true,
                    }),
                );
            }
            if q.permissions & DANGEROUS != 0 {
                row(
                    "quarantine_perms",
                    Severity::Bad,
                    "Quarantine role is too powerful",
                    format!("{} grants dangerous permissions.", q.name),
                    strip(q.id),
                );
            }
        }
    }
    let everyone = roles
        .iter()
        .find(|r| r.id == g.id)
        .map_or(0, |r| r.permissions);
    if everyone & EVERYONE_DANGEROUS != 0 {
        row(
            "everyone_perms",
            Severity::Bad,
            "@everyone holds dangerous permissions",
            "Every member, raid accounts too, can ban, manage, mention @everyone or delete messages.".into(),
            strip(g.id),
        );
    } else if everyone != 0 {
        row(
            "everyone_perms",
            Severity::Warn,
            "@everyone holds permissions",
            "Move them to your main member role, so new and unverified accounts start with nothing.".into(),
            None,
        );
    } else {
        row(
            "everyone_perms",
            Severity::Ok,
            "@everyone is safe",
            "New accounts get no permissions until a role gives them some.".into(),
            None,
        );
    }
    // (good, text when good, text when not) for the setup rows below.
    let pick = |good: bool, yes: &str, no: String| {
        (
            if good { Severity::Ok } else { Severity::Warn },
            if good { yes.into() } else { no },
        )
    };
    let bot = roles
        .iter()
        .filter(|r| setup.bot_roles.contains(&r.id))
        .fold(0, |p, r| p | r.permissions);
    let (sev, text) = pick(
        bot == ADMINISTRATOR,
        "The bot's roles grant Administrator and nothing else.",
        "Give the bot's role Administrator and nothing else, so its access is one clear switch."
            .into(),
    );
    row("bot_admin_only", sev, "Bot role permissions", text, None);
    let staff = &setup.roleless_staff;
    let (sev, text) = pick(
        staff.is_empty(),
        "Every staff permit is held by a role.",
        format!(
            "No role holds {}, so nobody gets that permit.",
            staff.join(" or ")
        ),
    );
    row("staff_permits", sev, "Staff permit roles", text, None);
    let (sev, text) = pick(
        setup.whitelisted,
        "Trusted roles, members or channels skip the automod.",
        "Add trusted roles, members or channels to an automod whitelist so staff are not punished by mistake.".into(),
    );
    row("whitelist", sev, "Automod whitelist", text, None);
    let (sev, text) = pick(
        setup.mod_log,
        "Staff actions are recorded as cases in the mod log channel.",
        "Pick a mod log channel so staff actions are recorded.".into(),
    );
    row("mod_log", sev, "Mod log channel", text, None);
    let (sev, text) = pick(
        !setup.main_roles.is_empty(),
        "Lockdown and verification know your member roles.",
        "Set your main member roles so lockdowns and verification work.".into(),
    );
    row("main_roles", sev, "Main roles", text, None);
    for (name, on) in &setup.modules {
        let (sev, text) = pick(
            *on,
            "Switched on.",
            format!("{name} is off, so it does not protect the server."),
        );
        row(
            &format!("module_{}", name.to_lowercase().replace(' ', "_")),
            sev,
            *name,
            text,
            None,
        );
    }
    let above: Vec<&str> = roles
        .iter()
        .filter(|r| r.position > bot_top && !r.managed && r.id != g.id)
        .map(|r| r.name.as_str())
        .collect();
    if above.is_empty() {
        row(
            "bot_position",
            Severity::Ok,
            "Bot role is near the top",
            "No manageable roles sit above the bot.".into(),
            None,
        );
    } else {
        row(
            "bot_position",
            Severity::Warn,
            "Roles above the bot",
            format!("The bot cannot manage: {}.", above.join(", ")),
            None,
        );
    }
    // Any role, bot roles included, counts: a raider's bot sitting above ours wins the race.
    let higher = roles
        .iter()
        .filter(|r| r.position > bot_top && r.id != g.id)
        .count();
    row(
        "bot_top_eight",
        if higher < 8 {
            Severity::Ok
        } else {
            Severity::Warn
        },
        "Bot role among the top 8",
        format!("{higher} roles sit above the bot's highest role."),
        None,
    );
    for r in roles.iter().filter(|r| {
        !r.managed
            && r.id != g.id
            && r.permissions & DANGEROUS != 0
            && quarantine.is_none_or(|q| q.id != r.id)
    }) {
        row(
            &format!("role_{}", r.id),
            Severity::Bad,
            "Role holds dangerous permissions",
            format!(
                "{} can administer, ban, kick, time out or manage the server.",
                r.name
            ),
            strip(r.id),
        );
    }
    let overwrites = overwrite_targets(channels, roles, g.id, setup);
    for (c, o) in &overwrites {
        let who = if o.target == g.id {
            "@everyone".into()
        } else if o.is_role {
            roles
                .iter()
                .find(|r| r.id == o.target)
                .map_or(format!("role {}", o.target), |r| r.name.clone())
        } else {
            format!("member {}", o.target)
        };
        let place = match synced_children(channels, c).len() {
            0 if c.kind == CATEGORY => format!("Category {}", c.name),
            0 => format!("#{}", c.name),
            n => format!("Category {} and its {n} synced channel(s)", c.name),
        };
        row(
            &format!("overwrite_{}_{}", c.id, o.target),
            Severity::Bad,
            "Channel grants dangerous permissions",
            format!(
                "{place} gives {who}: {}.",
                names(o.allow & overwrite_mask(o, g.id, setup))
            ),
            Some(Fix {
                label: "Strip permissions",
                role_id: Some(o.target.to_string()),
                channel_id: Some(c.id.to_string()),
                setting: None,
                move_under_bot: false,
            }),
        );
    }
    let strippable = strip_targets(roles, g.id, bot_top).len() + overwrites.len();
    if strippable > 1 {
        row(
            "roles_bulk",
            Severity::Warn,
            "Remove dangerous permissions everywhere",
            format!(
                "{strippable} roles or channel overwrites can administer, ban, kick, time out or manage the server."
            ),
            Some(Fix {
                label: "Strip all",
                role_id: Some("*".into()),
                channel_id: None,
                setting: None,
                move_under_bot: false,
            }),
        );
    }
    if !setup.log_fallback {
        row(
            "log_fallback",
            Severity::Warn,
            "No fallback log channel",
            "Events without a dedicated channel are dropped.".into(),
            None,
        );
    }
    for c in channels
        .iter()
        .filter(|c| setup.log_channels.contains(&c.id))
        .filter(|c| is_public(c, roles, g.id, &setup.main_roles))
    {
        // Overwrites cannot hide a channel from Administrator; that needs the role changed.
        let fixable = !private_overwrites(c, g.id, 0, &setup.main_roles).is_empty();
        row(
            &format!("log_channel_public_{}", c.id),
            Severity::Bad,
            "Log channel is public",
            format!(
                "Every member can read #{}, and logs show deleted messages and staff actions. TommyBot does not post logs there until it is private.{}",
                c.name,
                if fixable { "" } else { " @everyone or a main role has Administrator, so take that away first." }
            ),
            fixable.then(|| Fix {
                label: "Make private",
                role_id: None,
                channel_id: Some(c.id.to_string()),
                setting: Some(PRIVATE),
                move_under_bot: false,
            }),
        );
    }
    let ok = |good: bool| if good { Severity::Ok } else { Severity::Warn };
    let fix = |good: bool, s| if good { None } else { set(s) };
    row(
        "verification_level",
        ok(g.verification >= 2),
        "Server verification level",
        "Set it to at least Medium so brand new accounts cannot post at once.".into(),
        fix(g.verification >= 2, "verification_level"),
    );
    row(
        "content_filter",
        ok(g.content_filter == 2),
        "Explicit media content filter",
        "Set it to scan media from all members.".into(),
        fix(g.content_filter == 2, "content_filter"),
    );
    row(
        "notifications",
        ok(g.notifications != 0),
        "Default notifications",
        "Set them to mentions only, so a spam raid does not notify every member for every message."
            .into(),
        fix(g.notifications != 0, "notifications"),
    );
    row(
        "mfa",
        ok(g.mfa != 0),
        "2FA for moderators",
        "The owner can turn on \"Require 2FA for moderator actions\" under Server Settings > Safety Setup, so a stolen password alone cannot ban or delete.".into(),
        None,
    );
    out
}

/// Share of checks that pass, 0-100.
pub fn score(checks: &[Check]) -> usize {
    let ok = checks.iter().filter(|c| c.severity == Severity::Ok).count();
    (ok * 100).checked_div(checks.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: u64, position: i64, managed: bool, permissions: u64) -> RoleImg {
        RoleImg {
            id,
            name: format!("r{id}"),
            color: 0,
            permissions,
            position,
            hoist: false,
            mentionable: false,
            managed,
        }
    }

    const SAFE: GuildSafety = GuildSafety {
        id: 1,
        mfa: 1,
        verification: 2,
        content_filter: 2,
        notifications: 1,
    };

    fn setup(quarantine_role: Option<u64>, log_fallback: bool) -> Setup {
        Setup {
            quarantine_role,
            log_fallback,
            roleless_staff: vec![],
            whitelisted: true,
            bot_roles: vec![],
            mod_log: true,
            main_roles: vec![99],
            modules: vec![],
            trusted: vec![],
            log_channels: vec![],
        }
    }

    fn sev<'a>(rows: &'a [Check], id: &str) -> Option<&'a Check> {
        rows.iter().find(|r| r.id == id)
    }

    #[test]
    fn server_settings_checks() {
        let setup = setup(None, true);
        let rows = checks(&SAFE, &[], &[], 0, &setup);
        for id in [
            "verification_level",
            "content_filter",
            "notifications",
            "mfa",
        ] {
            assert_eq!(sev(&rows, id).unwrap().severity, Severity::Ok, "{id}");
        }
        let risky = GuildSafety {
            id: 1,
            mfa: 0,
            verification: 1,
            content_filter: 0,
            notifications: 0,
        };
        let rows = checks(&risky, &[], &[], 0, &setup);
        for id in [
            "verification_level",
            "content_filter",
            "notifications",
            "mfa",
        ] {
            assert_eq!(sev(&rows, id).unwrap().severity, Severity::Warn, "{id}");
        }
        assert_eq!(
            sev(&rows, "verification_level").unwrap().fix,
            set("verification_level")
        );
        assert_eq!(sev(&rows, "mfa").unwrap().fix, None);
        for s in ["verification_level", "content_filter", "notifications"] {
            assert!(setting_fix(s).is_some());
        }
    }

    #[test]
    fn score_flags_dangerous_roles_and_missing_fallback() {
        let roles = [
            role(1, 0, false, 0),
            role(2, 5, false, 0x8),
            role(3, 6, true, 0x8),
            role(4, 1, false, 0x400),
            role(9, 3, false, 0x1),
            role(5, 2, false, 1 << 13),
            role(6, 2, false, 1 << 40),
        ];
        let rows = checks(&SAFE, &roles, &[], 4, &setup(Some(9), false));
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"role_2") && !ids.contains(&"role_3") && !ids.contains(&"role_4"));
        // Manage Messages and Timeout count too.
        assert!(ids.contains(&"role_5") && ids.contains(&"role_6"));
        assert!(ids.contains(&"log_fallback") && ids.contains(&"bot_position"));
        assert_eq!(sev(&rows, "role_2").unwrap().fix, strip(2));
        let rows = checks(&SAFE, &roles, &[], 2, &setup(Some(9), true));
        assert!(sev(&rows, "quarantine_above_bot").is_some());
    }

    #[test]
    fn everyone_with_mass_mention_is_flagged_and_strippable() {
        let roles = [role(1, 0, false, (1 << 17) | 0x400)];
        let rows = checks(&SAFE, &roles, &[], 4, &setup(None, true));
        let row = sev(&rows, "everyone_perms").unwrap();
        assert_eq!(row.severity, Severity::Bad);
        assert_eq!(row.fix, strip(1));
        assert_eq!(stripped(1, 1, (1 << 17) | 0x400), 0x400);
        assert_eq!(stripped(2, 1, (1 << 17) | 0x8), 1 << 17);
    }

    #[test]
    fn bulk_strip_skips_managed_everyone_and_roles_above_bot() {
        let roles = [
            role(1, 0, false, 0x8),
            role(2, 2, false, 0x8),
            role(3, 3, true, 0x8),
            role(4, 4, false, 0x8),
            role(5, 1, false, 0x400),
        ];
        let ids: Vec<u64> = strip_targets(&roles, 1, 4).iter().map(|r| r.id).collect();
        assert_eq!(ids, [2]);
    }

    #[test]
    fn score_is_the_share_of_passing_checks() {
        let setup = setup(None, true);
        let rows = checks(&SAFE, &[], &[], 0, &setup);
        // Only the missing quarantine role and the bot role (none here) fail.
        assert_eq!(rows.len(), 13);
        assert_eq!(score(&rows), 11 * 100 / 13);
        assert_eq!(score(&[]), 0);
    }

    #[test]
    fn quarantine_must_sit_right_below_the_bot() {
        let row = |roles: &[RoleImg]| {
            let mut s = setup(Some(9), true);
            s.bot_roles = vec![50];
            let rows = checks(&SAFE, roles, &[], 5, &s);
            (
                sev(&rows, "quarantine_not_right_under").map(|c| (c.severity, c.fix.is_some())),
                sev(&rows, "quarantine_above_bot").is_some(),
            )
        };
        let bot = role(50, 5, true, 0x8);
        // Right below: no row.
        assert_eq!(
            row(&[bot.clone(), role(9, 4, false, 0), role(2, 3, false, 0)]),
            (None, false)
        );
        // One role in between: a warning with a move fix, not the "above" row.
        assert_eq!(
            row(&[bot.clone(), role(2, 4, false, 0), role(9, 3, false, 0)]),
            (Some((Severity::Warn, true)), false)
        );
        // Tied positions follow Discord's order: the lower id ranks higher.
        assert_eq!(
            row(&[bot.clone(), role(9, 4, false, 0), role(20, 4, false, 0)]),
            (None, false)
        );
        assert_eq!(
            row(&[bot.clone(), role(9, 4, false, 0), role(3, 4, false, 0)]),
            (Some((Severity::Warn, true)), false)
        );
        let roles = [
            bot.clone(),
            role(2, 4, false, 0),
            role(3, 3, false, 0),
            role(9, 2, false, 0),
            role(4, 1, false, 0),
        ];
        let mut s = setup(Some(9), true);
        s.bot_roles = vec![50];
        assert_eq!(
            move_under_bot(&roles, 9, 5, &s),
            vec![(9, 4), (2, 3), (3, 2)]
        );
        // Only the quarantine role can be moved, and only when it is not already in place.
        assert!(move_under_bot(&roles, 3, 5, &s).is_empty());
        s.quarantine_role = Some(2);
        assert!(move_under_bot(&roles, 2, 5, &s).is_empty());
        // A role tied with the bot: the quarantine role goes just below the bot's position.
        let tied = [bot.clone(), role(60, 5, false, 0), role(9, 2, false, 0)];
        s.quarantine_role = Some(9);
        assert_eq!(move_under_bot(&tied, 9, 5, &s), vec![(9, 4), (60, 2)]);
        // Above the bot: only the "above" row.
        assert_eq!(row(&[bot, role(9, 6, false, 0)]), (None, true));
        let f = Fix {
            label: "Move role",
            role_id: Some("9".into()),
            channel_id: None,
            setting: None,
            move_under_bot: true,
        };
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            serde_json::json!({"label": "Move role", "role_id": "9", "move_under_bot": true})
        );
    }

    #[test]
    fn bot_role_must_be_among_the_top_eight() {
        let at = |above: u64| {
            let roles: Vec<RoleImg> = (0..above)
                .map(|i| role(10 + i, 2 + i as i64, true, 0))
                .collect();
            sev(
                &checks(&SAFE, &roles, &[], 1, &setup(None, true)),
                "bot_top_eight",
            )
            .unwrap()
            .severity
        };
        assert_eq!(at(7), Severity::Ok);
        assert_eq!(at(8), Severity::Warn);
    }

    #[test]
    fn setup_rows_follow_permits_whitelist_and_bot_role() {
        let roles = [role(1, 0, false, 0), role(7, 5, true, ADMINISTRATOR)];
        let good = Setup {
            bot_roles: vec![7],
            ..setup(None, true)
        };
        let rows = checks(&SAFE, &roles, &[], 5, &good);
        for id in [
            "bot_admin_only",
            "staff_permits",
            "whitelist",
            "everyone_perms",
        ] {
            assert_eq!(sev(&rows, id).unwrap().severity, Severity::Ok, "{id}");
        }
        let roles = [
            role(1, 0, false, 0x400),
            role(7, 5, true, ADMINISTRATOR | 0x400),
        ];
        let bad = Setup {
            roleless_staff: vec!["Admins".into(), "Moderators".into()],
            whitelisted: false,
            mod_log: false,
            main_roles: vec![],
            modules: vec![("Join gate", false)],
            ..good
        };
        let rows = checks(&SAFE, &roles, &[], 5, &bad);
        for id in [
            "bot_admin_only",
            "staff_permits",
            "whitelist",
            "everyone_perms",
            "mod_log",
            "main_roles",
            "module_join_gate",
        ] {
            assert_eq!(sev(&rows, id).unwrap().severity, Severity::Warn, "{id}");
        }
        assert!(sev(&rows, "staff_permits")
            .unwrap()
            .detail
            .contains("Admins or Moderators"));
        assert_eq!(sev(&rows, "everyone_perms").unwrap().fix, None);
    }

    #[test]
    fn rows_serialize_for_the_dashboard() {
        let setup = setup(None, true);
        let v =
            serde_json::to_value(checks(&SAFE, &[role(1, 0, false, 0x8)], &[], 0, &setup)).unwrap();
        let everyone = v
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "everyone_perms")
            .unwrap();
        assert_eq!(everyone["severity"], "bad");
        assert_eq!(
            everyone["fix"],
            serde_json::json!({"label": "Strip permissions", "role_id": "1"})
        );
    }

    fn chan(id: u64, kind: u8, parent: Option<u64>, overwrites: Vec<OverwriteImg>) -> ChannelImg {
        ChannelImg {
            id,
            name: format!("c{id}"),
            kind,
            parent,
            position: 0,
            topic: None,
            nsfw: false,
            overwrites,
        }
    }

    fn ow(target: u64, is_role: bool, allow: u64) -> OverwriteImg {
        OverwriteImg {
            target,
            is_role,
            allow,
            deny: 0x800,
        }
    }

    #[test]
    fn overwrites_on_private_categories_and_channels_are_flagged() {
        let roles = [
            role(1, 0, false, 0),
            role(2, 1, false, 0),
            role(3, 2, true, 0),
        ];
        // Category 10 opens to role 2 with Manage Channels; 11 is synced, 12 has its own overwrites.
        let private = vec![ow(1, true, 0), ow(2, true, 0x400 | 0x10)];
        let channels = [
            chan(10, CATEGORY, None, private.clone()),
            chan(11, 0, Some(10), private.clone()),
            chan(
                12,
                0,
                Some(10),
                vec![ow(1, true, 1 << 17), ow(7, false, 0x2000_0000)],
            ),
            chan(
                13,
                0,
                None,
                vec![
                    ow(3, true, 0x10),
                    ow(8, false, 0x10),
                    ow(2, true, 0x400 | (1 << 17)),
                ],
            ),
        ];
        let setup = Setup {
            main_roles: vec![],
            trusted: vec![8],
            ..setup(None, true)
        };
        let hits: Vec<(u64, u64)> = overwrite_targets(&channels, &roles, 1, &setup)
            .iter()
            .map(|(c, o)| (c.id, o.target))
            .collect();
        // Synced 11, the managed role 3, trusted member 8 and plain mass mention for role 2 are not hits.
        assert_eq!(hits, [(10, 2), (12, 1), (12, 7)]);
        assert_eq!(synced_children(&channels, &channels[0]).len(), 1);
        let rows = checks(&SAFE, &roles, &channels, 4, &setup);
        let r = sev(&rows, "overwrite_10_2").unwrap();
        assert_eq!(r.severity, Severity::Bad);
        assert!(
            r.detail.contains("Category c10 and its 1 synced")
                && r.detail.contains("Manage Channels")
        );
        assert_eq!(
            serde_json::to_value(&r.fix).unwrap(),
            serde_json::json!({"label": "Strip permissions", "role_id": "2", "channel_id": "10"})
        );
        assert!(sev(&rows, "roles_bulk").unwrap().detail.starts_with("3 "));
        let fixed = stripped_overwrite(&channels[0].overwrites[1], 1, &setup);
        assert_eq!((fixed.allow, fixed.deny), (0x400, 0x800));
        // Main roles are held to the @everyone rules.
        let main = Setup {
            main_roles: vec![2],
            ..setup
        };
        assert!(overwrite_targets(&channels, &roles, 1, &main)
            .iter()
            .any(|(c, o)| (c.id, o.target) == (13, 2)));
    }

    #[test]
    fn public_log_channels_are_flagged_and_made_private() {
        const EVERYONE: u64 = 1;
        let roles = [role(EVERYONE, 0, false, VIEW), role(99, 1, false, 0)];
        let hidden = |deny| OverwriteImg {
            target: EVERYONE,
            is_role: true,
            allow: 0,
            deny,
        };
        let channels = [
            chan(20, 0, None, vec![]),
            chan(21, 0, None, vec![hidden(VIEW)]),
            // Hidden from @everyone, but the main role 99 is let back in.
            chan(22, 0, None, vec![hidden(VIEW), ow(99, true, VIEW)]),
            // Not a log channel.
            chan(23, 0, None, vec![]),
        ];
        let setup = Setup {
            log_channels: vec![20, 21, 22],
            ..setup(None, true)
        };
        let rows = checks(&SAFE, &roles, &channels, 4, &setup);
        let flagged: Vec<_> = rows
            .iter()
            .filter(|r| r.id.starts_with("log_channel_public_"))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(flagged, ["log_channel_public_20", "log_channel_public_22"]);
        let r = sev(&rows, "log_channel_public_20").unwrap();
        assert_eq!(r.severity, Severity::Bad);
        assert_eq!(
            serde_json::to_value(&r.fix).unwrap(),
            serde_json::json!({"label": "Make private", "channel_id": "20", "setting": PRIVATE})
        );
        // Administrator sees through overwrites: flagged, but no fix button.
        let admin = [role(EVERYONE, 0, false, ADMINISTRATOR)];
        let rows = checks(&SAFE, &admin, &channels, 4, &setup);
        let r = sev(&rows, "log_channel_public_21").unwrap();
        assert!(r.fix.is_none() && r.detail.contains("Administrator"));
        // An @everyone without View and no overwrites is private already.
        let closed = [role(EVERYONE, 0, false, 0)];
        assert!(!is_public(&channels[0], &closed, EVERYONE, &[]));
        // The fix: @everyone and main roles denied View, the bot let in; nothing once private.
        let fix = private_overwrites(&channels[2], EVERYONE, 7, &[99]);
        assert_eq!(
            fix.iter()
                .map(|o| (o.target, o.allow, o.deny))
                .collect::<Vec<_>>(),
            [(EVERYONE, 0, VIEW), (99, 0, 0x800 | VIEW), (7, BOT_LOG, 0)]
        );
        let mut fixed = channels[2].clone();
        fixed.overwrites = fix;
        assert!(!is_public(&fixed, &roles, EVERYONE, &[99]));
        assert!(private_overwrites(&fixed, EVERYONE, 7, &[99]).is_empty());
        assert!(private_overwrites(&channels[1], EVERYONE, 7, &[99]).is_empty());
    }
}
