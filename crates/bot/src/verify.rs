//! Member verification: a button (or image captcha) in one channel hands out the main roles.
//! Web and third-party modes hand the member to the dashboard's /verify page, which queues a `verify_grant` job.
use anyhow::Result;
use captcha::{
    filters::{Dots, Noise, Wave},
    Captcha,
};
use image::{Rgb, RgbImage};
use serde::Deserialize;
use tommybot_automod::join::{
    account_created_ms, effective_mode, must_verify, CaptchaCfg, FailAction, VerifyCfg, VerifyMode,
};
use tommybot_core::jobs;
use tommybot_logging::{
    embed::{ALERT, DONE, GREEN, NO, RED, YES},
    Log, LogKind,
};
use twilight_model::{
    application::interaction::{modal::ModalInteractionComponent, Interaction, InteractionData},
    channel::message::{
        component::{ActionRow, Button, ButtonStyle, Component, TextInput, TextInputStyle},
        Embed, MessageFlags,
    },
    http::{
        attachment::Attachment,
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    },
    id::{
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
    user::User,
};
use twilight_util::builder::embed::{EmbedBuilder, EmbedFooterBuilder, ImageSource};

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
const HELP: &str = "verify:help";
// The enter and code ids end in the guild id: a DM interaction has no guild of its own.
const ENTER: &str = "verify:enter:";
const CODE: &str = "verify:code:";

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Say(&'static str),
    Grant,
    Web,
    Captcha,
    Simple,
    Dm,
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
        VerifyMode::Web | VerifyMode::ThirdParty => Step::Web,
        VerifyMode::SimpleCaptcha => Step::Simple,
        VerifyMode::Dm => Step::Dm,
        VerifyMode::Captcha => Step::Captcha,
    }
}

pub(crate) fn button(label: &str, id: &str) -> Component {
    styled_button(label, id, ButtonStyle::Primary)
}

fn styled_button(label: &str, id: &str, style: ButtonStyle) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id.into()),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

pub(crate) fn row(c: Component) -> Component {
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
    let embed = EmbedBuilder::new()
        .color(GREEN)
        .title("🧩 Verification Required!")
        .description(format!(
            "To access `{}`, you need to pass the verification first.\n› Press on the **Verify** button below.",
            guild_name(app, gid).await
        ))
        .build();
    let buttons = Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            styled_button("Verify", START, ButtonStyle::Success),
            styled_button("Help", HELP, ButtonStyle::Secondary),
        ],
    });
    app.http
        .create_message(ch)
        .embeds(&[embed])
        .components(&[buttons])
        .await?;
    Ok(format!("Posted the verify button in <#{ch}>"))
}

async fn guild_name(app: &App, gid: Id<GuildMarker>) -> String {
    async { anyhow::Ok(app.http.guild(gid).await?.model().await?.name) }
        .await
        .unwrap_or_else(|_| "the server".into())
}

/// Wick's "2 minutes" wording for the verification period.
fn period(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        s if s % 60 == 0 && s >= 60 => {
            format!("{} minute{}", s / 60, if s == 60 { "" } else { "s" })
        }
        s => format!("{s} seconds"),
    }
}

