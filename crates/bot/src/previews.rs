//! The sample messages the dashboard shows in its wizard previews, built with the same functions
//! the bot sends them with, so a preview can never drift from Discord. `messages.json` is checked
//! here; run `UPDATE_PREVIEWS=1 cargo test -p tommybot previews` to rewrite it after a change.
use serde_json::{json, Value};
use tommybot_automod::{
    join::{Action, RaidCfg},
    strike::{strike, StrikeCfg, StrikeState},
};
use tommybot_logging::{embed::verify_panel, emoji::AS_NAMES, Log};
use twilight_model::{
    channel::message::Embed,
    http::interaction::{InteractionResponse, InteractionResponseType},
    id::Id,
};

use crate::{antinuke, automod, commands, joingate, reply, verify};
use tommybot_core::sections::FilterAction;

const MEMBER: u64 = 1001;
const YOU: u64 = 1002;
const ROGUE: u64 = 1003;
const GENERAL: u64 = 2001;
const SERVER: &str = "Your Server";
const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/previews");

/// A log as `send_log` sends it, with the clock line's time zeroed.
fn log(l: Log) -> Value {
    let mut v = json!({"embeds": [l.time().build()]});
    let d = v["embeds"][0]["description"].as_str().unwrap_or_default();
    let at = d.rfind("<t:").map_or(d.len(), |i| i + 3);
    let end = d[at..].find(':').map_or(at, |e| at + e);
    v["embeds"][0]["description"] = json!(format!("{}0{}", &d[..at], &d[end..]));
    v
}

fn embeds(e: &[Embed]) -> Value {
    json!({ "embeds": e })
}

fn interaction(r: InteractionResponse) -> Value {
    serde_json::to_value(r.data).unwrap()
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
    let challenge = |body: &str, png: bool| json!({"embeds": [verify::challenge_embed(body, png, png, 120_000)], "components": [answer], "flags": 64});
    let reply_text = |t: &str| {
        interaction(verify::ephemeral(
            InteractionResponseType::ChannelMessageWithSource,
            Some(t),
        ))
    };
    json!({
        "names": {"users": {MEMBER.to_string(): "member", YOU.to_string(): "you", ROGUE.to_string(): "rogue-admin"}, "channels": {GENERAL.to_string(): "general"}},
        "ban_result": embeds(&reply::result_embeds("ban", &[full], Some("toxic"), YOU)),
        "ban_compact": {"content": reply::summary("ban", true, &[compact], Some("toxic"), YOU)},
        "automod_warn": log(automod::filter_log("member", FilterAction::Warn, "Use of blacklisted words", (GENERAL, "you all suck"), MEMBER)),
        "automod_kick": log(automod::filter_log("member", FilterAction::Kick, "Posting an invite link", (GENERAL, "Join my server everyone discord.gg/example"), MEMBER)),
        "automod_timeout": log(automod::timeout_log("member", "Spamming the same message", (MEMBER, GENERAL), act.timeout_ms, act.strike)),
        "mod_log": log(commands::mod_log_embed((4151, "ban"), (YOU, MEMBER), None, Some("DM ads"))),
        "nuke_quarantined": nuke(true, true),
        "nuke_no_quarantine": nuke(false, false),
        "raid": log(joingate::raid_trigger_log(&raid, &raiders, code, Some(format!("https://tommybot.example/raid/{code}")))),
        "verify_panel": {"embeds": [panel], "components": [buttons]},
        "verify_button": interaction(verify::embed_reply(verify::passed_embed(SERVER))),
        "verify_web": interaction(verify::web_link_at("https://tommybot.example", Id::new(1))),
        "verify_captcha": challenge(verify::PROMPT, true),
        "verify_simple": challenge(&verify::sum(4, 3).0, false),
        "verify_dm": reply_text("Check your DMs for the challenge."),
    })
}

#[test]
fn previews_match_the_bot() {
    AS_NAMES.with(|n| n.set(true));
    let got = serde_json::to_string_pretty(&messages()).unwrap() + "\n";
    let path = format!("{DIR}/messages.json");
    if std::env::var_os("UPDATE_PREVIEWS").is_some() {
        std::fs::write(&path, &got).unwrap();
        let (_, png) = verify::new_captcha(&Default::default());
        std::fs::write(format!("{DIR}/captcha.png"), png).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        got == want,
        "dashboard previews are stale: run UPDATE_PREVIEWS=1 cargo test -p tommybot previews"
    );
    // Every emoji the previews use must be drawable by the dashboard.
    let icons = include_str!("../../dashboard/ui/src/icons.rs");
    for name in got.split("<:").skip(1).filter_map(|r| r.split(':').next()) {
        assert!(
            icons.contains(&format!("\"{name}\"")),
            "add \"{name}\" to the icons! list in dashboard/ui/src/icons.rs"
        );
    }
}
