//! Tickets: the panel's button opens a private channel in the ticket category for the member and
//! the staff whose permit has Manage Tickets. Closing saves a transcript to the ticket log.
use anyhow::Result;
use tommybot_logging::{embed::BLUE, Log};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::{
        message::{AllowedMentions, Message},
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
        ChannelType,
    },
    guild::Permissions,
    http::{attachment::Attachment, interaction::InteractionResponseType},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use crate::{
    app::App,
    verify::{button, ephemeral, row},
};

const OPEN: &str = "ticket:open";
const CLOSE: &str = "ticket:close";
/// Transcripts keep the last 500 messages (5 pages).
const PAGES: usize = 5;

fn member_perms() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ATTACH_FILES
        | Permissions::EMBED_LINKS
}

fn allow(id: u64, kind: PermissionOverwriteType) -> PermissionOverwrite {
    PermissionOverwrite {
        allow: member_perms(),
        deny: Permissions::empty(),
        id: Id::new(id),
        kind,
    }
}

/// Hidden from @everyone (its role id is the guild id); the member, the bot and staff get in.
fn overwrites(
    gid: Id<GuildMarker>,
    user: u64,
    bot: u64,
    staff: &[i64],
) -> Vec<PermissionOverwrite> {
    let mut v = vec![
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
            id: gid.cast(),
            kind: PermissionOverwriteType::Role,
        },
        allow(user, PermissionOverwriteType::Member),
        allow(bot, PermissionOverwriteType::Member),
    ];
    v.extend(
        staff
            .iter()
            .map(|r| allow(*r as u64, PermissionOverwriteType::Role)),
    );
    v
}

/// Roles of the permits that hold Manage Tickets.
async fn staff_roles(app: &App, gid: Id<GuildMarker>) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT r.role_id FROM permit_roles r JOIN permits p ON p.id = r.permit_id
         WHERE p.guild_id = $1 AND 'tickets' = ANY(p.perms)",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?)
}

