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
pub use tommybot_core::sections::SENSITIVE;

/// Level and step-up a write to `section` needs.
pub fn write_bar(section: &str) -> (Level, bool) {
    // Trusted Admins cannot change Miscellaneous or Anti-Nuke (and Panic).
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
    "joingate",
    "joinraid",
    "verification",
    "tickets",
    "lockdown",
    "catch",
    "appeals",
    "misc",
    "logging",
];

/// What a dashboard page shows the user: everything, a greyed-out read-only view, or nothing.
/// Only a hint for the UI; every API route still checks on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PageAccess {
    Full,
    Read,
    Hidden,
}

/// Page names as the UI routes them (`#/<guild>/<page>`).
pub const PAGES: &[&str] = &[
    "wizard",
    "overview",
    "misc",
    "permits",
    "logging",
    "backups",
    "channel_setup",
    "lockdown",
    "lockdown_settings",
    "appeal_list",
    "appeals",
    "appeal_blacklist",
    "automod",
    "filters",
    "discord_automod",
    "whitelist",
    "automod_panic",
    "antinuke",
    "antinuke_filters",
    "antinuke_panic",
    "joingate",
    "joingate_log",
    "joinraid",
    "raid_log",
    "verification",
    "cases",
    "mod_stats",
    "notes",
    "report_list",
    "report_blacklist",
    "tickets",
    "catch",
    "revert",
    "history",
    "embeds",
    "rescue",
];

/// Mirrors the checks of the routes each page calls (`api::staff`, `require`, `write_bar`).
pub fn page(page: &str, l: Level, perms: &[String]) -> PageAccess {
    use PageAccess::*;
    let has = |want: &[&str]| l >= Level::Admin || perms.iter().any(|p| want.contains(&p.as_str()));
    let need = |min: Level| if l >= min { Full } else { Read };
    if l == Level::None {
        return Hidden;
    }
    match page {
        // Before `has`, which lets every trusted admin through.
        "mod_stats" if mod_stats(l, perms) => Full,
        "mod_stats" => Hidden,
        "cases" | "raid_log" | "joingate_log" if has(&["cases"]) => Full,
        "notes" if has(&["notes_manage"]) => Full,
        "notes" if has(&["notes_view"]) => Read,
        "report_list" | "report_blacklist" if has(&["reports"]) => Full,
        "appeal_list" if has(&["appeals"]) => Full,
        "cases" | "raid_log" | "joingate_log" | "notes" | "report_list" | "report_blacklist"
        | "appeal_list" => Hidden,
        "rescue" if l < Level::Owner => Hidden,
        "revert" if l < Level::Editor => Hidden,
        // Pages whose buttons are mostly for reading; the server refuses the few writes.
        "overview" | "history" | "backups" => Full,
        // Thomas 2026-11-11: only owners change channels; trusted admins may look.
        "channel_setup" if l < Level::Admin => Hidden,
        "channel_setup" => need(Level::Owner),
        // Lock, unlock and the member update are the page; a viewer only watches.
        "lockdown" => need(Level::Editor),
        "permits" | "revert" => need(Level::Admin),
        "antinuke" | "antinuke_filters" => need(write_bar("antinuke").0),
        "automod_panic" | "antinuke_panic" => need(write_bar("panic").0),
        "lockdown_settings" => need(write_bar("lockdown").0),
        "appeal_blacklist" | "appeals" => need(write_bar("appeals").0),
        s if SECTIONS.contains(&s) => need(write_bar(s).0),
        _ => need(Level::Editor),
    }
}

/// Moderator stats show what colleagues did: owners, and roles an owner gave View Mod Stats (Thomas 2026-11-11).
pub fn mod_stats(l: Level, perms: &[String]) -> bool {
    l == Level::Owner || (l > Level::None && perms.iter().any(|p| p == "mod_stats"))
}

/// Whether a bot command needing `perm` is open to the user, as the bot's `commands` list decides.
pub fn command(perm: &str, l: Level, perms: &[String]) -> bool {
    let custom = tommybot_core::permits::Level::Custom {
        rank: 0,
        perms: perms.to_vec(),
        whitelist: vec![],
        limits: Default::default(),
    };
    match perm {
        "" => true,
        "*" => l > Level::None,
        "owner" => l == Level::Owner,
        p => l >= Level::Admin || custom.has(p),
    }
}

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
    fn pages_follow_permits() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(page("cases", Level::Viewer, &[]), PageAccess::Hidden);
        assert_eq!(
            page("cases", Level::Viewer, &p(&["cases"])),
            PageAccess::Full
        );
        assert_eq!(
            page("notes", Level::Viewer, &p(&["notes_view"])),
            PageAccess::Read
        );
        assert_eq!(page("automod", Level::Viewer, &[]), PageAccess::Read);
        assert_eq!(page("lockdown", Level::Viewer, &[]), PageAccess::Read);
        assert_eq!(page("lockdown", Level::Editor, &[]), PageAccess::Full);
        assert_eq!(page("automod", Level::Editor, &[]), PageAccess::Full);
        assert_eq!(page("antinuke", Level::Admin, &[]), PageAccess::Read);
        assert_eq!(page("misc", Level::Admin, &[]), PageAccess::Read);
        assert_eq!(page("permits", Level::Editor, &[]), PageAccess::Read);
        assert_eq!(page("rescue", Level::Admin, &[]), PageAccess::Hidden);
        assert_eq!(page("cases", Level::Admin, &[]), PageAccess::Full);
        for l in [Level::Viewer, Level::Editor, Level::Admin] {
            assert_eq!(page("mod_stats", l, &p(&["cases"])), PageAccess::Hidden);
            assert_eq!(page("mod_stats", l, &p(&["mod_stats"])), PageAccess::Full);
        }
        assert_eq!(page("mod_stats", Level::None, &p(&["mod_stats"])), PageAccess::Hidden);
        assert_eq!(page("mod_stats", Level::Owner, &[]), PageAccess::Full);
        assert_eq!(
            page("channel_setup", Level::Editor, &[]),
            PageAccess::Hidden
        );
        assert_eq!(page("channel_setup", Level::Admin, &[]), PageAccess::Read);
        assert!(PAGES
            .iter()
            .all(|s| page(s, Level::Owner, &[]) == PageAccess::Full));
        assert!(PAGES
            .iter()
            .all(|s| page(s, Level::None, &[]) == PageAccess::Hidden));
    }

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
