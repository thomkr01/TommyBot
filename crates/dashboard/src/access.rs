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
pub const SENSITIVE: &[&str] = &["antinuke", "whitelist"];

pub const SECTIONS: &[&str] = &[
    "wizard", "whitelist", "antinuke", "panic", "backups", "automod", "filters",
    "discord_automod", "condition_automod", "joingate", "joinraid", "verification", "lockdown",
    "moderation", "warn_points", "punish", "appeals", "logging", "utility",
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
    fn levels() {
        let perms = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let l = |o, e, t, p: &[&str]| level(&Facts { discord_owner: o, extra_owner: e, trusted_admin: t, permit_perms: &perms(p) });
        assert_eq!(l(true, false, false, &[]), Level::Owner);
        assert_eq!(l(false, true, false, &[]), Level::Owner);
        assert_eq!(l(false, false, true, &[]), Level::Admin);
        assert_eq!(l(false, false, false, &["ban", "dashboard_view", "dashboard_edit"]), Level::Editor);
        assert_eq!(l(false, false, false, &["dashboard_view"]), Level::Viewer);
        assert_eq!(l(false, false, false, &["ban"]), Level::None);
    }
}
