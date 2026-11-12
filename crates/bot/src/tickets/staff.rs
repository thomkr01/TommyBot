use super::*;

/// Relays a staff message to the member: a reply, a snippet or a plain message in the ticket.
#[allow(clippy::too_many_arguments)]
pub(super) async fn relay(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
    text: &str,
    anon: bool,
    snippet: bool,
) -> Result<()> {
    let ch = m.channel_id;
    let cfg = app.cfg(gid.get()).await?;
    if cfg.tickets.blocked.contains(&user) {
        let text = "That user is blacklisted from sending a message here. You need to whitelist them before you can send them a message.";
        return say(app, ch, error(text)).await;
    }
    let Some(member) = http_err::member(&app.http, gid, user).await? else {
        let text = format!(
            "The user was not found. Use `{}close [reason]` to close this channel.",
            cfg.misc.prefix
        );
        return say(app, ch, error(&text)).await;
    };
    let Some(s) = server(app, gid).await? else {
        return Ok(());
    };
    let text = if snippet {
        tags(text, &member.user)
    } else {
        text.to_string()
    };
    let staff = &m.author;
    let (to_member, copy) = from_staff(&s, staff, &member.user, &text, anon);
    let files = download(app, &m.attachments).await;
    let dm = app
        .http
        .create_private_channel(user)
        .await?
        .model()
        .await?
        .id;
    let posted = match app
        .http
        .create_message(dm)
        .embeds(&[to_member.build()])
        .attachments(&files)
        .await
    {
        Ok(r) => r.model().await?,
        Err(_) => {
            let text =
                "The message could not be sent. The user might have disabled Direct Messages.";
            return say(app, ch, error(text)).await;
        }
    };
    let copy = attachment_fields(copy, &posted.attachments);
    app.http
        .create_message(ch)
        .embeds(&[copy.build()])
        .attachments(&files)
        .await?;
    best_effort(
        app.http.delete_message(ch, m.id).await,
        "relayed ticket message",
    );
    remember(app, user, gid).await
}

/// Who wrote a line of the history, from the relay embeds the bot posts in the ticket.
fn speaker(title: &str, author: Option<&str>, footer: Option<&str>) -> Option<String> {
    // Titles start with an emoji, so match the words at the end.
    match title {
        t if t.ends_with("Message Received") => Some(format!(
            "{} (User)",
            footer
                .and_then(|f| f.rsplit_once(" | "))
                .map_or("Unknown", |x| x.0)
        )),
        t if t.ends_with("Message Sent") => Some(format!(
            "{} (Staff)",
            author.map_or("Unknown", |a| a.strip_suffix(" (Anonymous)").unwrap_or(a))
        )),
        _ => None,
    }
}

/// The conversation as text, oldest first: member and staff messages plus staff comments.
async fn history(app: &App, ch: Id<ChannelMarker>) -> Result<String> {
    let mut all: Vec<Message> = vec![];
    for _ in 0..HISTORY_PAGES {
        let req = app.http.channel_messages(ch);
        let page = match all.last() {
            Some(m) => req.before(m.id).limit(100).await?,
            None => req.limit(100).await?,
        }
        .models()
        .await?;
        let done = page.len() < 100;
        all.extend(page);
        if done {
            break;
        }
    }
    let mut lines = vec![];
    for m in all.iter().rev() {
        let at: String = m
            .timestamp
            .iso_8601()
            .to_string()
            .replace('T', " ")
            .chars()
            .take(19)
            .collect();
        let (who, mut text) = if m.author.bot {
            let Some(e) = m.embeds.first().filter(|_| m.author.id == app.me) else {
                continue;
            };
            let Some(who) = speaker(
                e.title.as_deref().unwrap_or(""),
                e.author.as_ref().map(|a| a.name.as_str()),
                e.footer.as_ref().map(|f| f.text.as_str()),
            ) else {
                continue;
            };
            let mut text = e.description.clone().unwrap_or_default();
            for f in e
                .fields
                .iter()
                .filter(|f| f.name.starts_with("Attachment "))
            {
                text = format!("{text} (Attachment: {})", f.value)
                    .trim_start()
                    .to_string();
            }
            (who, text)
        } else {
            (format!("{} (Comment)", m.author.name), m.content.clone())
        };
        for a in &m.attachments {
            text = format!("{text} (Attachment: {})", a.url)
                .trim_start()
                .to_string();
        }
        if text.is_empty() {
            continue;
        }
        lines.push(format!("[{at}] {who}: {text}"));
    }
    Ok(lines.join("\n"))
}

