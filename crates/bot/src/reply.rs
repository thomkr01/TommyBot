//! Reply-shaping helpers driven by `MiscCfg`: compact text, DMs, delayed deletes, message purges.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use tommybot_core::jobs;
use tommybot_logging::embed::{Log, ARROW, BLUE, GREEN, NO, YES};
use tommybot_logging::emoji::{MODERATOR, PANIC, REASON};
use tommybot_logging::LogKind;
use twilight_model::channel::message::Embed;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
    Id,
};

use crate::app::{now_secs, App};
use crate::http_err::{best_effort, gone};

/// Compact replies are one line; the full form adds moderator and reason.
pub fn case_text(
    compact: bool,
    (case_no, kind, target): (i64, &str, u64),
    reason: Option<&str>,
    actor: u64,
) -> String {
    let mut s = format!("Case #{case_no}: {kind} <@{target}>");
    if !compact {
        s += format!("\nModerator: <@{actor}>").as_str();
        if let Some(r) = reason {
            s += format!("\nReason: {r}").as_str();
        }
    }
    if reason.is_none() {
        s += add_reason_hint(case_no).as_str();
    }
    s
}

/// One reply for all targets. Compact mode turns several targets into count lines; failures stay listed.
pub fn summary(
    kind: &str,
    compact: bool,
    texts: &[String],
    reason: Option<&str>,
    actor: u64,
) -> String {
    let mut s = if compact && texts.len() > 1 {
        let (ok, failed): (Vec<_>, Vec<_>) = texts.iter().partition(|t| t.starts_with("Case #"));
        let mut s = format!(
            "{YES} Successful {kind}s: `{}`\n{NO} Unsuccessful {kind}s: `{}`",
            ok.len(),
            failed.len()
        );
        if let Some(r) = reason {
            s += format!("\n{REASON} Reason: {r}").as_str();
        }
        s += format!("\n{MODERATOR} Moderator: <@{actor}>").as_str();
        failed.iter().fold(s, |s, f| s + "\n" + f.as_str())
    } else {
        texts.join("\n")
    };
    s.truncate(s.floor_char_boundary(2000));
    s
}

/// "Add Reason" pointer for actions run without a reason.
pub fn add_reason_hint(case_no: i64) -> String {
    format!("\nAdd Reason: `/cases modify case:{case_no} reason:`")
}

/// Two result embeds: "Timeout Result:" and its kin, and a pointer to add a reason when none was given.
pub fn result_embeds(kind: &str, texts: &[String], reason: Option<&str>, actor: u64) -> Vec<Embed> {
    let (title, done) = match kind {
        "ban" => ("Ban Result:", "Banned:"),
        "kick" => ("Kick Result:", "Kicked:"),
        "warn" => ("Warn Result:", "Warned:"),
        "unban" => ("Unban Result:", "Unbanned:"),
        "untimeout" => ("Untimeout Result:", "Timeout removed:"),
        "quarantine" => ("Quarantine Result:", "Quarantined:"),
        "unquarantine" => ("Unquarantine Result:", "Released:"),
        "verify" => ("Verify Result:", "Verified:"),
        "sanitize" => ("Sanitize Result:", "Sanitized:"),
        "purge" => ("Purge Result:", "Purged:"),
        "slowmode" => ("Slowmode Result:", "Slowmode:"),
        _ => ("Timeout Result:", "Timed out:"),
    };
    let mut result = Log::new(title, BLUE)
        .text(&format!(
            "{REASON} **Reason:** {}",
            reason.unwrap_or("No reason.")
        ))
        .text(&format!("{MODERATOR} **Moderator:** <@{actor}>"))
        .text(&format!("**{done}**"));
    let mut cases = vec![];
    for t in texts {
        let first = t.lines().next().unwrap_or_default();
        let case = first
            .strip_prefix("Case #")
            .and_then(|r| r.split(':').next());
        let Some(n) = case else {
            result = result.text(&format!("{NO} {first}"));
            continue;
        };
        cases.push(n);
        let mention = first.find("<@").and_then(|s| {
            let e = first[s..].find('>')?;
            Some(&first[s..=s + e])
        });
        result = match mention {
            Some(who) => {
                let id = who.trim_matches(|c: char| !c.is_ascii_digit());
                result.text(&format!("{YES} {who} [`{id}`]"))
            }
            // Purge and slowmode have no member: show what happened.
            None => result.text(&format!(
                "{YES} {}",
                first.split_once(": ").map_or(first, |c| c.1)
            )),
        };
        // Warn threshold notes; the embed already shows moderator and reason.
        for extra in t.lines().skip(1).filter(|l| {
            !["Moderator:", "Reason:", "Add Reason"]
                .iter()
                .any(|p| l.starts_with(p))
        }) {
            result = result.text(extra);
        }
    }
    let mut out = vec![result.build()];
    if reason.is_none() {
        let mut add = Log::new("Add Reason", GREEN);
        for n in cases {
            add = add.text(&format!("{ARROW} `/cases modify (Case: {n})`"));
        }
        out.push(add.build());
    }
    out
}

