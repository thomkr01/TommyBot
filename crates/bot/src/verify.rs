//! Member verification: a button (or image captcha) in one channel hands out the main roles.
//! shortcut: web mode needs the web crate, DM and simple-captcha modes fall back to the image captcha.
use anyhow::Result;
use captcha::{
    filters::{Dots, Noise, Wave},
    Captcha,
};
use serde::Deserialize;
use tommybot_automod::join::{
    effective_mode, must_verify, CaptchaCfg, FailAction, VerifyCfg, VerifyMode,
};
use tommybot_core::jobs;
use twilight_model::{
    application::interaction::{modal::ModalInteractionComponent, Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component, TextInput, TextInputStyle},
        MessageFlags,
    },
    http::{
        attachment::Attachment,
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    },
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::{now_secs, App},
    events::send_log,
};

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct VerificationCfg {
    pub enabled: bool,
    /// Where `verifysetup` posts the button.
    pub channel: Option<Id<ChannelMarker>>,
    /// Roles handed out on a pass.
    pub roles: Vec<Id<RoleMarker>>,
    #[serde(flatten)]
    pub verify: VerifyCfg,
}

const START: &str = "verify:start";
const ENTER: &str = "verify:enter";
const CODE: &str = "verify:code";

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Say(&'static str),
    Grant,
    Captcha,
}

/// What a click on the verify button leads to.
fn decide(cfg: &VerificationCfg, raid_active: bool, suspicious: bool, has_role: bool) -> Step {
    if !cfg.enabled || cfg.roles.is_empty() {
        return Step::Say("Verification is not set up on this server.");
    }
    if has_role {
        return Step::Say("You are already verified.");
    }
    if !must_verify(&cfg.verify, suspicious, false) {
        return Step::Grant;
    }
    match effective_mode(&cfg.verify, raid_active) {
        VerifyMode::Button => Step::Grant,
        VerifyMode::Web => Step::Say("Verification for this server happens on its web page."),
        _ => Step::Captcha,
    }
}

fn button(label: &str, id: &str) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id.into()),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Primary,
        url: None,
        sku_id: None,
    })
}

fn row(c: Component) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![c],
    })
}

/// Posts the verify button into the configured channel (staff command `verifysetup`).
pub async fn setup(app: &App, gid: Id<GuildMarker>) -> Result<String> {
    let cfg = app.cfg(gid.get()).await?;
    let Some(ch) = cfg.verification.channel else {
        return Ok("Set a verification channel on the dashboard first.".into());
    };
    app.http
        .create_message(ch)
        .content("Press the button to verify and get access to the server.")
        .components(&[row(button("Verify", START))])
        .await?;
    Ok(format!("Posted the verify button in <#{ch}>"))
}

/// Gives a fresh joiner until the time limit to verify; see [`deadline`].
pub async fn on_join(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>, suspicious: bool) {
    let Ok(cfg) = app.cfg(gid.get()).await else {
        return;
    };
    let v = &cfg.verification;
    if !v.enabled || !must_verify(&v.verify, suspicious, false) {
        return;
    }
    if v.verify.quarantine_on_join {
        if let Err(e) = crate::antinuke::quarantine(app, gid, user).await {
            tracing::warn!(?e, "quarantining an unverified joiner failed");
        }
    }
    if v.verify.on_fail == FailAction::Nothing {
        return;
    }
    let at = now_secs() + v.verify.time_limit_ms / 1000;
    if let Err(e) = jobs::enqueue(
        &app.pool,
        gid.get() as i64,
        "verify_deadline",
        serde_json::json!({ "user": user.get() }),
        at,
    )
    .await
    {
        tracing::warn!(?e, "scheduling the verification deadline failed");
    }
}

/// Applies the fail action to a member who never verified.
pub async fn deadline(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    let Ok(member) = app.http.guild_member(gid, user).await else {
        return Ok(());
    };
    let member = member.model().await?;
    if !v.enabled || member.roles.iter().any(|r| v.roles.contains(r)) {
        return Ok(());
    }
    match v.verify.on_fail {
        FailAction::Nothing => return Ok(()),
        FailAction::Kick => {
            app.http.remove_guild_member(gid, user).await?;
        }
        FailAction::Ban => {
            app.http.create_ban(gid, user).await?;
        }
        FailAction::Quarantine => crate::antinuke::quarantine(app, gid, user).await?,
    }
    send_log(
        app,
        gid.get(),
        tommybot_logging::LogKind::JoinGate,
        &format!("<@{user}> did not verify in time: {:?}", v.verify.on_fail),
    )
    .await
}

