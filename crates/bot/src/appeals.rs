//! The dashboard appeal link that goes into the DM of a member about to be banned or timed out.
use tommybot_core::sections::AppealsCfg;
use twilight_model::id::{marker::GuildMarker, Id};

pub fn link(base: &str, guild: Id<GuildMarker>) -> String {
    format!("{}/#/appeal/{guild}", base.trim_end_matches('/'))
}

/// Line appended to a ban or timeout DM, so the member gets one message and not two.
pub fn note(cfg: &AppealsCfg, guild: Id<GuildMarker>) -> Option<String> {
    let base = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| cfg.enabled && !u.is_empty())?;
    Some(format!(
        "You can appeal this on the dashboard: {}",
        link(&base, guild)
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_has_no_double_slash() {
        let g = twilight_model::id::Id::new(5);
        assert_eq!(super::link("https://x.io/", g), "https://x.io/#/appeal/5");
    }
}
