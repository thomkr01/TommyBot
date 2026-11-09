//! The sample messages the dashboard shows in its previews. The bot builds them at startup with
//! the same functions it sends them with and saves them for the dashboard, so a preview always
//! matches what this bot version sends, custom emojis included.
use anyhow::Result;
use base64::Engine as _;
use serde_json::{json, Value};
use sqlx::PgPool;
use tommybot_automod::{
    join::{Action, RaidCfg},
    strike::{strike, StrikeCfg, StrikeState},
};
use tommybot_logging::{embed::verify_panel, Log};
use twilight_model::{
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::Id,
};

use crate::{antinuke, automod, commands, joingate, reply, tickets, verify};
use tommybot_core::sections::FilterAction;

const MEMBER: u64 = 1001;
const YOU: u64 = 1002;
const ROGUE: u64 = 1003;
const GENERAL: u64 = 2001;
const SERVER: &str = "Your Server";

/// A log as `send_log` sends it.
fn log(l: Log) -> Value {
    json!({"embeds": [l.time().build()]})
}

fn interaction(r: InteractionResponse) -> Value {
    serde_json::to_value(r.data).unwrap()
}

fn embed(e: twilight_model::channel::message::Embed) -> Value {
    json!({"embeds": [e]})
}

/// A ticket in both places: the member's DMs and the staff channel. `{prefix}` and `{text}` are
/// filled in by the dashboard from the settings.
fn tickets() -> Vec<(&'static str, Value)> {
    let user = |id: u64, name: &str| -> twilight_model::user::User {
        serde_json::from_value(
            json!({"id": id.to_string(), "username": name, "discriminator": "0", "avatar": null}),
        )
        .unwrap()
    };
    let (member, staff) = (user(MEMBER, "member"), user(YOU, "you"));
    let s = tickets::Server {
        id: Id::new(1),
        name: SERVER.into(),
        icon: None,
    };
    let (confirm, row) = tickets::confirm(&s, Id::new(1));
    let ask = "Hi, someone in the server keeps sending me scam links.";
    let (sent, got) = tickets::from_member(&s, &member, ask);
    let answer = "Thanks, we are looking into it.";
    let (dm, copy) = tickets::from_staff(&s, &staff, &member, answer, false);
    let (dm_anon, copy_anon) = tickets::from_staff(&s, &staff, &member, answer, true);
    let intro = |cmd| embed(tickets::intro("{prefix}", cmd, &member, &[]));
    let reason = "Solved, thanks for the report.";
    vec![
        (
            "ticket_confirm",
            json!({"embeds": [confirm], "components": [row]}),
        ),
        ("ticket_sent", embed(sent.build())),
        ("ticket_received", embed(got.build())),
        ("ticket_reply", embed(dm.build())),
        ("ticket_reply_anon", embed(dm_anon.build())),
        ("ticket_reply_copy", embed(copy.build())),
        ("ticket_reply_copy_anon", embed(copy_anon.build())),
        ("ticket_intro", intro(false)),
        ("ticket_intro_cmd", intro(true)),
        (
            "ticket_greeting",
            embed(tickets::notice(&s, false, "{text}", &member)),
        ),
        (
            "ticket_closing",
            embed(tickets::notice(&s, true, "{text}", &member)),
        ),
        (
            "ticket_closed",
            embed(tickets::closed(&s, reason, Some(&staff))),
        ),
        (
            "ticket_closed_anon",
            embed(tickets::closed(&s, reason, None)),
        ),
    ]
}