fn ephemeral(kind: InteractionResponseType, content: Option<&str>) -> InteractionResponse {
    InteractionResponse {
        kind,
        data: Some(InteractionResponseData {
            content: content.map(Into::into),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// Hands out the verification roles; a member quarantined on join leaves quarantine.
async fn grant(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    v: &VerificationCfg,
) -> Result<()> {
    if v.verify.replace_roles || v.verify.quarantine_on_join {
        app.http
            .update_guild_member(gid, user)
            .roles(&v.roles)
            .await?;
    } else {
        for r in &v.roles {
            app.http.add_guild_member_role(gid, user, *r).await?;
        }
    }
    if v.verify.quarantine_on_join {
        sqlx::query("DELETE FROM quarantines WHERE guild_id = $1 AND user_id = $2")
            .bind(gid.get() as i64)
            .bind(user.get() as i64)
            .execute(&app.pool)
            .await?;
    }
    Ok(())
}

/// The code and its PNG. Sync so the non-Send generator never crosses an await.
fn new_captcha(c: &CaptchaCfg) -> (String, Vec<u8>) {
    let mut cap = Captcha::new();
    let hex = c.font_color.trim_start_matches('#');
    if let (6, Ok(rgb)) = (hex.len(), u32::from_str_radix(hex, 16)) {
        cap.set_color([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8]);
    }
    // The library draws on a fixed 400x300 canvas; these bounds keep the crop inside it.
    cap.add_chars(c.length.clamp(3, 7));
    if c.trace {
        cap.apply_filter(Wave::new(2.0, 8.0));
    }
    if c.mixed {
        cap.apply_filter(Noise::new(0.3));
    }
    if c.spread {
        cap.apply_filter(Dots::new(12).min_radius(2).max_radius(4));
    }
    cap.view(c.width.clamp(150, 300), c.height.clamp(60, 200));
    (cap.chars_as_string(), cap.as_png().unwrap_or_default())
}

const NOTES: &str = "Type the characters you see.\n-# Letters are not case sensitive. Ignore the dots and lines, they are only there to stop bots. Press Verify again for a new image.";

/// The text a member typed, wherever the modal nests the input.
fn typed_text(c: &ModalInteractionComponent) -> Option<String> {
    match c {
        ModalInteractionComponent::TextInput(t) => Some(t.value.clone()),
        ModalInteractionComponent::ActionRow(r) => r.components.iter().find_map(typed_text),
        ModalInteractionComponent::Label(l) => typed_text(&l.component),
        _ => None,
    }
}

/// Handles the verify button, the "enter code" button and the code modal. Returns false for other interactions.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let (Some(gid), Some(u)) = (i.guild_id, i.author()) else {
        return Ok(false);
    };
    let user = u.id;
    let client = app.http.interaction(Id::new(app.me.get()));
    let reply = |text: &'static str| {
        ephemeral(
            InteractionResponseType::ChannelMessageWithSource,
            Some(text),
        )
    };
    match &i.data {
        Some(InteractionData::MessageComponent(c)) if c.custom_id == START => {
            let cfg = app.cfg(gid.get()).await?;
            let member = i.member.as_ref();
            let has_role =
                member.is_some_and(|m| m.roles.iter().any(|r| cfg.verification.roles.contains(r)));
            let joiner = tommybot_automod::join::Joiner {
                id: u.id.get(),
                name: &u.name,
                has_avatar: u.avatar.is_some(),
                ..Default::default()
            };
            let now = now_secs() * 1000;
            let min_age = cfg.joingate.min_age_ms();
            let suspicious =
                tommybot_automod::join::is_suspicious(&joiner, &cfg.joingate, min_age, now);
            let raid = app
                .raids
                .lock()
                .unwrap()
                .get(&gid.get())
                .is_some_and(|r| r.active(now));
            match decide(&cfg.verification, raid, suspicious, has_role) {
                Step::Say(t) => {
                    client.create_response(i.id, &i.token, &reply(t)).await?;
                }
                Step::Grant => {
                    grant(app, gid, user, &cfg.verification).await?;
                    client
                        .create_response(i.id, &i.token, &reply("Verified, welcome!"))
                        .await?;
                }
                Step::Captcha => {
                    let (code, png) = new_captcha(&cfg.verification.verify.captcha);
                    app.captchas
                        .lock()
                        .unwrap()
                        .insert((gid.get(), user.get()), code);
                    client
                        .create_response(
                            i.id,
                            &i.token,
                            &ephemeral(
                                InteractionResponseType::DeferredChannelMessageWithSource,
                                None,
                            ),
                        )
                        .await?;
                    client
                        .update_response(&i.token)
                        .content(Some(if cfg.verification.verify.show_notes {
                            NOTES
                        } else {
                            "Type the characters you see."
                        }))
                        .attachments(&[Attachment::from_bytes("captcha.png".into(), png, 1)])
                        .components(Some(&[row(button("Enter code", ENTER))]))
                        .await?;
                }
            }
            Ok(true)
        }
        Some(InteractionData::MessageComponent(c)) if c.custom_id == ENTER => {
            #[allow(deprecated)]
            // Discord moved labels to the Label component; a plain row still works.
            let input = Component::TextInput(TextInput {
                id: None,
                custom_id: "code".into(),
                label: Some("Characters in the image".into()),
                max_length: Some(10),
                min_length: Some(1),
                placeholder: None,
                required: Some(true),
                style: TextInputStyle::Short,
                value: None,
            });
            client
                .create_response(
                    i.id,
                    &i.token,
                    &InteractionResponse {
                        kind: InteractionResponseType::Modal,
                        data: Some(InteractionResponseData {
                            custom_id: Some(CODE.into()),
                            title: Some("Verify".into()),
                            components: Some(vec![row(input)]),
                            ..Default::default()
                        }),
                    },
                )
                .await?;
            Ok(true)
        }
        Some(InteractionData::ModalSubmit(m)) if m.custom_id == CODE => {
            let typed = m.components.iter().find_map(typed_text).unwrap_or_default();
            let expected = app
                .captchas
                .lock()
                .unwrap()
                .remove(&(gid.get(), user.get()));
            let text = if expected.is_some_and(|e| e.eq_ignore_ascii_case(typed.trim())) {
                let cfg = app.cfg(gid.get()).await?;
                grant(app, gid, user, &cfg.verification).await?;
                "Verified, welcome!"
            } else {
                "Wrong code. Press Verify to get a new one."
            };
            client.create_response(i.id, &i.token, &reply(text)).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tommybot_automod::join::VerifyTarget;

    fn cfg(mode: VerifyMode, target: VerifyTarget) -> VerificationCfg {
        VerificationCfg {
            enabled: true,
            roles: vec![Id::new(1)],
            verify: VerifyCfg {
                mode,
                target,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn click_outcomes() {
        let c = cfg(VerifyMode::Button, VerifyTarget::Everyone);
        assert_eq!(decide(&c, false, false, false), Step::Grant);
        assert_eq!(decide(&c, true, false, false), Step::Captcha);
        assert!(matches!(decide(&c, false, false, true), Step::Say(_)));
        let c = cfg(VerifyMode::Captcha, VerifyTarget::SuspiciousOnly);
        assert_eq!(decide(&c, false, false, false), Step::Grant);
        assert_eq!(decide(&c, false, true, false), Step::Captcha);
        assert!(matches!(
            decide(&VerificationCfg::default(), false, true, false),
            Step::Say(_)
        ));
        assert!(matches!(
            decide(
                &cfg(VerifyMode::Web, VerifyTarget::Everyone),
                false,
                true,
                false
            ),
            Step::Say(_)
        ));
    }

    #[test]
    fn captcha_follows_its_settings_within_bounds() {
        let (code, png) = new_captcha(&CaptchaCfg::default());
        assert_eq!(code.chars().count(), 6);
        assert_eq!(&png[1..4], b"PNG");
        for (length, width, height) in [(1, 1, 1), (99, 9999, 9999), (7, 150, 60)] {
            for _ in 0..3 {
                let c = CaptchaCfg {
                    length,
                    width,
                    height,
                    font_color: "#ff0000".into(),
                    ..Default::default()
                };
                assert!(!new_captcha(&c).1.is_empty());
            }
        }
    }

    #[test]
    fn config_accepts_flattened_verify_fields_and_defaults() {
        let c: VerificationCfg = serde_json::from_value(
            serde_json::json!({"enabled": true, "channel": "5", "roles": ["7"], "mode": "Web"}),
        )
        .unwrap();
        assert!(c.enabled && c.verify.mode == VerifyMode::Web && c.roles.len() == 1);
        assert!(!VerificationCfg::default().enabled);
    }
}
