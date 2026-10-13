//! DMs the dashboard appeal link to a user about to be banned or timed out.
use crate::app::App;
use tommybot_core::sections::AppealsCfg;
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

pub fn link(base: &str, guild: Id<GuildMarker>) -> String {
    format!("{}/#/appeal/{guild}", base.trim_end_matches('/'))
}

/// Best effort: closed DMs or a missing DASHBOARD_URL just mean no link.
pub async fn dm_link(app: &App, cfg: &AppealsCfg, guild: Id<GuildMarker>, user: Id<UserMarker>) {
    let Some(base) = cfg
        .enabled
        .then(|| {
            std::env::var("DASHBOARD_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .flatten()
    else {
        return;
    };
    let text = format!(
        "You can appeal this on the dashboard: {}",
        link(&base, guild)
    );
    let sent = async {
        let ch = app.http.create_private_channel(user).await?.model().await?;
        app.http.create_message(ch.id).content(&text).await?;
        anyhow::Ok(())
    };
    if let Err(e) = sent.await {
        tracing::debug!(?e, "appeal DM failed");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_has_no_double_slash() {
        let g = twilight_model::id::Id::new(5);
        assert_eq!(super::link("https://x.io/", g), "https://x.io/#/appeal/5");
    }
}