fn messages() -> Value {
    let full = reply::case_text(false, (4151, "ban", MEMBER), Some("toxic"), YOU);
    let compact = reply::case_text(true, (4151, "ban", MEMBER), Some("toxic"), YOU);
    let act = strike(&mut StrikeState::default(), &StrikeCfg::default(), 0);
    let sum = antinuke::RevertSummary {
        ops: 5,
        done: vec![],
        failed: 0,
        reinvite: vec![],
        recreated: 5,
    };
    let nuke = |auto, quarantined| {
        let why = format!(
            "{:?} limit reached (5/min, 20/hour)",
            tommybot_antinuke::Action::ChannelDelete
        );
        log(antinuke::report_log(
            (4152, "n7Kd2x"),
            ROGUE,
            (auto, quarantined),
            &why,
            &sum,
        ))
    };
    let raid = RaidCfg {
        action: Action::Kick,
        ..RaidCfg::default()
    };
    let raiders = [
        (1_234_567_890_123, "free nitro"),
        (1_234_567_890_456, "free nitro 2"),
        (1_234_567_890_789, "free nitro 3"),
    ];
    let code = "Ab3dE9fGh1_0123456789";
    let (panel, buttons) = verify_panel(SERVER);
    let answer = verify::row(verify::button("Answer", &format!("{}1", verify::ENTER)));
    let captcha = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(verify::new_captcha(&Default::default()).1)
    );
    let challenge = |body: &str, png: bool| {
        let mut e = json!(verify::challenge_embed(body, png, png, 120_000));
        if png {
            e["image"]["url"] = json!(captcha);
        }
        json!({"embeds": [e], "components": [answer], "flags": 64})
    };
    let reply_text = |t: &str| {
        interaction(verify::ephemeral(
            InteractionResponseType::ChannelMessageWithSource,
            Some(t),
        ))
    };
    let mut all = json!({
        "names": {"users": {MEMBER.to_string(): "member", YOU.to_string(): "you", ROGUE.to_string(): "rogue-admin"}, "channels": {GENERAL.to_string(): "general"}},
        "ban_result": json!({"embeds": reply::result_embeds("ban", &[full], Some("toxic"), YOU)}),
        "ban_compact": {"content": reply::summary("ban", true, &[compact], Some("toxic"), YOU)},
        "automod_warn": log(automod::filter_log("member", FilterAction::Warn, "Use of blacklisted words", (GENERAL, "you all suck"), MEMBER)),
        "automod_kick": log(automod::filter_log("member", FilterAction::Kick, "Posting an invite link", (GENERAL, "Join my server everyone discord.gg/example"), MEMBER)),
        "automod_timeout": log(automod::timeout_log("member", "Spamming the same message", (MEMBER, GENERAL), act.timeout_ms, act.strike)),
        "mod_log": log(commands::mod_log_embed((4151, "ban"), (YOU, MEMBER), None, Some("DM ads"))),
        "nuke_quarantined": nuke(true, true),
        "nuke_no_quarantine": nuke(false, false),
        "raid": log(joingate::raid_trigger_log(&raid, &raiders, code, crate::invites::raid_url(code))),
        "raid_report": {
            "embeds": [joingate::raid_report_log(raiders.len() as i64, Some(code)).time().build()],
            "components": [{"type": 1, "components": [{"type": 2, "style": 5, "label": joingate::RAID_REPORT, "url": crate::invites::raid_url(code)}]}],
        },
        "verify_panel": {"embeds": [panel], "components": [buttons]},
        "verify_button": interaction(verify::embed_reply(verify::passed_embed(SERVER))),
        "verify_web": interaction(verify::web_link(Id::new(1))),
        "verify_captcha": challenge(verify::PROMPT, true),
        "verify_simple": challenge(&verify::new_sum().0, false),
        "verify_dm": reply_text("Check your DMs for the challenge."),
    });
    for (k, v) in tickets() {
        all[k] = v;
    }
    all
}

/// Saves the samples, and the command list under `commands`, for the dashboard; run at startup,
/// once the custom emojis are loaded.
pub async fn publish(pool: &PgPool) -> Result<()> {
    let mut m = messages();
    m["commands"] = crate::general::catalog();
    sqlx::query(
        "INSERT INTO bot_previews (id, messages) VALUES (true, $1)
         ON CONFLICT (id) DO UPDATE SET messages = excluded.messages, updated_at = now()",
    )
    .bind(m)
    .execute(pool)
    .await?;
    Ok(())
}

#[test]
fn every_sample_has_a_message() {
    let m = messages();
    let o = m.as_object().unwrap();
    assert!(o.len() > 10);
    for (k, v) in o.iter().filter(|(k, _)| *k != "names") {
        assert!(
            v["embeds"].is_array() || v["content"].is_string(),
            "{k} is empty"
        );
    }
}
