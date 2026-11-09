//! Security score: risky Discord settings a raider or nuker would use. Shared by `/security` and
//! the dashboard Overview, which runs the `fix` of a row with one click.
use crate::RoleImg;
use serde::Serialize;

/// Administrator, Manage Server, Manage Roles, Manage Channels, Ban, Kick, Manage Webhooks.
pub const DANGEROUS: u64 = 0x8 | 0x20 | 0x1000_0000 | 0x10 | 0x4 | 0x2 | 0x2000_0000;
const ADMINISTRATOR: u64 = 0x8;
/// What @everyone must not have: [`DANGEROUS`] plus Mention @everyone and Manage Messages.
const EVERYONE_DANGEROUS: u64 = DANGEROUS | (1 << 17) | (1 << 13);

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
    /// A guild setting [`setting_fix`] knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting: Option<&'static str>,
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
        setting: None,
    })
}

fn set(setting: &'static str) -> Option<Fix> {
    Some(Fix {
        label: "Fix",
        role_id: None,
        setting: Some(setting),
    })
}

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

/// Roles the bot can safely strip in one go: not managed, not @everyone, below the bot.
pub fn strip_targets(roles: &[RoleImg], guild: u64, bot_top: i64) -> Vec<&RoleImg> {
    roles
        .iter()
        .filter(|r| {
            !r.managed && r.id != guild && r.position < bot_top && r.permissions & DANGEROUS != 0
        })
        .collect()
}

pub fn checks(g: &GuildSafety, roles: &[RoleImg], bot_top: i64, setup: &Setup) -> Vec<Check> {
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
            format!("{} can administer, ban, kick or manage the server.", r.name),
            strip(r.id),
        );
    }
    let strippable = strip_targets(roles, g.id, bot_top).len();
    if strippable > 1 {
        row(
            "roles_bulk",
            Severity::Warn,
            "Remove dangerous permissions from all roles",
            format!(
                "{strippable} roles below the bot can administer, ban, kick or manage the server."
            ),
            Some(Fix {
                label: "Strip all",
                role_id: Some("*".into()),
                setting: None,
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
        }
    }

    fn sev<'a>(rows: &'a [Check], id: &str) -> Option<&'a Check> {
        rows.iter().find(|r| r.id == id)
    }

    #[test]
    fn server_settings_checks() {
        let setup = setup(None, true);
        let rows = checks(&SAFE, &[], 0, &setup);
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
        let rows = checks(&risky, &[], 0, &setup);
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
        ];
        let rows = checks(&SAFE, &roles, 4, &setup(Some(9), false));
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"role_2") && !ids.contains(&"role_3") && !ids.contains(&"role_4"));
        assert!(ids.contains(&"log_fallback") && ids.contains(&"bot_position"));
        assert_eq!(sev(&rows, "role_2").unwrap().fix, strip(2));
        let rows = checks(&SAFE, &roles, 2, &setup(Some(9), true));
        assert!(sev(&rows, "quarantine_above_bot").is_some());
    }

    #[test]
    fn everyone_with_mass_mention_is_flagged_and_strippable() {
        let roles = [role(1, 0, false, (1 << 17) | 0x400)];
        let rows = checks(&SAFE, &roles, 4, &setup(None, true));
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
        let rows = checks(&SAFE, &[], 0, &setup);
        // Only the missing quarantine role and the bot role (none here) fail.
        assert_eq!(rows.len(), 10);
        assert_eq!(score(&rows), 8 * 100 / 10);
        assert_eq!(score(&[]), 0);
    }

    #[test]
    fn setup_rows_follow_permits_whitelist_and_bot_role() {
        let roles = [role(1, 0, false, 0), role(7, 5, true, ADMINISTRATOR)];
        let good = Setup {
            bot_roles: vec![7],
            ..setup(None, true)
        };
        let rows = checks(&SAFE, &roles, 5, &good);
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
            ..good
        };
        let rows = checks(&SAFE, &roles, 5, &bad);
        for id in [
            "bot_admin_only",
            "staff_permits",
            "whitelist",
            "everyone_perms",
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
        let v = serde_json::to_value(checks(&SAFE, &[role(1, 0, false, 0x8)], 0, &setup)).unwrap();
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
}
