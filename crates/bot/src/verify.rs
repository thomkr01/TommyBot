//! Member verification: a button (or image captcha) in one channel hands out the main roles.
//! Web and third-party modes hand the member to the dashboard's /verify page, which queues a `verify_grant` job.
use anyhow::Result;
use image::{GrayImage, Luma, Rgb, RgbImage};
use serde::Deserialize;
use tommybot_automod::join::{
    account_created_ms, effective_mode, must_verify, CaptchaCfg, FailAction, VerifyCfg, VerifyMode,
};
use tommybot_core::jobs;
use tommybot_logging::emoji::ARROW;
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
    channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    guild::Permissions,
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

use tommybot_logging::embed::{verify_panel, VERIFY_HELP as HELP, VERIFY_START as START};
// The enter and code ids end in the guild id: a DM interaction has no guild of its own.
pub(crate) const ENTER: &str = "verify:enter:";
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
    tommybot_logging::embed::button(label, id, ButtonStyle::Primary)
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
    let (embed, buttons) = verify_panel(&guild_name(app, gid).await);
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

/// "2 minutes" wording for the verification period.
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
    if v.channel.is_none() {
        if let Err(e) = ensure_channel(app, gid).await {
            tracing::warn!(?e, "creating the verification channel failed");
        }
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

static CHANNEL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// First join after setup with no verification channel (`s 12`): create one that unverified
/// members can read but verified ones do not see, save it and post the button.
async fn ensure_channel(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let _one_at_a_time = CHANNEL_LOCK.lock().await;
    let cfg = app.cfg(gid.get()).await?;
    if cfg.verification.channel.is_some() {
        return Ok(());
    }
    let role = |id: Id<RoleMarker>, allow: Permissions, deny: Permissions| PermissionOverwrite {
        id: id.cast(),
        kind: PermissionOverwriteType::Role,
        allow,
        deny,
    };
    let none = Permissions::empty();
    let read = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
    let mut overwrites = vec![role(gid.cast(), read, Permissions::SEND_MESSAGES)];
    overwrites.extend(
        cfg.antinuke
            .quarantine_role
            .map(|q| role(q, read, Permissions::SEND_MESSAGES)),
    );
    overwrites.extend(
        cfg.verification
            .roles
            .iter()
            .map(|&r| role(r, none, Permissions::VIEW_CHANNEL)),
    );
    let ch = app
        .http
        .create_guild_channel(gid, "verification")
        .permission_overwrites(&overwrites)
        .await?
        .model()
        .await?;
    let saved = crate::settings::save(app, gid, app.me.get(), &["verification"], |doc| {
        doc["verification"]["channel"] = serde_json::json!(ch.id.to_string());
        Ok(())
    })
    .await?;
    if let Err(e) = saved {
        tracing::warn!(e, "saving the verification channel failed");
    }
    setup(app, gid).await?;
    Ok(())
}

/// The dashboard page passed Turnstile and sign-in: hand out the roles if the server still asks for web verification.
/// `same_ip`: members who verified from the same IP in the last day (alt accounts, or a shared network).
pub async fn web_grant(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    same_ip: &[u64],
) -> Result<()> {
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
    if !same_ip.is_empty() {
        // Only logged: families, schools and mobile networks share addresses too.
        let others = same_ip
            .iter()
            .map(|u| format!("<@{u}>"))
            .collect::<Vec<_>>();
        let log = Log::new("Possible Alt Account", ALERT)
            .user("Member", user.get())
            .line("Same IP as", &others.join(", "));
        if let Err(e) = send_log(app, gid.get(), LogKind::Verification, log).await {
            tracing::warn!(?e, "alt log failed");
        }
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

pub(crate) fn passed_embed(server: &str) -> Embed {
    EmbedBuilder::new()
        .color(GREEN)
        .title("You have been verified!")
        .description(format!(
            "{YES} You passed the verification successfully. You can now access `{server}`!"
        ))
        .build()
}

pub(crate) fn embed_reply(embed: Embed) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            embeds: Some(vec![embed]),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

pub(crate) fn web_link(gid: Id<GuildMarker>) -> InteractionResponse {
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

/// "name's Verification Result:" log.
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

/// Captcha glyphs as rows of `#` (ink) and `.`: the captcha crate's MIT-licensed font (1-9, A-Z without
/// I L O, a-z without o) plus `! $ # . : ;` drawn from DejaVu Sans Bold. Each cell keeps its own size.
static GLYPHS: std::sync::LazyLock<std::collections::BTreeMap<char, Vec<String>>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/captcha_glyphs.json")).expect("captcha glyphs")
    });

/// Picks `n` random glyphs from `pool`.
fn random_chars(pool: &[char], n: u32) -> Vec<char> {
    (0..n)
        .map(|_| pool[rand::random_range(0..pool.len())])
        .collect()
}

/// Writes `chars` left to right as black ink, each centered vertically, the row centered on a white `w` x `h` mask.
fn draw(chars: &[char], w: u32, h: u32) -> GrayImage {
    let mut img = GrayImage::from_pixel(w, h, Luma([255]));
    let glyphs: Vec<&Vec<String>> = chars.iter().map(|c| &GLYPHS[c]).collect();
    let total: i64 = glyphs.iter().map(|g| g[0].len() as i64).sum();
    let mut x = (w as i64 - total) / 2;
    for g in glyphs {
        let top = (h as i64 - g.len() as i64) / 2;
        for (dy, row) in g.iter().enumerate() {
            for (dx, b) in row.bytes().enumerate() {
                let (px, py) = (x + dx as i64, top + dy as i64);
                if b == b'#' && (0..w as i64).contains(&px) && (0..h as i64).contains(&py) {
                    img.put_pixel(px as u32, py as u32, Luma([0]));
                }
            }
        }
        x += g[0].len() as i64;
    }
    img
}

/// Shifts each column up or down along two sine periods across the width, `amp` px high.
fn wave(img: &GrayImage, amp: f64) -> GrayImage {
    let (w, h) = img.dimensions();
    let mut out = GrayImage::from_pixel(w, h, Luma([255]));
    for x in 0..w {
        let f = (x as f64 * 4.0 * std::f64::consts::PI / w as f64).sin() * amp;
        for y in 0..h {
            let ny = y as i64 - f as i64;
            if (0..h as i64).contains(&ny) {
                out.put_pixel(x, ny as u32, *img.get_pixel(x, y));
            }
        }
    }
    out
}

/// The code and its PNG.
pub(crate) fn new_captcha(c: &CaptchaCfg) -> (String, Vec<u8>) {
    let (w, h) = (c.width.clamp(150, 300), c.height.clamp(60, 200));
    // Glyphs are about 40 px tall: draw on a box scaled by 40 / font size, then resize it to w x h.
    let size = c.font_size.clamp(30, 60);
    let (cw, ch) = ((w * 40 / size).min(300), (h * 40 / size).min(200));
    let pool: Vec<char> = GLYPHS
        .keys()
        .copied()
        .filter(|&x| c.charset.allows(x))
        .collect();
    let code = random_chars(&pool, c.length.clamp(3, 7));
    let mut text = draw(&code, cw, ch);
    if c.trace {
        text = wave(&text, 8.0);
    }
    // The text alone stays in `text`, so the decoys added to `all` can be colored separately.
    let mut all = text.clone();
    if c.mixed {
        for p in all.pixels_mut().filter(|_| rand::random::<f32>() <= 0.3) {
            *p = Luma([0]);
        }
    }
    if c.spread {
        for _ in 0..12 {
            let (x, y) = (
                rand::random_range(0..cw) as i64,
                rand::random_range(0..ch) as i64,
            );
            let r = rand::random_range(2..=4i64);
            for (px, py) in (x - r..=x + r).flat_map(|px| (y - r..=y + r).map(move |py| (px, py))) {
                let inside = (px - x).pow(2) + (py - y).pow(2) <= r * r;
                if inside && (0..cw as i64).contains(&px) && (0..ch as i64).contains(&py) {
                    all.put_pixel(px as u32, py as u32, Luma([0]));
                }
            }
        }
    }
    // Faint background letters: a second code that is never the answer.
    let every: Vec<char> = GLYPHS.keys().copied().collect();
    let bg = draw(&random_chars(&every, 5), cw, ch);
    let code: String = code.into_iter().collect();
    let (font, decoy) = (rgb(&c.font_color), rgb(&c.decoy_color));
    let custom = |c: Option<Rgb<u8>>| c.filter(|c| c.0 != [0, 0, 0]);
    // Each character slot gets its own green or yellow unless the settings name a color.
    let slots = c.length.clamp(3, 7);
    let pick: Vec<(Rgb<u8>, Rgb<u8>)> = (0..slots)
        .map(|_| {
            let t = if rand::random() {
                Rgb([40, 190, 60])
            } else {
                Rgb([225, 200, 30])
            };
            let d = Rgb([
                rand::random_range(150..230),
                rand::random_range(150..230),
                rand::random_range(150..230),
            ]);
            (custom(font).unwrap_or(t), custom(decoy).unwrap_or(d))
        })
        .collect();
    let mut img = RgbImage::from_fn(all.width(), all.height(), |x, y| {
        let (t, d) = pick[(x * slots / all.width()) as usize];
        match (
            text.get_pixel(x, y).0[0] < 128,
            all.get_pixel(x, y).0[0] < 128,
            bg.get_pixel(x, y).0[0] < 128,
        ) {
            (true, _, _) => t,
            (_, true, _) | (_, _, true) => d,
            _ => Rgb([255, 255, 255]),
        }
    });
    if c.trace {
        // A zigzag through the text, alternating red and green.
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let steps = slots * 2;
        let at = |i: u32| {
            (
                iw * i as f32 / steps as f32,
                ih * if i.is_multiple_of(2) { 0.3 } else { 0.7 },
            )
        };
        for i in 0..steps {
            let ((x0, y0), (x1, y1)) = (at(i), at(i + 1));
            let col = if i % 2 == 0 {
                Rgb([220, 30, 30])
            } else {
                Rgb([30, 170, 50])
            };
            for k in 0..=200 {
                let f = k as f32 / 200.0;
                let (x, y) = (x0 + (x1 - x0) * f, y0 + (y1 - y0) * f);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (px, py) = (x as u32 + dx, y as u32 + dy);
                    if px < img.width() && py < img.height() {
                        img.put_pixel(px, py, col);
                    }
                }
            }
        }
    }
    let img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
    let mut png = std::io::Cursor::new(vec![]);
    let ok = img.write_to(&mut png, image::ImageFormat::Png).is_ok();
    (code, if ok { png.into_inner() } else { vec![] })
}

/// A sum a member types; no image, so screen readers and slow connections cope. Returns (question, answer).
pub(crate) fn new_sum() -> (String, String) {
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

pub(crate) const PROMPT: &str = "Please type the captcha below to be able to access this server!";
const NOTES: &str = "\n\n**Additional Notes:**\n› Type out the traced colored characters from left to right.\n› Ignore the decoy characters spread-around.\n› You don't have to respect characters cases (upper/lower case)!";

pub(crate) fn challenge_embed(body: &str, notes: bool, png: bool, limit_ms: i64) -> Embed {
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

/// Swaps the captcha image in the member's ephemeral message every `every` ms until it is
/// solved, expires or the interaction token runs out (15 minutes).
async fn regenerate(
    app: &App,
    client: &twilight_http::client::InteractionClient<'_>,
    token: &str,
    key: (u64, u64),
    expires: i64,
    every: i64,
    v: &VerifyCfg,
) {
    let stop = expires.min(now_secs() * 1000 + 14 * 60_000);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(every as u64)).await;
        let (code, png) = new_captcha(&v.captcha);
        let live = {
            let mut codes = app.captchas.lock().unwrap();
            // A newer Verify click replaces the entry (new expiry): this message is then stale.
            let live = now_secs() * 1000 < stop && codes.get(&key).is_some_and(|e| e.1 == expires);
            if live {
                codes.get_mut(&key).unwrap().0 = code;
            }
            live
        };
        let embeds = [challenge_embed(PROMPT, v.show_notes, true, v.time_limit_ms)];
        let file = [Attachment::from_bytes("captcha.png".into(), png, 1)];
        let enter = [row(button("Answer", &format!("{ENTER}{}", key.0)))];
        if !live
            || client
                .update_response(token)
                .embeds(Some(&embeds))
                .attachments(&file)
                .components(Some(&enter))
                .await
                .is_err()
        {
            return;
        }
    }
}

/// The text a member typed, wherever the modal nests the input.
pub(crate) fn typed_text(c: &ModalInteractionComponent) -> Option<String> {
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
            // Owners are immune and never need to verify.
            if matches!(cfg.verification.verify.mode, VerifyMode::Web) {
                let owner = app.guild_info(gid).await?.owner;
                if matches!(
                    app.level(gid, owner, user).await?,
                    tommybot_core::permits::Level::Owner
                        | tommybot_core::permits::Level::ExtraOwner
                ) {
                    let t = "The server owner and extra owners cannot verify; you are exempt.";
                    client.create_response(i.id, &i.token, &reply(t)).await?;
                    return Ok(true);
                }
            }
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
                    let png_shown = png.is_some();
                    let embed = challenge_embed(
                        &body,
                        v.show_notes && png.is_some(),
                        png.is_some(),
                        v.time_limit_ms,
                    );
                    let key = (gid.get(), user.get());
                    let expires = now_secs() * 1000 + v.time_limit_ms;
                    app.captchas.lock().unwrap().insert(key, (code, expires));
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
                        if let (Some(every), true) = (v.regen_ms, png_shown) {
                            regenerate(app, &client, &i.token, key, expires, every.max(5_000), v)
                                .await;
                        }
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
            let solved = {
                let mut codes = app.captchas.lock().unwrap();
                let key = (gid.get(), user.get());
                let ok = codes.get(&key).is_some_and(|(code, expires)| {
                    *expires > now_secs() * 1000 && code.eq_ignore_ascii_case(typed.trim())
                });
                if ok {
                    codes.remove(&key);
                }
                ok
            };
            if solved {
                let cfg = app.cfg(gid.get()).await?;
                grant(app, gid, user, &cfg.verification).await?;
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
                    .title("❌ Unsuccessful Operation!")
                    .description(format!(
                        "{ARROW} Wrong answer, try again before it's too late."
                    ))
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
    fn every_charset_draws_only_its_characters() {
        for name in tommybot_automod::join::Charset::NAMES {
            let charset = serde_json::from_value(serde_json::json!(name)).unwrap();
            let (code, png) = new_captcha(&CaptchaCfg {
                charset,
                ..Default::default()
            });
            assert!(!png.is_empty() && code.len() == 6, "{name}");
            assert!(code.chars().all(|c| charset.allows(c)), "{name}: {code}");
        }
        assert!("!$#.:;".chars().all(|c| GLYPHS.contains_key(&c)));
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
    fn captcha_defaults_draw_green_yellow_text_and_a_red_line() {
        let img = image::load_from_memory(&new_captcha(&CaptchaCfg::default()).1)
            .unwrap()
            .into_rgb8();
        assert_eq!(img.dimensions(), (300, 100));
        let has = |f: fn(&Rgb<u8>) -> bool| img.pixels().any(f);
        assert!(has(|p| p.0[0] > 200 && p.0[1] < 80), "red line");
        assert!(
            has(|p| p.0[1] > 150 && p.0[0] < 100 && p.0[2] < 100),
            "green"
        );
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