/// What the target is told; `actor` is None when staff identity is hidden.
pub fn dm_text(kind: &str, reason: Option<&str>, actor: Option<u64>) -> String {
    let mut s = format!("You received a {kind} in a server");
    if let Some(a) = actor {
        s += format!(" from <@{a}>").as_str();
    }
    s + format!(". Reason: {}", reason.unwrap_or("none given")).as_str()
}

/// Best effort: closed DMs are normal, other failures are logged.
pub async fn dm(app: &App, user: Id<UserMarker>, text: &str) {
    let Some(ch) = best_effort(app.http.create_private_channel(user).await, "open DM") else {
        return;
    };
    let Ok(ch) = ch.model().await else { return };
    best_effort(
        app.http.create_message(ch.id).content(text).await,
        "send DM",
    );
}

/// DMs the server owner that an attack started, so they hear about it even when nobody watches
/// the log channel. At most one DM per server per 10 minutes: a nuke that trips panic mode is one alert.
pub async fn alert_owner(app: &App, gid: Id<GuildMarker>, what: &str) {
    static LAST: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());
    if !alert_due(&mut LAST.lock().unwrap(), gid.get(), now_secs(), 600) {
        return;
    }
    let info = match app.guild_info(gid).await {
        Ok(i) => i,
        Err(e) => return tracing::warn!(?e, "owner alert: guild lookup failed"),
    };
    let logs = app
        .cfg(gid.get())
        .await
        .ok()
        .and_then(|c| c.logging.target(LogKind::Panic, None))
        .map_or(String::new(), |ch| {
            format!("\nDetails: https://discord.com/channels/{gid}/{ch}")
        });
    let text = format!("{PANIC} **{what}** on your server. TommyBot is handling it.{logs}");
    dm(app, info.owner, &text).await;
}

/// DMs the server owner right away, without [`alert_owner`]'s 10-minute limit.
pub async fn dm_owner(app: &App, gid: Id<GuildMarker>, text: &str) {
    match app.guild_info(gid).await {
        Ok(i) => dm(app, i.owner, text).await,
        Err(e) => tracing::warn!(?e, "owner DM: guild lookup failed"),
    }
}

/// Whether `key` (a guild or channel) is due another alert, at most one per `every` seconds.
pub(crate) fn alert_due(last: &mut BTreeMap<u64, i64>, key: u64, now: i64, every: i64) -> bool {
    if last.get(&key).is_some_and(|t| now - t < every) {
        return false;
    }
    last.insert(key, now);
    true
}

/// Queued as a job so it survives restarts and never blocks the handler.
pub async fn delete_later(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Id<ChannelMarker>,
    msg: Id<MessageMarker>,
    secs: u32,
) -> Result<()> {
    let job = json!({"channel": channel.get(), "message": msg.get()});
    let at = now_secs() + i64::from(secs);
    jobs::enqueue(&app.pool, gid.get() as i64, "delete_message", job, at).await?;
    Ok(())
}

/// Scheduler arm for `delete_message`; the scheduler drops the job when the message is already gone.
pub async fn run_delete(app: &App, payload: &Value) -> Result<()> {
    let id = |k: &str| payload[k].as_u64().with_context(|| format!("no {k}"));
    app.http
        .delete_message(Id::new(id("channel")?), Id::new(id("message")?))
        .await?;
    Ok(())
}

/// Bulk delete `user`'s messages among the channel's last 100. Discord refuses messages older than 14 days.
pub async fn purge_user(app: &App, channel: Id<ChannelMarker>, user: Id<UserMarker>) -> Result<()> {
    let msgs = app.http.channel_messages(channel).limit(100).await?;
    let cutoff = now_secs() - 14 * 86_400 + 60;
    let ids: Vec<_> = msgs
        .models()
        .await?
        .into_iter()
        .filter(|m| m.author.id == user && m.timestamp.as_secs() > cutoff)
        .map(|m| m.id)
        .collect();
    delete_batch(app, channel, &ids).await
}

/// Deletes tracked (channel, message) pairs, one bulk delete per channel; too old ones are skipped.
pub async fn purge_ids(app: &App, msgs: &[(u64, u64)]) {
    for (c, ids) in batches(msgs, now_secs()) {
        if let Err(e) = delete_batch(app, Id::new(c), &ids).await {
            tracing::warn!(?e, channel = c, "purging tracked messages failed");
        }
    }
}