/// Closes a ticket: the log (with the conversation) first, since the channel holds the only copy,
/// then the channel, then the member hears about it.
pub(super) async fn close(
    app: &App,
    gid: Id<GuildMarker>,
    ch: Id<ChannelMarker>,
    user: Id<UserMarker>,
    staff: &User,
    reason: &str,
    anon: bool,
) -> Result<()> {
    say(app, ch, info("Closing ticket...")).await?;
    let claimed = sqlx::query(
        "UPDATE tickets SET closed_at = now(), closed_by = $2 WHERE channel_id = $1 AND closed_at IS NULL",
    )
    .bind(ch.get() as i64)
    .bind(staff.id.get() as i64)
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Ok(());
    }
    let reopen = || async {
        sqlx::query("UPDATE tickets SET closed_at = NULL, closed_by = NULL WHERE channel_id = $1")
            .bind(ch.get() as i64)
            .execute(&app.pool)
            .await
    };
    let cfg = app.cfg(gid.get()).await?;
    let t = &cfg.tickets;
    let member = http_err::member(&app.http, gid, user).await?;
    let reason = if reason.is_empty() {
        "No reason was provided."
    } else {
        reason
    };
    if let Some(log) = t.log_channel {
        if let Err(e) = close_log(
            app,
            gid,
            ch,
            log,
            user,
            member.as_ref().map(|m| &m.user),
            staff,
            reason,
            anon,
            t.full_log,
        )
        .await
        {
            reopen().await?;
            let text = "Could not save the ticket log, so the ticket stays open. Check that the bot can send files in the ticket log.";
            say(app, ch, error(text)).await?;
            return Err(e);
        }
    }
    if let Err(e) = app.http.delete_channel(ch).await {
        tracing::warn!(?e, %ch, "deleting a ticket failed");
        reopen().await?;
        return say(
            app,
            ch,
            error("Missing permissions to delete this channel."),
        )
        .await;
    }
    let Some(member) = member else {
        return Ok(());
    };
    let Some(s) = server(app, gid).await? else {
        return Ok(());
    };
    let dm = app
        .http
        .create_private_channel(user)
        .await?
        .model()
        .await?
        .id;
    if !t.closing.is_empty() {
        let e = notice(&s, true, &t.closing, &member.user);
        best_effort(
            app.http.create_message(dm).embeds(&[e]).await,
            "ticket closing message",
        );
    }
    let e = closed(&s, reason, (!anon).then_some(staff));
    best_effort(
        app.http.create_message(dm).embeds(&[e]).await,
        "ticket closed dm",
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn close_log(
    app: &App,
    gid: Id<GuildMarker>,
    ch: Id<ChannelMarker>,
    log: Id<ChannelMarker>,
    user: Id<UserMarker>,
    member: Option<&User>,
    staff: &User,
    reason: &str,
    anon: bool,
    full: bool,
) -> Result<()> {
    let shown = if anon {
        format!("{} (Anonymous)", staff.name)
    } else {
        staff.name.clone()
    };
    let who = member.map_or("Unknown | 000000000000000000".to_string(), |u| {
        format!("{} | {}", u.name, u.id)
    });
    let e = Log::new("Ticket Closed", RED)
        .author(staff, &shown)
        .text(reason)
        .user("User", user.get())
        .footer(&who)
        .time()
        .build();
    let file = if full {
        let text = history(app, ch).await?;
        Some(Attachment::from_bytes(
            format!("ticket_log_{user}.txt"),
            text.into_bytes(),
            1,
        ))
    } else {
        None
    };
    let (posted, file) = crate::events::send_private(app, log, &e, file).await?;
    if let (Some(file), Some(base)) = (file, crate::general::dashboard()) {
        let url = format!(
            "{}/#/{gid}/tickets/log/{log}/{file}",
            base.trim_end_matches('/')
        );
        let link = Component::Button(Button {
            id: None,
            custom_id: None,
            disabled: false,
            emoji: None,
            label: Some("Message Logs".into()),
            style: ButtonStyle::Link,
            url: Some(url),
            sku_id: None,
        });
        best_effort(
            app.http
                .update_message(log, posted)
                .components(Some(&[action_row(vec![link])]))
                .await,
            "ticket log link",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_names_the_speaker_of_relay_embeds() {
        assert_eq!(
            speaker(
                "<:modmail_reply:1> Message Received",
                None,
                Some("ann | 12")
            )
            .as_deref(),
            Some("ann (User)")
        );
        assert_eq!(
            speaker("💬 Message Sent", Some("bob (Anonymous)"), None).as_deref(),
            Some("bob (Staff)")
        );
        assert_eq!(speaker("New Ticket", None, None), None);
    }
}