async fn open(app: &App, gid: Id<GuildMarker>, user: Id<UserMarker>) -> Result<String> {
    let cfg = app.cfg(gid.get()).await?;
    let t = &cfg.tickets;
    let (true, Some(category)) = (t.enabled, t.category) else {
        return Ok("Tickets are not set up on this server.".into());
    };
    let g = gid.get() as i64;
    let mine: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT channel_id FROM tickets WHERE guild_id = $1 AND user_id = $2 AND closed_at IS NULL",
    )
    .bind(g)
    .bind(user.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    if mine.len() >= t.max_open.max(1) as usize {
        let at = mine
            .iter()
            .flatten()
            .map(|c| format!(" <#{c}>"))
            .collect::<String>();
        return Ok(format!("You already have an open ticket:{at}"));
    }
    // shortcut: two members opening at the same instant can collide on the number; the loser just presses again.
    let number: i32 = sqlx::query_scalar(
        "INSERT INTO tickets (guild_id, number, user_id)
         SELECT $1, COALESCE(MAX(number), 0) + 1, $2 FROM tickets WHERE guild_id = $1 RETURNING number",
    )
    .bind(g)
    .bind(user.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    let staff = staff_roles(app, gid).await?;
    let name = format!("ticket-{number:04}");
    let ows = overwrites(gid, user.get(), app.me.get(), &staff);
    let created = async {
        let r = app
            .http
            .create_guild_channel(gid, &name)
            .kind(ChannelType::GuildText)
            .parent_id(category)
            .permission_overwrites(&ows)
            .await?;
        anyhow::Ok(r.model().await?)
    };
    let ch = match created.await {
        Ok(c) => c.id,
        Err(e) => {
            sqlx::query("DELETE FROM tickets WHERE guild_id = $1 AND number = $2")
                .bind(g)
                .bind(number)
                .execute(&app.pool)
                .await?;
            return Err(e);
        }
    };
    sqlx::query("UPDATE tickets SET channel_id = $3 WHERE guild_id = $1 AND number = $2")
        .bind(g)
        .bind(number)
        .bind(ch.get() as i64)
        .execute(&app.pool)
        .await?;
    let pings = AllowedMentions {
        users: vec![user],
        ..Default::default()
    };
    app.http
        .create_message(ch)
        .content(&format!(
            "<@{user}> Thanks for opening a ticket. Tell us what you need and staff will be with you shortly."
        ))
        .components(&[row(button("Close ticket", CLOSE))])
        .allowed_mentions(Some(&pings))
        .await?;
    Ok(format!("Ticket opened: <#{ch}>"))
}

fn transcript_line(m: &Message) -> String {
    let mut l = format!(
        "[{}] {}: {}",
        m.timestamp.iso_8601(),
        m.author.name,
        m.content
    );
    for a in &m.attachments {
        l.push_str(&format!(" [{}]", a.url));
    }
    if !m.embeds.is_empty() {
        l.push_str(&format!(" ({} embed(s))", m.embeds.len()));
    }
    l
}

/// The last 500 messages, oldest first.
async fn transcript(app: &App, ch: Id<ChannelMarker>) -> Result<(usize, String)> {
    let mut all: Vec<Message> = vec![];
    for _ in 0..PAGES {
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
    let text = all
        .iter()
        .rev()
        .map(transcript_line)
        .collect::<Vec<_>>()
        .join("\n");
    Ok((all.len(), text))
}

/// Who may close: the member who opened the ticket, or staff with Manage Tickets.
fn may_close(opener: u64, clicker: u64, staff: bool) -> bool {
    opener == clicker || staff
}

async fn close(
    app: &App,
    i: &Interaction,
    gid: Id<GuildMarker>,
    ch: Id<ChannelMarker>,
    user: Id<UserMarker>,
) -> Result<()> {
    let client = app.http.interaction(Id::new(app.me.get()));
    let say = |t: &str| ephemeral(InteractionResponseType::ChannelMessageWithSource, Some(t));
    let row: Option<(i32, i64)> = sqlx::query_as(
        "SELECT number, user_id FROM tickets WHERE channel_id = $1 AND closed_at IS NULL",
    )
    .bind(ch.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((number, opener)) = row else {
        client
            .create_response(i.id, &i.token, &say("This ticket is already closed."))
            .await?;
        return Ok(());
    };
    let owner = app.guild_info(gid).await?.owner;
    let staff = app.level(gid, owner, user).await?.has("tickets");
    if !may_close(opener as u64, user.get(), staff) {
        let no = say(
            "Only the member who opened it and staff with Manage Tickets can close this ticket.",
        );
        client.create_response(i.id, &i.token, &no).await?;
        return Ok(());
    }
    // Claims the close, so a double click does not log twice.
    let claimed = sqlx::query(
        "UPDATE tickets SET closed_at = now(), closed_by = $2 WHERE channel_id = $1 AND closed_at IS NULL",
    )
    .bind(ch.get() as i64)
    .bind(user.get() as i64)
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Ok(());
    }
    let mut r = say("Closing the ticket...");
    if let Some(d) = r.data.as_mut() {
        d.flags = None;
    }
    client.create_response(i.id, &i.token, &r).await?;
    let cfg = app.cfg(gid.get()).await?;
    if let Some(log) = cfg.tickets.log_channel {
        let (count, text) = transcript(app, ch).await?;
        let embed = Log::new(&format!("Ticket #{number:04} closed"), BLUE)
            .user("Opened by", opener as u64)
            .user("Closed by", user.get())
            .line("Messages", &count.to_string())
            .time()
            .build();
        let file = [Attachment::from_bytes(
            format!("ticket-{number:04}.txt"),
            text.into_bytes(),
            1,
        )];
        app.http
            .create_message(log)
            .embeds(&[embed])
            .attachments(&file)
            .await?;
    }
    app.http.delete_channel(ch).await?;
    Ok(())
}

/// Handles the panel's open button and a ticket's close button. Returns false for other interactions.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let (Some(gid), Some(user), Some(ch)) =
        (i.guild_id, i.author_id(), i.channel.as_ref().map(|c| c.id))
    else {
        return Ok(false);
    };
    match c.custom_id.as_str() {
        OPEN => {
            let client = app.http.interaction(Id::new(app.me.get()));
            let text = match open(app, gid, user).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(?e, "opening a ticket failed");
                    "Could not open a ticket, the bot may lack Manage Channels or Manage Roles."
                        .into()
                }
            };
            let r = ephemeral(
                InteractionResponseType::ChannelMessageWithSource,
                Some(&text),
            );
            client.create_response(i.id, &i.token, &r).await?;
        }
        CLOSE => close(app, i, gid, ch, user).await?,
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opener_or_staff_closes() {
        assert!(may_close(1, 1, false));
        assert!(may_close(1, 2, true));
        assert!(!may_close(1, 2, false));
    }

    #[test]
    fn ticket_is_hidden_from_everyone_but_member_bot_and_staff() {
        let o = overwrites(Id::new(9), 1, 2, &[3]);
        assert_eq!(o[0].id.get(), 9);
        assert_eq!(o[0].deny, Permissions::VIEW_CHANNEL);
        assert_eq!(
            o.iter()
                .filter(|w| w.allow.contains(Permissions::VIEW_CHANNEL))
                .map(|w| w.id.get())
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
}
