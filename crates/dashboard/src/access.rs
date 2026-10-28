//! Who may see and edit a guild's dashboard. Discord's Manage Server is deliberately ignored (spec 3.1).

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    None,
    Viewer,
    Editor,
    Admin, // trusted admin
    Owner, // Discord owner or extra owner
}

/// Sections that need Admin + TOTP step-up (spec 2.5.5).
pub const SENSITIVE: &[&str] = &["antinuke", "whitelist", "panic", "catch"];

/// Level and step-up a write to `section` needs.
pub fn write_bar(section: &str) -> (Level, bool) {
    // Wick: Trusted Admins cannot change Miscellaneous or Anti-Nuke (and Panic).
    match section {
        "misc" => (Level::Owner, false),
        "antinuke" | "panic" => (Level::Owner, true),
        s if SENSITIVE.contains(&s) => (Level::Admin, true),
        _ => (Level::Editor, false),
    }
}

pub const SECTIONS: &[&str] = &[
    "wizard",
    "whitelist",
    "antinuke",
    "panic",
    "backups",
    "automod",
    "filters",
    "discord_automod",
    "condition_automod",
    "joingate",
    "joinraid",
    "verification",
    "tickets",
    "lockdown",
    "catch",
    "moderation",
    "warn_points",
    "punish",
    "appeals",
    "misc",
    "logging",
    "utility",
    "channel_profiles",
];

pub struct Facts<'a> {
    pub discord_owner: bool,
    pub extra_owner: bool,
    pub trusted_admin: bool,
    /// perms of every permit the user holds through a role
    pub permit_perms: &'a [String],
}

pub fn level(f: &Facts) -> Level {
    if f.discord_owner || f.extra_owner {
        Level::Owner
    } else if f.trusted_admin {
        Level::Admin
    } else if f.permit_perms.iter().any(|p| p == "dashboard_edit") {
        Level::Editor
    } else if f.permit_perms.iter().any(|p| p == "dashboard_view") {
        Level::Viewer
    } else {
        Level::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_a_write_bar() {
        for s in SECTIONS {
            let (min, step_up) = write_bar(s);
            assert_eq!(step_up, SENSITIVE.contains(s));
            let owner_only = matches!(*s, "misc" | "antinuke" | "panic");
            let want = match (owner_only, step_up) {
                (true, _) => Level::Owner,
                (_, true) => Level::Admin,
                _ => Level::Editor,
            };
            assert_eq!(min, want);
        }
        assert!(SENSITIVE.iter().all(|s| SECTIONS.contains(s)));
        // A viewer can never write, and an editor never touches anti-nuke.
        assert!(Level::Viewer < write_bar("misc").0 && Level::Editor < write_bar("antinuke").0);
    }

    #[test]
    fn levels() {
        let perms = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let l = |o, e, t, p: &[&str]| {
            level(&Facts {
                discord_owner: o,
                extra_owner: e,
                trusted_admin: t,
                permit_perms: &perms(p),
            })
        };
        assert_eq!(l(true, false, false, &[]), Level::Owner);
        assert_eq!(l(false, true, false, &[]), Level::Owner);
        assert_eq!(l(false, false, true, &[]), Level::Admin);
        assert_eq!(
            l(
                false,
                false,
                false,
                &["ban", "dashboard_view", "dashboard_edit"]
            ),
            Level::Editor
        );
        assert_eq!(l(false, false, false, &["dashboard_view"]), Level::Viewer);
        assert_eq!(l(false, false, false, &["ban"]), Level::None);
    }
}