/// Steps shown by the Help button, adapted to the mode.
fn help_steps(v: &VerifyCfg) -> String {
    let mut steps = vec!["Press the **Verify** button.".to_string()];
    steps.extend(match v.mode {
        VerifyMode::Button => vec!["You get access right away.".into()],
        VerifyMode::Web | VerifyMode::ThirdParty => {
            vec!["Open the link and finish the check on the website.".into()]
        }
        VerifyMode::SimpleCaptcha => vec![
            "Solve the question you are shown.".into(),
            "Press **Answer** and type the result.".into(),
        ],
        _ => vec![
            "Read the captcha image you are shown.".into(),
            "Press **Answer** and type the characters.".into(),
        ],
    });
    if v.on_fail != FailAction::Nothing {
        steps.push(format!("Finish within {}.", period(v.time_limit_ms)));
    }
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("**{}.** {s}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Gives a fresh joiner until the time limit to verify; see [`deadline`].
pub async fn on_join(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    suspicious: bool,
    is_bot: bool,
) {
    let Ok(cfg) = app.cfg(gid.get()).await else {
        return;
    };
    let v = &cfg.verification;
    if is_bot || !v.enabled || !must_verify(&v.verify, suspicious, false) {
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

/// The dashboard page passed Turnstile and sign-in: hand out the roles if the server still asks for web verification.
pub async fn web_grant(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<()> {
    let cfg = app.cfg(gid.get()).await?;
    let v = &cfg.verification;
    if !v.enabled || !matches!(v.verify.mode, VerifyMode::Web | VerifyMode::ThirdParty) {
        return Ok(());
    }
    let Ok(member) = app.http.guild_member(gid, user).await else {
        return Ok(());
    };
    if member
        .model()
        .await?
        .roles
        .iter()
        .any(|r| v.roles.contains(r))
    {
        return Ok(());
    }
    grant(app, gid, user, v).await
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
        LogKind::Verification,
        result_log(
            &member.user,
            ALERT,
            &format!(
                "{NO} `{}` did not verify in time: **{:?}**",
                member.user.name, v.verify.on_fail
            ),
        ),
    )
    .await
}

pub(crate) fn ephemeral(
    kind: InteractionResponseType,
    content: Option<&str>,
) -> InteractionResponse {
    InteractionResponse {
        kind,
        data: Some(InteractionResponseData {
            content: content.map(Into::into),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

fn passed_embed(server: &str) -> Embed {
    EmbedBuilder::new()
        .color(GREEN)
        .title("You have been verified!")
        .description(format!(
            "✅ You passed the verification successfully. You can now access `{server}`!"
        ))
        .build()
}

fn embed_reply(embed: Embed) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

fn web_link(gid: Id<GuildMarker>) -> InteractionResponse {
    let base = std::env::var("DASHBOARD_URL").unwrap_or_default();
    let mut r = ephemeral(
        InteractionResponseType::ChannelMessageWithSource,
        Some("The dashboard URL is not configured, ask the server staff."),
    );
    if let (Some(d), false) = (r.data.as_mut(), base.is_empty()) {
        d.content = Some("Open the page to verify.".into());
        d.components = Some(vec![row(Component::Button(Button {
            id: None,
            custom_id: None,
            disabled: false,
            emoji: None,
            label: Some("Verify on the website".into()),
            style: ButtonStyle::Link,
            url: Some(format!("{}/verify/{gid}", base.trim_end_matches('/'))),
            sku_id: None,
        }))]);
    }
    r
}

/// Hands out the verification roles; a member quarantined on join leaves quarantine.
pub(crate) async fn grant(
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
    if let Ok(u) = async { anyhow::Ok(app.http.user(user).await?.model().await?) }.await {
        let status = format!("{YES} `{}` has passed verification successfully.", u.name);
        let log = result_log(&u, DONE, &status);
        if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
            tracing::warn!(?e, "verification log failed");
        }
    }
    Ok(())
}

/// Wick's "name's Verification Result:" log.
fn result_log(u: &User, color: u32, status: &str) -> Log {
    Log::new(&format!("{}'s Verification Result:", u.name), color)
        .thumbnail(u)
        .line("Member", &format!("{} [{}]", u.name, u.id))
        .line(
            "Creation",
            &format!("<t:{}:F>", account_created_ms(u.id.get()) / 1000),
        )
        .field("Status:", status)
}

fn rgb(hex: &str) -> Option<Rgb<u8>> {
    let hex = hex.trim_start_matches('#');
    let n = u32::from_str_radix(hex, 16)
        .ok()
        .filter(|_| hex.len() == 6)?;
    Some(Rgb([(n >> 16) as u8, (n >> 8) as u8, n as u8]))
}

/// The code and its PNG. Sync so the non-Send generator never crosses an await.
fn new_captcha(c: &CaptchaCfg) -> (String, Vec<u8>) {
    let (w, h) = (c.width.clamp(150, 300), c.height.clamp(60, 200));
    // The library draws 40 px text on a fixed 400x300 canvas: crop a smaller or larger box and scale it to
    // w x h. These bounds keep the crop inside the canvas and the text mostly inside the crop.
    let size = c.font_size.clamp(30, 60);
    let (cw, ch) = ((w * 40 / size).min(300), (h * 40 / size).min(200));
    let mut cap = Captcha::new();
    cap.add_chars(c.length.clamp(3, 7));
    if c.trace {
        cap.apply_filter(Wave::new(2.0, 8.0));
    }
    cap.view(cw, ch);
    // The text alone, so the decoys added next can be told apart and colored separately.
    let text = cap.as_png();
    if c.mixed {
        cap.apply_filter(Noise::new(0.3));
    }
    if c.spread {
        cap.apply_filter(Dots::new(12).min_radius(2).max_radius(4));
    }
    let load = |png: Option<Vec<u8>>| Some(image::load_from_memory(&png?).ok()?.into_luma8());
    let (Some(text), Some(all)) = (load(text), load(cap.as_png())) else {
        return (cap.chars_as_string(), vec![]);
    };
    let black = Rgb([0, 0, 0]);
    let (font, decoy) = (
        rgb(&c.font_color).unwrap_or(black),
        rgb(&c.decoy_color).unwrap_or(black),
    );
    let img = RgbImage::from_fn(all.width(), all.height(), |x, y| {
        match (
            text.get_pixel(x, y).0[0] < 128,
            all.get_pixel(x, y).0[0] < 128,
        ) {
            (true, _) => font,
            (false, true) => decoy,
            _ => Rgb([255, 255, 255]),
        }
    });
    let img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
    let mut png = std::io::Cursor::new(vec![]);
    let ok = img
        .write_to(&mut png, image::ImageOutputFormat::Png)
        .is_ok();
    (
        cap.chars_as_string(),
        if ok { png.into_inner() } else { vec![] },
    )
}

/// A sum a member types; no image, so screen readers and slow connections cope. Returns (question, answer).
fn new_sum() -> (String, String) {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let (a, b) = (r % 9 + 1, (r >> 8) % 9 + 1);
    (format!("What is {a} + {b}?"), (a + b).to_string())
}

/// Which guild a verify interaction belongs to; DM ones carry it in the custom id.
fn guild_of(i: &Interaction, id: &str) -> Option<Id<GuildMarker>> {
    i.guild_id
        .or_else(|| id.rsplit(':').next()?.parse().ok().map(Id::new))
}

const PROMPT: &str = "Please type the captcha below to be able to access this server!";
const NOTES: &str = "\n\n**Additional Notes:**\n› Type out the traced colored characters from left to right.\n› Ignore the decoy characters spread-around.\n› You don't have to respect characters cases (upper/lower case)!";

fn challenge_embed(body: &str, notes: bool, png: bool, limit_ms: i64) -> Embed {
    let mut e = EmbedBuilder::new()
        .color(GREEN)
        .title("🧩 Hello! Are you human? Let's find out!")
        .description(format!(
            "```\n{body}\n```{}",
            if notes { NOTES } else { "" }
        ))
        .footer(EmbedFooterBuilder::new(format!(
            "Verification Period: {}",
            period(limit_ms)
        )));
    if let (true, Ok(img)) = (png, ImageSource::attachment("captcha.png")) {
        e = e.image(img);
    }
    e.build()
}

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
    let id = match &i.data {
        Some(InteractionData::MessageComponent(c)) => c.custom_id.as_str(),
        Some(InteractionData::ModalSubmit(m)) => m.custom_id.as_str(),
        _ => return Ok(false),
    };
    if !id.starts_with("verify:") {
        return Ok(false);
    }
    let (Some(gid), Some(u)) = (guild_of(i, id), i.author()) else {
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
        Some(InteractionData::MessageComponent(c)) if c.custom_id == HELP => {
            let cfg = app.cfg(gid.get()).await?;
            let help = EmbedBuilder::new()
                .color(GREEN)
                .title("🧩 Verification Help")
                .description(help_steps(&cfg.verification.verify))
                .build();
            client
                .create_response(i.id, &i.token, &embed_reply(help))
                .await?;
            Ok(true)
        }
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
                Step::Web => {
                    client
                        .create_response(i.id, &i.token, &web_link(gid))
                        .await?;
                }
                Step::Grant => {
                    grant(app, gid, user, &cfg.verification).await?;
                    let done = passed_embed(&guild_name(app, gid).await);
                    client
                        .create_response(i.id, &i.token, &embed_reply(done))
                        .await?;
                }
                step => {
                    let v = &cfg.verification.verify;
                    let (code, png, body) = match step {
                        Step::Simple => {
                            let (q, a) = new_sum();
                            (a, None, q)
                        }
                        _ => {
                            let (code, png) = new_captcha(&v.captcha);
                            (code, Some(png), PROMPT.to_string())
                        }
                    };
                    let embed = challenge_embed(
                        &body,
                        v.show_notes && png.is_some(),
                        png.is_some(),
                        v.time_limit_ms,
                    );
                    app.captchas
                        .lock()
                        .unwrap()
                        .insert((gid.get(), user.get()), code);
                    let enter = [row(button("Answer", &format!("{ENTER}{gid}")))];
                    let embeds = [embed];
                    let file = png.map(|p| [Attachment::from_bytes("captcha.png".into(), p, 1)]);
                    if step == Step::Dm {
                        let ch = app.http.create_private_channel(user).await;
                        let sent = match ch {
                            Ok(ch) => {
                                let ch = ch.model().await?;
                                let mut m = app.http.create_message(ch.id).embeds(&embeds);
                                if let Some(f) = &file {
                                    m = m.attachments(f);
                                }
                                m.components(&enter).await.is_ok()
                            }
                            Err(_) => false,
                        };
                        let t = if sent {
                            "Check your DMs for the challenge."
                        } else {
                            "I could not DM you. Open your DMs for this server and press Verify again."
                        };
                        client.create_response(i.id, &i.token, &reply(t)).await?;
                    } else {
                        let defer = InteractionResponseType::DeferredChannelMessageWithSource;
                        client
                            .create_response(i.id, &i.token, &ephemeral(defer, None))
                            .await?;
                        let mut u = client.update_response(&i.token).embeds(Some(&embeds));
                        if let Some(f) = &file {
                            u = u.attachments(f);
                        }
                        u.components(Some(&enter)).await?;
                    }
                }
            }
            Ok(true)
        }
        Some(InteractionData::MessageComponent(c)) if c.custom_id.starts_with(ENTER) => {
            #[allow(deprecated)]
            // Discord moved labels to the Label component; a plain row still works.
            let input = Component::TextInput(TextInput {
                id: None,
                custom_id: "code".into(),
                label: Some("Your answer".into()),
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
                            custom_id: Some(format!("{CODE}{gid}")),
                            title: Some("Verify".into()),
                            components: Some(vec![row(input)]),
                            ..Default::default()
                        }),
                    },
                )
                .await?;
            Ok(true)
        }
        Some(InteractionData::ModalSubmit(m)) if m.custom_id.starts_with(CODE) => {
            let typed = m.components.iter().find_map(typed_text).unwrap_or_default();
            // Kept until it is solved, so a wrong answer can be retried.
            let expected = app
                .captchas
                .lock()
                .unwrap()
                .get(&(gid.get(), user.get()))
                .cloned();
            if expected.is_some_and(|e| e.eq_ignore_ascii_case(typed.trim())) {
                app.captchas
                    .lock()
                    .unwrap()
                    .remove(&(gid.get(), user.get()));
                let cfg = app.cfg(gid.get()).await?;
                grant(app, gid, user, &cfg.verification).await?;
                // Turn the challenge message into "Verification done." when the modal came from it.
                let edit = i.message.is_some();
                let kind = if edit {
                    InteractionResponseType::UpdateMessage
                } else {
                    InteractionResponseType::ChannelMessageWithSource
                };
                let mut resp = ephemeral(kind, Some("Verification done."));
                if let (true, Some(d)) = (edit, resp.data.as_mut()) {
                    d.embeds = Some(vec![]);
                    d.components = Some(vec![]);
                    d.attachments = Some(vec![]);
                }
                client.create_response(i.id, &i.token, &resp).await?;
                let done = passed_embed(&guild_name(app, gid).await);
                client
                    .create_followup(&i.token)
                    .embeds(&[done])
                    .flags(MessageFlags::EPHEMERAL)
                    .await?;
            } else {
                let fail = EmbedBuilder::new()
                    .color(RED)
                    .title("❎ Unsuccessful Operation!")
                    .description("› Wrong answer, try again before it's too late.")
                    .build();
                client
                    .create_response(i.id, &i.token, &embed_reply(fail))
                    .await?;
            }
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
        for (mode, step) in [
            (VerifyMode::SimpleCaptcha, Step::Simple),
            (VerifyMode::Dm, Step::Dm),
            (VerifyMode::Captcha, Step::Captcha),
        ] {
            assert_eq!(
                decide(&cfg(mode, VerifyTarget::Everyone), false, false, false),
                step
            );
        }
        assert!(matches!(
            decide(&VerificationCfg::default(), false, true, false),
            Step::Say(_)
        ));
        for mode in [VerifyMode::Web, VerifyMode::ThirdParty] {
            assert_eq!(
                decide(&cfg(mode, VerifyTarget::Everyone), false, true, false),
                Step::Web
            );
        }
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
                    font_size: width,
                    font_color: "#ff0000".into(),
                    decoy_color: "bad".into(),
                    ..Default::default()
                };
                assert!(!new_captcha(&c).1.is_empty());
            }
        }
    }

    #[test]
    fn captcha_colors_text_and_decoys_apart() {
        let c = CaptchaCfg {
            font_size: 50,
            font_color: "#ff0000".into(),
            decoy_color: "#0000ff".into(),
            ..Default::default()
        };
        let img = image::load_from_memory(&new_captcha(&c).1)
            .unwrap()
            .into_rgb8();
        assert_eq!(img.dimensions(), (300, 100));
        let has = |f: fn(&Rgb<u8>) -> bool| img.pixels().any(f);
        assert!(has(|p| p.0[0] > 200 && p.0[2] < 50), "red text");
        assert!(has(|p| p.0[2] > 200 && p.0[0] < 50), "blue decoys");
    }

    #[test]
    fn sum_answer_matches_question() {
        for _ in 0..20 {
            let (q, a) = new_sum();
            let n: Vec<u32> = q
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|x| x.parse().ok())
                .collect();
            assert_eq!(n.iter().sum::<u32>().to_string(), a);
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
