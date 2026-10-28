//! Reply-shaping helpers driven by `MiscCfg`: compact text, DMs, delayed deletes, message purges.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use tommybot_core::jobs;
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker},
    Id,
};

use crate::app::{now_secs, App};

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

/// One reply for all targets. Compact mode turns several targets into Wick's count lines; failures stay listed.
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
            "✅ Successful {kind}s: `{}`\n❎ Unsuccessful {kind}s: `{}`",
            ok.len(),
            failed.len()
        );
        if let Some(r) = reason {
            s += format!("\n📄 Reason: {r}").as_str();
        }
        s += format!("\n👤 Moderator: <@{actor}>").as_str();
        failed.iter().fold(s, |s, f| s + "\n" + f.as_str())
    } else {
        texts.join("\n")
    };
    s.truncate(s.floor_char_boundary(2000));
    s
}

/// Wick's "Add Reason" pointer for actions run without a reason.
pub fn add_reason_hint(case_no: i64) -> String {
    format!("\nAdd Reason: `/cases modify case:{case_no} reason:`")
}

/// What the target is told; `actor` is None when staff identity is hidden.
pub fn dm_text(kind: &str, reason: Option<&str>, actor: Option<u64>) -> String {
    let mut s = format!("You received a {kind} in a server");
    if let Some(a) = actor {
        s += format!(" from <@{a}>").as_str();
    }
    s + format!(". Reason: {}", reason.unwrap_or("none given")).as_str()
}

/// Best effort: closed DMs are normal, so failures are ignored.
pub async fn dm(app: &App, user: Id<UserMarker>, text: &str) {
    let Ok(ch) = app.http.create_private_channel(user).await else {
        return;
    };
    let Ok(ch) = ch.model().await else { return };
    app.http.create_message(ch.id).content(text).await.ok();
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

/// Scheduler arm for `delete_message`; an already deleted message is fine.
pub async fn run_delete(app: &App, payload: &Value) -> Result<()> {
    let id = |k: &str| payload[k].as_u64().with_context(|| format!("no {k}"));
    app.http
        .delete_message(Id::new(id("channel")?), Id::new(id("message")?))
        .await
        .ok();
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
    let cutoff_ms = (now_secs() - 14 * 86_400 + 60) * 1000;
    let mut by_channel: HashMap<u64, Vec<Id<MessageMarker>>> = HashMap::new();
    for &(c, m) in msgs {
        // Snowflake timestamp: ms since the Discord epoch in the top bits.
        if ((m >> 22) + 1_420_070_400_000) as i64 > cutoff_ms {
            by_channel.entry(c).or_default().push(Id::new(m));
        }
    }
    for (c, ids) in by_channel {
        delete_batch(app, Id::new(c), &ids).await.ok();
    }
}

async fn delete_batch(
    app: &App,
    channel: Id<ChannelMarker>,
    ids: &[Id<MessageMarker>],
) -> Result<()> {
    match ids {
        [] => {}
        [one] => {
            app.http.delete_message(channel, *one).await?;
        }
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
            "✅ Successful kicks: `1`\n❎ Unsuccessful kicks: `1`\n📄 Reason: spam\n👤 Moderator: <@9>\nCannot kick <@6>: they are immune"
        );
        assert_eq!(summary("kick", false, &t, None, 9), t.join("\n"));
        assert_eq!(summary("kick", true, &t[..1], None, 9), t[0]);
    }

    #[test]
    fn dm_hides_staff() {
        assert_eq!(
            dm_text("ban", None, None),
            "You received a ban in a server. Reason: none given"
        );
        assert!(dm_text("kick", Some("r"), Some(9)).contains("from <@9>"));
    }
}