/// Groups messages younger than 14 days by channel, at most 100 per group (Discord's bulk delete cap).
fn batches(msgs: &[(u64, u64)], now: i64) -> Vec<(u64, Vec<Id<MessageMarker>>)> {
    let cutoff_ms = (now - 14 * 86_400 + 60) * 1000;
    let mut by_channel: HashMap<u64, Vec<Id<MessageMarker>>> = HashMap::new();
    for &(c, m) in msgs {
        // Snowflake timestamp: ms since the Discord epoch in the top bits.
        if ((m >> 22) + 1_420_070_400_000) as i64 > cutoff_ms {
            by_channel.entry(c).or_default().push(Id::new(m));
        }
    }
    by_channel
        .into_iter()
        .flat_map(|(c, ids)| {
            ids.chunks(100)
                .map(|ch| (c, ch.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn delete_batch(
    app: &App,
    channel: Id<ChannelMarker>,
    ids: &[Id<MessageMarker>],
) -> Result<()> {
    match ids {
        [] => {}
        // Already deleted (by the author or another moderator) is fine.
        [one] => match app.http.delete_message(channel, *one).await {
            Err(e) if !gone(&e) => return Err(e.into()),
            _ => {}
        },
        _ => {
            app.http.delete_messages(channel, ids).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_text_forms() {
        assert_eq!(
            case_text(true, (3, "ban", 5), Some("x"), 9),
            "Case #3: ban <@5>"
        );
        assert_eq!(
            case_text(false, (3, "ban", 5), Some("x"), 9),
            "Case #3: ban <@5>\nModerator: <@9>\nReason: x"
        );
        assert!(case_text(true, (3, "ban", 5), None, 9)
            .ends_with("\nAdd Reason: `/cases modify case:3 reason:`"));
    }

    #[test]
    fn compact_summary_counts() {
        let t = [
            "Case #1: kick <@5>".to_string(),
            "Cannot kick <@6>: they are immune".into(),
        ];
        assert_eq!(
            summary("kick", true, &t, Some("spam"), 9),
            "✅ Successful kicks: `1`\n❌ Unsuccessful kicks: `1`\n📄 Reason: spam\n👤 Moderator: <@9>\nCannot kick <@6>: they are immune"
        );
        assert_eq!(summary("kick", false, &t, None, 9), t.join("\n"));
        assert_eq!(summary("kick", true, &t[..1], None, 9), t[0]);
    }

    #[test]
    fn timeout_embeds_list_results_and_add_reason() {
        let t = [
            "Case #4: timeout <@5>".to_string(),
            "Cannot timeout <@6>: they are immune".into(),
        ];
        let e = result_embeds("timeout", &t, None, 9);
        assert_eq!(e.len(), 2);
        let d = e[0].description.clone().unwrap();
        assert!(d.contains("No reason.") && d.contains("✅ <@5> [`5`]") && d.contains("❌ Cannot"));
        assert!(e[1].description.clone().unwrap().contains("(Case: 4)"));
        assert_eq!(result_embeds("timeout", &t, Some("x"), 9).len(), 1);
    }

    #[test]
    fn other_commands_get_result_embeds() {
        let t = ["Case #5: Deleted 7 messages".to_string()];
        let e = result_embeds("purge", &t, Some("x"), 9);
        assert_eq!(e.len(), 1);
        let d = e[0].description.clone().unwrap();
        assert!(d.contains("Purged:") && d.contains("✅ Deleted 7 messages"));
        let t = ["Case #6: Renamed <@5> to `bob`".to_string()];
        let d = result_embeds("sanitize", &t, Some("x"), 9)[0]
            .description
            .clone()
            .unwrap();
        assert!(d.contains("✅ <@5> [`5`]"));
    }

    #[test]
    fn batches_cap_at_100_and_skip_old() {
        // A snowflake made "now" in ms since the Discord epoch.
        let now = 1_800_000_000;
        let fresh = ((now as u64 * 1000) - 1_420_070_400_000) << 22;
        let old = ((now as u64 - 15 * 86_400) * 1000 - 1_420_070_400_000) << 22;
        let mut msgs: Vec<(u64, u64)> = (0..150).map(|i| (1, fresh + i)).collect();
        msgs.push((1, old));
        msgs.push((2, fresh));
        let mut b = batches(&msgs, now);
        b.sort_by_key(|(c, ids)| (*c, std::cmp::Reverse(ids.len())));
        let sizes: Vec<_> = b.iter().map(|(c, ids)| (*c, ids.len())).collect();
        assert_eq!(sizes, [(1, 100), (1, 50), (2, 1)]);
    }

    #[test]
    fn dm_hides_staff() {
        assert_eq!(
            dm_text("ban", None, None),
            "You received a ban in a server. Reason: none given"
        );
        assert!(dm_text("kick", Some("r"), Some(9)).contains("from <@9>"));
    }

    #[test]
    fn owner_alert_waits_ten_minutes_per_server() {
        let mut last = BTreeMap::new();
        assert!(alert_due(&mut last, 1, 1000, 600));
        assert!(!alert_due(&mut last, 1, 1599, 600));
        assert!(alert_due(&mut last, 2, 1599, 600));
        assert!(alert_due(&mut last, 1, 1600, 600));
    }
}
