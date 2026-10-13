//! Reply-shaping helpers driven by `MiscCfg`: compact text, DMs, delayed deletes, message purges.
use anyhow::{Context, Result};
use serde_json::{json, Value};
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
        s += &format!("\nModerator: <@{actor}>");
        if let Some(r) = reason {
            s += &format!("\nReason: {r}");
        }
    }
    s
}

/// What the target is told; `actor` is None when staff identity is hidden.
pub fn dm_text(kind: &str, reason: Option<&str>, actor: Option<u64>) -> String {
    let mut s = format!("You received a {kind} in a server");
    if let Some(a) = actor {
        s += &format!(" from <@{a}>");
    }
    s + &format!(". Reason: {}", reason.unwrap_or("none given"))
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
// shortcut: only the triggering channel, the bot keeps no per-user channel history.
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
    match ids.len() {
        0 => {}
        1 => {
            app.http.delete_message(channel, ids[0]).await?;
        }
        _ => {
            app.http.delete_messages(channel, &ids).await?;
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
