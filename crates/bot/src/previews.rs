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

use crate::antinuke::Applied;
use crate::panic::{self, Backup};
use crate::{antinuke, automod, commands, joingate, prefix::Flags, reply, tickets, verify};
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

/// A finished action on [`MEMBER`] with its DM delivered.
fn applied(kind: &str, compact: bool) -> reply::Applied {
    reply::Applied {
        target: Some(MEMBER),
        case: Some(4151),
        text: reply::case_text(compact, (4151, kind, MEMBER), Some("toxic"), YOU),
        dm: reply::DmOutcome::Sent,
    }
}

/// Result embeds for each punishment, once with every target done and once with one refused.
fn results() -> Vec<(String, Value)> {
    let hour = Some(3600);
    let cases: [(&str, &str, Flags, Option<i64>); 7] = [
        ("warn", "warn", Flags::default(), None),
        ("timeout", "timeout", Flags::default(), hour),
        ("kick", "kick", Flags::default(), None),
        ("ban", "ban", Flags::default(), None),
        ("softban", "softban", Flags::default(), None),
        ("tempban", "ban", Flags::default(), Some(7 * 86_400)),
        ("quarantine", "quarantine", Flags::default(), None),
    ];
    let mut out = vec![];
    for (key, kind, flags, dur) in cases {
        let ok = [applied(kind, false)];
        let refused = reply::Applied {
            target: Some(ROGUE),
            text: format!("Cannot {kind} <@{ROGUE}>: they are immune"),
            ..Default::default()
        };
        let one_failed = [applied(kind, false), refused];
        let embeds = |done: &[reply::Applied]| json!({"embeds": reply::result_embeds(kind, done, (flags, dur), Some("toxic"), YOU)});
        out.push((format!("{key}_result_all"), embeds(&ok)));
        out.push((format!("{key}_result_failed"), embeds(&one_failed)));
    }
    out
}

/// The DM a member gets for each punishment and each removal.
fn dms() -> Vec<(String, Value)> {
    let kinds = [
        ("warn", None),
        ("timeout", Some(3600)),
        ("quarantine", None),
        ("kick", None),
        ("ban", None),
        ("softban", None),
        ("tempban", Some(7 * 86_400)),
        ("unquarantine", None),
        ("untimeout", None),
        ("unban", None),
    ];
    kinds
        .into_iter()
        .map(|(k, d)| {
            let e = reply::dm_embed(k, SERVER, Some("toxic"), Some("you"), d);
            (format!("dm_{k}"), embed(e))
        })
        .collect()
}

fn messages() -> Value {
    let full = applied("ban", false);
    let compact = applied("ban", true);
    let act = strike(&mut StrikeState::default(), &StrikeCfg::default(), 0);
    let sum = antinuke::RevertSummary {
        ops: 5,
        done: vec![],
        failed: 0,
        reinvite: vec![],
        recreated: 5,
    };
    let nuke = |applied, bot_role| {
        let why = format!(
            "{:?} limit reached (5/min, 20/hour)",
            tommybot_antinuke::Action::ChannelDelete
        );
        log(antinuke::report_log(
            (4152, "n7Kd2x"),
            ROGUE,
            antinuke::Contained { applied, bot_role },
            &why,
            &sum,
        ))
    };
    let case = (
        "9g5udu".to_string(),
        json!({"quarantined": [ROGUE], "failed": [], "fragments": {ROGUE.to_string(): 8}}),
    );
    let gone = crate::backup::Counts {
        created: 4,
        deleted: 0,
        updated: 1,
    };
    let restored = crate::backup::Summary {
        channels: gone,
        ..Default::default()
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
    let help_cfg = verify::VerificationCfg {
        roles: vec![Id::new(3001)],
        verify: tommybot_automod::join::VerifyCfg {
            on_fail: tommybot_automod::join::FailAction::Kick,
            ..Default::default()
        },
        ..Default::default()
    };
    let gate_bot = joingate::Detail {
        filters: &["No Avatar", "Suspicious Account"],
        is_bot: true,
        adder: Some(YOU),
    };
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
        "names": {"users": {MEMBER.to_string(): "member", YOU.to_string(): "you", ROGUE.to_string(): "rogue-admin"}, "channels": {GENERAL.to_string(): "general"}, "roles": {"3001": "Member"}},
        "ban_result": json!({"embeds": reply::result_embeds("ban", &[full], (Flags::default(), None), Some("toxic"), YOU)}),
        "ban_compact": {"content": reply::summary("ban", true, &[compact], Some("toxic"), YOU)},
        "automod_warn": log(automod::filter_log("member", FilterAction::Warn, "Use of blacklisted words", (GENERAL, "you all suck"), MEMBER)),
        "automod_kick": log(automod::filter_log("member", FilterAction::Kick, "Posting an invite link", (GENERAL, "Join my server everyone discord.gg/example"), MEMBER)),
        "automod_timeout": log(automod::timeout_log("member", "Spamming the same message", (MEMBER, GENERAL), act.timeout_ms, act.strike)),
        "mod_log": log(commands::mod_log_embed((4151, "ban"), (YOU, MEMBER), None, Some("DM ads"))),
        "nuke_quarantined": nuke(Applied::Quarantined, None),
        "nuke_no_quarantine": nuke(Applied::Off, None),
        "nuke_bot_cleared": nuke(Applied::Quarantined, Some(true)),
        "nuke_bot_kicked": nuke(Applied::Kicked, Some(false)),
        "panic_report_disabled": log(panic::report(Some(&case), &Backup::Disabled)),
        "panic_report_none": log(panic::report(Some(&case), &Backup::None)),
        "panic_report_loaded": log(panic::report(Some(&case), &Backup::Loaded(restored))),
        "panic_report_failed": log(panic::report(Some(&case), &Backup::Failed("Missing Permissions".into()))),
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
        "note_modified": log(crate::modview::note_log(&tommybot_core::notes::NoteLog {
            action: tommybot_core::notes::NoteAction::Modify,
            id: Some("1d300v".into()),
            user: Some(MEMBER),
            by: YOU,
            text: Some("Asked staff for the raid list twice".into()),
            old: Some("Asked staff for the raid list".into()),
            ..Default::default()
        })),
        "verify_help": interaction(verify::embed_reply(verify::help_embed(&help_cfg))),
        "verify_deleted": log(verify::deleted::deleted_log("The verification channel #verify-here was deleted by <@1003>. TommyBot made a new one: <#2001>. <@1003> is trusted, so only this message was sent. If they delete it again, they will be quarantined.")),
        "joingate_kick": log(joingate::gate_log(ROGUE, Action::Kick, "join gate", gate_bot, None)),
    });
    for (k, v) in tickets() {
        all[k] = v;
    }
    for (k, v) in results().into_iter().chain(dms()) {
        all[k] = v;
    }
    let question = crate::confirm::sample_question(MEMBER);
    all["confirm_question"] = json!({"embeds": [question.0], "components": [question.1]});
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
