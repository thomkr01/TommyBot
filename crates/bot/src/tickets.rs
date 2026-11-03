//! Tickets: a member DMs the bot, picks a server they share with it, and the bot opens a channel
//! in that server's ticket category. Messages are relayed both ways until staff close the ticket,
//! which logs it (with the whole conversation as a file) to the ticket log.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_core::{permits, sections::TicketsCfg};
use tommybot_logging::embed::{avatar, clip, Log, BLUE, GREEN, ORANGE, RED};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::{
        message::{
            component::{
                ActionRow, Button, ButtonStyle, SelectMenu, SelectMenuOption, SelectMenuType,
            },
            AllowedMentions, Component, Embed, MentionType, Message,
        },
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
        Attachment as Uploaded, ChannelType,
    },
    gateway::payload::incoming::MessageCreate,
    guild::Permissions,
    http::{
        attachment::Attachment,
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    },
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
        Id,
    },
    user::User,
    util::Timestamp,
};
use twilight_util::builder::embed::{
    EmbedAuthorBuilder, EmbedBuilder, EmbedFieldBuilder, EmbedFooterBuilder, ImageSource,
};

use crate::{
    app::{now_secs, App},
    http_err::{self, best_effort},
    settings,
};

/// Prefix for commands in DMs, where no server prefix applies.
const DM_PREFIX: &str = "w!";
/// A confirmation or server menu stops working after this long.
const MENU_SECS: i64 = 180;
/// History read for the close log, newest first.
const HISTORY_PAGES: usize = 100;

const STAFF: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::READ_MESSAGE_HISTORY)
    .union(Permissions::ATTACH_FILES)
    .union(Permissions::EMBED_LINKS)
    .union(Permissions::ADD_REACTIONS);

/// Hidden from @everyone (its role id is the guild id); the bot and staff get in. The member never
/// sees the channel, everything reaches them by DM.
fn overwrites(gid: Id<GuildMarker>, bot: u64, staff: &[u64]) -> Vec<PermissionOverwrite> {
    let allow = |id: u64, kind| PermissionOverwrite {
        allow: STAFF,
        deny: Permissions::empty(),
        id: Id::new(id),
        kind,
    };
    let mut v = vec![
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
            id: gid.cast(),
            kind: PermissionOverwriteType::Role,
        },
        allow(bot, PermissionOverwriteType::Member),
    ];
    v.extend(
        staff
            .iter()
            .map(|r| allow(*r, PermissionOverwriteType::Role)),
    );
    v
}

/// Permit roles with Manage Tickets plus the access roles.
async fn staff_roles(app: &App, gid: Id<GuildMarker>, t: &TicketsCfg) -> Result<Vec<u64>> {
    let mut v: Vec<u64> = permits::roles_with(&app.pool, gid.get() as i64, "tickets")
        .await?
        .into_iter()
        .map(|r| r as u64)
        .collect();
    v.extend(t.access_roles.iter().map(|r| r.get()));
    v.sort_unstable();
    v.dedup();
    Ok(v)
}

/// `{username}`, `{userid}` and `{usermention}` in greeting, closing and snippet texts.
fn tags(text: &str, u: &User) -> String {
    clip(
        &text
            .replace("{username}", &u.name)
            .replace("{userid}", &u.id.to_string())
            .replace("{usermention}", &format!("<@{}>", u.id)),
        2048,
    )
}

/// The username without punctuation, or the id when nothing is left.
fn channel_name(name: &str, id: Id<UserMarker>) -> String {
    let n: String = name
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_ascii_punctuation() && !c.is_control())
        .collect();
    if n.trim().is_empty() {
        id.to_string()
    } else {
        n
    }
}

// ---- embeds ----

fn emb(title: &str, text: &str, color: u32) -> EmbedBuilder {
    let mut b = EmbedBuilder::new().color(color);
    if !title.is_empty() {
        b = b.title(title);
    }
    if !text.is_empty() {
        b = b.description(clip(text, 4096));
    }
    b
}

fn stamp(b: EmbedBuilder) -> EmbedBuilder {
    match Timestamp::from_secs(now_secs()) {
        Ok(t) => b.timestamp(t),
        Err(_) => b,
    }
}

fn footer(b: EmbedBuilder, text: &str, icon: Option<String>) -> EmbedBuilder {
    let mut f = EmbedFooterBuilder::new(clip(text, 2048));
    if let Some(src) = icon.and_then(|u| ImageSource::url(u).ok()) {
        f = f.icon_url(src);
    }
    b.footer(f)
}

fn author(b: EmbedBuilder, name: &str, icon: String) -> EmbedBuilder {
    let mut a = EmbedAuthorBuilder::new(clip(name, 256));
    if let Ok(src) = ImageSource::url(icon) {
        a = a.icon_url(src);
    }
    b.author(a)
}

fn user_footer(b: EmbedBuilder, u: &User) -> EmbedBuilder {
    footer(b, &format!("{} | {}", u.name, u.id), Some(avatar(u)))
}

fn info(text: &str) -> Embed {
    emb("", text, BLUE).build()
}

fn error(text: &str) -> Embed {
    emb("", text, RED).build()
}

async fn say(app: &App, ch: Id<ChannelMarker>, e: Embed) -> Result<()> {
    app.http
        .create_message(ch)
        .embeds(&[e])
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

/// A server the bot is in, for embed footers and the server menu.
struct Server {
    id: Id<GuildMarker>,
    name: String,
    icon: Option<String>,
}

impl Server {
    fn footer(&self, b: EmbedBuilder) -> EmbedBuilder {
        footer(
            b,
            &format!("{} | {}", self.name, self.id),
            self.icon.clone(),
        )
    }
}

/// `None` when the bot is not in the server.
async fn server(app: &App, gid: Id<GuildMarker>) -> Result<Option<Server>> {
    match app.http.guild(gid).await {
        Ok(r) => {
            let g = r.model().await?;
            Ok(Some(Server {
                id: g.id,
                icon: g
                    .icon
                    .map(|h| format!("https://cdn.discordapp.com/icons/{}/{h}.png", g.id)),
                name: g.name,
            }))
        }
        Err(e) if http_err::no_access(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Copies of a message's files, to post them again elsewhere. A file that fails to download is skipped.
async fn download(list: &[Uploaded]) -> Vec<Attachment> {
    let mut out = vec![];
    for (i, a) in list.iter().enumerate() {
        let got = async {
            anyhow::Ok(
                reqwest::get(&a.url)
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?,
            )
        };
        match got.await {
            Ok(b) => out.push(Attachment::from_bytes(
                a.filename.clone(),
                b.to_vec(),
                i as u64,
            )),
            Err(e) => tracing::warn!(?e, file = %a.filename, "ticket attachment download failed"),
        }
    }
    out
}

fn attachment_fields(mut b: EmbedBuilder, posted: &[Uploaded]) -> EmbedBuilder {
    for (n, a) in posted.iter().enumerate() {
        b = b.field(EmbedFieldBuilder::new(
            format!("Attachment {}", n + 1),
            a.url.clone(),
        ));
    }
    b
}

// ---- state ----

/// Whether the member wants a confirmation, and the server their last ticket message went to.
async fn user_state(app: &App, uid: Id<UserMarker>) -> Result<(bool, Option<Id<GuildMarker>>)> {
    let r: Option<(bool, Option<i64>)> =
        sqlx::query_as("SELECT confirmation, last_guild FROM ticket_users WHERE user_id = $1")
            .bind(uid.get() as i64)
            .fetch_optional(&app.pool)
            .await?;
    Ok(r.map_or((true, None), |(c, g)| {
        (c, g.and_then(|g| Id::new_checked(g as u64)))
    }))
}

async fn remember(app: &App, uid: Id<UserMarker>, gid: Id<GuildMarker>) -> Result<()> {
    sqlx::query(
        "INSERT INTO ticket_users (user_id, last_guild) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET last_guild = $2",
    )
    .bind(uid.get() as i64)
    .bind(gid.get() as i64)
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// The member's open ticket channel in a server.
async fn open_ticket(
    app: &App,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
) -> Result<Option<Id<ChannelMarker>>> {
    let ch: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT channel_id FROM tickets WHERE guild_id = $1 AND user_id = $2 AND closed_at IS NULL",
    )
    .bind(gid.get() as i64)
    .bind(uid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    Ok(ch.flatten().map(|c| Id::new(c as u64)))
}

/// The open ticket in this channel: its member. Only channels in the ticket category are looked up.
async fn ticket_here(
    app: &App,
    t: &TicketsCfg,
    gid: Id<GuildMarker>,
    ch: Id<ChannelMarker>,
) -> Result<Option<Id<UserMarker>>> {
    let Some(cat) = t.category else {
        return Ok(None);
    };
    if app.channel_scope(gid, ch).await?.1 != Some(cat) {
        return Ok(None);
    }
    let user: Option<i64> = sqlx::query_scalar(
        "SELECT user_id FROM tickets WHERE channel_id = $1 AND closed_at IS NULL",
    )
    .bind(ch.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    Ok(user.map(|u| Id::new(u as u64)))
}

/// The next ticket number in a server, or `None` when the member already has an open ticket there.
async fn claim_number(
    pool: &sqlx::PgPool,
    gid: Id<GuildMarker>,
    uid: Id<UserMarker>,
) -> Result<Option<i32>> {
    // Numbers are handed out one at a time per server; the unique open-ticket index makes a
    // second DM from the same member at the same instant lose here.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(gid.get() as i64)
        .execute(&mut *tx)
        .await?;
    let number = sqlx::query_scalar(
        "INSERT INTO tickets (guild_id, number, user_id)
         SELECT $1, COALESCE(MAX(number), 0) + 1, $2 FROM tickets WHERE guild_id = $1
         ON CONFLICT DO NOTHING RETURNING number",
    )
    .bind(gid.get() as i64)
    .bind(uid.get() as i64)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(number)
}

/// Servers whose newest config has tickets on.
async fn ticket_guilds(pool: &sqlx::PgPool) -> Result<Vec<i64>> {
    // shortcut: reads every server's newest config; fine for hundreds of servers, index tickets.enabled when it grows.
    Ok(sqlx::query_scalar(
        "SELECT guild_id FROM (
            SELECT DISTINCT ON (guild_id) guild_id, config FROM guild_config ORDER BY guild_id, version DESC
         ) c WHERE c.config #>> '{tickets,enabled}' = 'true'",
    )
    .fetch_all(pool)
    .await?)
}

/// A deleted ticket channel ends its ticket.
pub async fn on_channel_delete(app: &App, ch: Id<ChannelMarker>) -> Result<()> {
    sqlx::query("UPDATE tickets SET closed_at = now() WHERE channel_id = $1 AND closed_at IS NULL")
        .bind(ch.get() as i64)
        .execute(&app.pool)
        .await?;
    Ok(())
}

// ---- member side (DMs) ----

/// `w!word args` or `@bot word args` in a DM.
fn dm_command(content: &str, me: Id<UserMarker>) -> Option<(String, &str)> {
    let rest = [
        DM_PREFIX.to_string(),
        format!("<@{me}> "),
        format!("<@!{me}> "),
    ]
    .iter()
    .find_map(|p| content.strip_prefix(p.as_str()))?
    .trim_start();
    let (word, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((word.to_lowercase(), args.trim()))
}

/// What a DM asks to send: the text after `new` or `send ID`, else the whole message.
fn body(content: &str, me: Id<UserMarker>) -> String {
    match dm_command(content, me) {
        Some((w, a)) if matches!(w.as_str(), "new" | "create" | "switch" | "change") => a.into(),
        Some((w, a)) if w == "send" => a
            .split_once(char::is_whitespace)
            .map_or("", |x| x.1)
            .trim()
            .into(),
        _ => content.into(),
    }
}

pub async fn on_dm(app: &App, m: &Message) -> Result<()> {
    let dm = m.channel_id;
    if let Some((word, args)) = dm_command(&m.content, app.me) {
        return match word.as_str() {
            "new" | "create" | "switch" | "change"
                if args.is_empty() && m.attachments.is_empty() =>
            {
                say(
                    app,
                    dm,
                    error(&format!("Usage: `{DM_PREFIX}new <message>`")),
                )
                .await
            }
            "new" | "create" | "switch" | "change" => pick(app, m).await,
            "send" => match args
                .split_whitespace()
                .next()
                .and_then(|g| g.parse().ok())
                .and_then(Id::new_checked)
            {
                Some(g) if !body(&m.content, app.me).is_empty() || !m.attachments.is_empty() => {
                    send_mail(app, m, g).await
                }
                _ => {
                    say(
                        app,
                        dm,
                        error(&format!("Usage: `{DM_PREFIX}send <server ID> <message>`")),
                    )
                    .await
                }
            },
            "confirmation" => confirmation(app, m).await,
            _ => Ok(()),
        };
    }
    if m.content.trim().is_empty() && m.attachments.is_empty() {
        return Ok(());
    }
    match user_state(app, m.author.id).await? {
        (true, Some(g)) => match server(app, g).await? {
            Some(s) => ask(app, m, &s).await,
            None => pick(app, m).await,
        },
        (false, Some(g)) => send_mail(app, m, g).await,
        (_, None) => pick(app, m).await,
    }
}

async fn confirmation(app: &App, m: &Message) -> Result<()> {
    let (on, _) = user_state(app, m.author.id).await?;
    sqlx::query(
        "INSERT INTO ticket_users (user_id, confirmation) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET confirmation = $2",
    )
    .bind(m.author.id.get() as i64)
    .bind(!on)
    .execute(&app.pool)
    .await?;
    let text = if on {
        format!("Confirmation messages are disabled. To send messages to another server, either use `{DM_PREFIX}new <message>` or `{DM_PREFIX}send <server ID> <message>`.")
    } else {
        "Confirmation messages are enabled.".into()
    };
    say(app, m.channel_id, info(&text)).await
}

fn button(label: &str, id: String, style: ButtonStyle) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(id),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

fn action_row(components: Vec<Component>) -> Component {
    Component::ActionRow(ActionRow {
        id: None,
        components,
    })
}

async fn ask(app: &App, m: &Message, s: &Server) -> Result<()> {
    let e = emb(
        "Confirmation",
        &format!(
            "You're sending this message to **{}** (ID: {}). Press **Send** to confirm.\nWant to send to another server? Press **Other server**.\nTo cancel this request, press **Cancel**.",
            s.name, s.id
        ),
        BLUE,
    );
    let e = footer(
        e,
        &format!(
            "Tip: You can disable confirmation messages with the {DM_PREFIX}confirmation command."
        ),
        None,
    );
    let row = action_row(vec![
        button(
            "Send",
            format!("tk:ok:{}:{}", s.id, m.id),
            ButtonStyle::Success,
        ),
        button(
            "Other server",
            format!("tk:other:{}", m.id),
            ButtonStyle::Secondary,
        ),
        button("Cancel", "tk:no".into(), ButtonStyle::Danger),
    ]);
    app.http
        .create_message(m.channel_id)
        .embeds(&[e.build()])
        .components(&[row])
        .await?;
    Ok(())
}

/// Servers the member shares with the bot that take tickets, and whether a ticket is open there.
async fn mutual(app: &App, uid: Id<UserMarker>) -> Result<Vec<(Server, bool)>> {
    let guilds = ticket_guilds(&app.pool).await?;
    let open: Vec<i64> =
        sqlx::query_scalar("SELECT guild_id FROM tickets WHERE user_id = $1 AND closed_at IS NULL")
            .bind(uid.get() as i64)
            .fetch_all(&app.pool)
            .await?;
    let mut out = vec![];
    for g in guilds.into_iter().filter_map(|g| Id::new_checked(g as u64)) {
        // A server the bot left answers with an error; it is simply not shared.
        if !matches!(http_err::member(&app.http, g, uid).await, Ok(Some(_))) {
            continue;
        }
        if let Some(s) = server(app, g).await? {
            out.push((s, open.contains(&(g.get() as i64))));
        }
        if out.len() == 25 {
            break;
        }
    }
    Ok(out)
}

/// The server menu, or the error to show instead.
async fn pick_view(
    app: &App,
    uid: Id<UserMarker>,
    msg: Id<MessageMarker>,
) -> Result<(Embed, Vec<Component>)> {
    let list = mutual(app, uid).await?;
    if list.is_empty() {
        return Ok((
            error("Oops, something strange happened. No server found."),
            vec![],
        ));
    }
    let options = list
        .iter()
        .map(|(s, open)| SelectMenuOption {
            default: false,
            description: Some(
                if *open {
                    "Existing ticket."
                } else {
                    "Create a new ticket."
                }
                .into(),
            ),
            emoji: None,
            label: clip(&s.name, 100),
            value: s.id.to_string(),
        })
        .collect();
    let menu = Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id: format!("tk:pick:{msg}"),
        default_values: None,
        disabled: false,
        kind: SelectMenuType::Text,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(options),
        placeholder: Some("Select a server".into()),
        required: None,
    });
    let e = emb(
        "Select Server",
        "Please select the server you want to send this message to.",
        BLUE,
    );
    Ok((e.build(), vec![action_row(vec![menu])]))
}

async fn pick(app: &App, m: &Message) -> Result<()> {
    let (e, rows) = pick_view(app, m.author.id, m.id).await?;
    app.http
        .create_message(m.channel_id)
        .embeds(&[e])
        .components(&rows)
        .await?;
    Ok(())
}

/// Sends a member's DM to a server, opening a ticket there first when none is open.
async fn send_mail(app: &App, m: &Message, gid: Id<GuildMarker>) -> Result<()> {
    let dm = m.channel_id;
    let Some(s) = server(app, gid).await? else {
        return say(app, dm, error("The server was not found.")).await;
    };
    let Some(member) = http_err::member(&app.http, gid, m.author.id).await? else {
        return say(
            app,
            dm,
            error("You are not in that server, and the message is not sent."),
        )
        .await;
    };
    let cfg = app.cfg(gid.get()).await?;
    let t = &cfg.tickets;
    let info_ = app.guild_info(gid).await?;
    let Some(category) = t
        .category
        .filter(|c| t.enabled && info_.channels.contains(c))
    else {
        let text = "A ticket category is not found. The bot is not set up properly in the server.";
        return say(app, dm, error(text)).await;
    };
    if t.blocked.contains(&m.author.id) {
        let text = "That server has blacklisted you from sending a message there.";
        return say(app, dm, error(text)).await;
    }
    let ch = match open_ticket(app, gid, m.author.id).await? {
        Some(ch) => ch,
        None if t.paused => {
            let reason = if t.pause_reason.is_empty() {
                "No reason was provided."
            } else {
                &t.pause_reason
            };
            let e = s.footer(stamp(emb("Ticket Creation Disabled", reason, RED)));
            return say(app, dm, e.build()).await;
        }
        None => match create(app, &s, m, &member.roles, t, category).await? {
            Some(ch) => ch,
            None => return Ok(()),
        },
    };
    let text = body(&m.content, app.me);
    let files = download(&m.attachments).await;
    let sent = s.footer(stamp(emb("Message Sent", &text, GREEN)));
    let dm_msg = app
        .http
        .create_message(dm)
        .embeds(&[sent.build()])
        .attachments(&files)
        .await?
        .model()
        .await?;
    let got = user_footer(stamp(emb("Message Received", &text, GREEN)), &m.author);
    let got = attachment_fields(got, &dm_msg.attachments);
    if let Err(e) = app
        .http
        .create_message(ch)
        .embeds(&[got.build()])
        .attachments(&files)
        .await
    {
        tracing::warn!(?e, %ch, "relaying a ticket message failed");
        best_effort(
            app.http.delete_message(dm, dm_msg.id).await,
            "undo ticket dm",
        );
        let text = "The bot is missing permissions. Please contact an admin on the server.";
        return say(app, dm, error(text)).await;
    }
    remember(app, m.author.id, gid).await
}

/// Opens the ticket channel, logs it and greets the member. `None` when the member was told why not.
async fn create(
    app: &App,
    s: &Server,
    m: &Message,
    roles: &[Id<RoleMarker>],
    t: &TicketsCfg,
    category: Id<ChannelMarker>,
) -> Result<Option<Id<ChannelMarker>>> {
    let (gid, user, dm) = (s.id, &m.author, m.channel_id);
    let g = gid.get() as i64;
    let number = claim_number(&app.pool, gid, user.id).await?;
    let Some(number) = number else {
        let text = "Your ticket is being opened, please send your message again in a moment.";
        say(app, dm, error(text)).await?;
        return Ok(None);
    };
    let ows = overwrites(gid, app.me.get(), &staff_roles(app, gid, t).await?);
    let make = |name: String| {
        let ows = &ows;
        async move {
            let r = app
                .http
                .create_guild_channel(gid, &name)
                .kind(ChannelType::GuildText)
                .parent_id(category)
                .permission_overwrites(ows)
                .await?;
            anyhow::Ok(r.model().await?.id)
        }
    };
    let made = match make(channel_name(&user.name, user.id)).await {
        // Discord refuses names with words it does not allow (50035); the id always passes.
        Err(e) if http_err::code(&e) == Some(50035) => make(user.id.to_string()).await,
        r => r,
    };
    let ch = match made {
        Ok(ch) => ch,
        Err(e) => {
            sqlx::query("DELETE FROM tickets WHERE guild_id = $1 AND number = $2")
                .bind(g)
                .bind(number)
                .execute(&app.pool)
                .await?;
            let text = match http_err::code(&e) {
                Some(30013) => "The server has reached the maximum number of active tickets. Please try again later.".into(),
                _ => format!("An error occurred. Please contact an admin on the server with the following information: {}.", http_err::why(&e)),
            };
            say(app, dm, error(&text)).await?;
            return Ok(None);
        }
    };
    sqlx::query("UPDATE tickets SET channel_id = $3 WHERE guild_id = $1 AND number = $2")
        .bind(g)
        .bind(number)
        .bind(ch.get() as i64)
        .execute(&app.pool)
        .await?;
    if let Some(log) = t.log_channel {
        let e = Log::new("New Ticket", GREEN)
            .user("User", user.id.get())
            .footer(&format!("{} | {}", user.name, user.id));
        if let Err(e) = crate::events::send_private(app, log, &e.time().build(), None).await {
            tracing::warn!(?e, "ticket log failed");
        }
    }
    let pfx = &app.cfg(gid.get()).await?.misc.prefix;
    let how = if t.command_only {
        format!("Type `{pfx}reply <message>` in this channel to reply. All other messages are ignored, and can be used for staff discussion. Use the command `{pfx}close [reason]` to close this ticket. (Command only mode enabled)")
    } else {
        format!("Type a message in this channel to reply. Messages starting with the server prefix `{pfx}` are ignored, and can be used for staff discussion. Use the command `{pfx}close [reason]` to close this ticket.")
    };
    let role_list: String = roles.iter().map(|r| format!("<@&{r}> ")).collect();
    let role_list = match role_list.trim_end() {
        "" => "*None*".to_string(),
        l if l.len() <= 1024 => l.to_string(),
        _ => format!("*{} roles*", roles.len()),
    };
    let intro = stamp(emb("New Ticket", &how, BLUE))
        .field(EmbedFieldBuilder::new("User", format!("<@{}> ({})", user.id, user.id)).inline())
        .field(EmbedFieldBuilder::new("Roles", role_list).inline());
    let intro = user_footer(intro, user);
    let (pings, mentions) = pings(gid, t);
    if let Err(e) = app
        .http
        .create_message(ch)
        .content(&pings)
        .embeds(&[intro.build()])
        .allowed_mentions(Some(&mentions))
        .await
    {
        tracing::warn!(?e, %ch, "ticket intro failed");
        let text = "The bot is missing permissions. Please contact an admin on the server.";
        say(app, dm, error(text)).await?;
        return Ok(None);
    }
    if !t.greeting.is_empty() {
        let e = s.footer(stamp(emb(
            "Greeting Message",
            &tags(&t.greeting, user),
            ORANGE,
        )));
        say(app, dm, e.build()).await?;
    }
    Ok(Some(ch))
}

/// The ping line for a new ticket and the mentions it may use.
fn pings(gid: Id<GuildMarker>, t: &TicketsCfg) -> (String, AllowedMentions) {
    let mut words = vec![];
    let mut am = AllowedMentions::default();
    for r in &t.ping_roles {
        if r.get() == gid.get() {
            words.push("@everyone".to_string());
            am.parse = vec![MentionType::Everyone];
        } else {
            words.push(format!("<@&{r}>"));
            am.roles.push(*r);
        }
    }
    if t.ping_here {
        words.push("@here".into());
        am.parse = vec![MentionType::Everyone];
    }
    (words.join(" "), am)
}

/// Buttons and the server menu in DMs.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix("tk:") else {
        return Ok(false);
    };
    let (Some(user), Some(prompt)) = (i.author_id(), i.message.as_ref()) else {
        return Ok(true);
    };
    let client = app.http.interaction(app.me.cast());
    let update = |e: Embed, rows: Vec<Component>| InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            embeds: Some(vec![e]),
            components: Some(rows),
            ..Default::default()
        }),
    };
    let made_ms = ((prompt.id.get() >> 22) + 1_420_070_400_000) as i64;
    if now_secs() * 1000 - made_ms > MENU_SECS * 1000 {
        let r = update(
            error("This request has expired. Send your message again."),
            vec![],
        );
        client.create_response(i.id, &i.token, &r).await?;
        return Ok(true);
    }
    let parts: Vec<&str> = rest.split(':').collect();
    // The member's own DM, fetched again so nothing is kept between the prompt and the answer.
    let original = |id: &str| {
        let id = id.parse().ok().and_then(Id::new_checked);
        async move {
            let Some(id) = id else {
                return anyhow::Ok(None);
            };
            match app.http.message(prompt.channel_id, id).await {
                Ok(r) => Ok(Some(r.model().await?)),
                Err(e) if http_err::gone(&e) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
    };
    let chosen = match parts.as_slice() {
        ["no"] => {
            let r = update(error("Request cancelled successfully."), vec![]);
            client.create_response(i.id, &i.token, &r).await?;
            return Ok(true);
        }
        ["other", msg] => {
            let defer = InteractionResponse {
                kind: InteractionResponseType::DeferredUpdateMessage,
                data: None,
            };
            client.create_response(i.id, &i.token, &defer).await?;
            let Some(msg) = msg.parse().ok().and_then(Id::new_checked) else {
                return Ok(true);
            };
            let (e, rows) = pick_view(app, user, msg).await?;
            client
                .update_response(&i.token)
                .embeds(Some(&[e]))
                .components(Some(&rows))
                .await?;
            return Ok(true);
        }
        ["ok", g, msg] => g.parse().ok().and_then(Id::new_checked).map(|g| (g, *msg)),
        ["pick", msg] => c
            .values
            .first()
            .and_then(|v| v.parse().ok())
            .and_then(Id::new_checked)
            .map(|g| (g, *msg)),
        _ => None,
    };
    let Some((gid, msg)) = chosen else {
        return Ok(true);
    };
    let defer = InteractionResponse {
        kind: InteractionResponseType::DeferredUpdateMessage,
        data: None,
    };
    client.create_response(i.id, &i.token, &defer).await?;
    best_effort(
        app.http.delete_message(prompt.channel_id, prompt.id).await,
        "ticket prompt",
    );
    match original(msg).await? {
        Some(m) => send_mail(app, &m, gid).await,
        None => {
            say(
                app,
                prompt.channel_id,
                error("Your message was deleted, so nothing was sent."),
            )
            .await
        }
    }?;
    Ok(true)
}

// ---- staff side ----

/// Relays a staff message to the member: a reply, a snippet or a plain message in the ticket.
#[allow(clippy::too_many_arguments)]
async fn relay(
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
    let mut to_member = s.footer(stamp(emb("Message Received", &text, ORANGE)));
    if !anon {
        to_member = author(to_member, &staff.name, avatar(staff));
    }
    let files = download(&m.attachments).await;
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
    let shown = if anon {
        format!("{} (Anonymous)", staff.name)
    } else {
        staff.name.clone()
    };
    let copy = author(
        stamp(emb("Message Sent", &text, ORANGE)),
        &shown,
        avatar(staff),
    );
    let copy = attachment_fields(user_footer(copy, &member.user), &posted.attachments);
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
    match title {
        // From the member: the footer is "name | id".
        "Message Received" => Some(format!(
            "{} (User)",
            footer
                .and_then(|f| f.rsplit_once(" | "))
                .map_or("Unknown", |x| x.0)
        )),
        "Message Sent" => Some(format!(
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
async fn close(
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
        let e = s.footer(stamp(emb(
            "Closing Message",
            &tags(&t.closing, &member.user),
            ORANGE,
        )));
        best_effort(
            app.http.create_message(dm).embeds(&[e.build()]).await,
            "ticket closing message",
        );
    }
    let mut e = s.footer(stamp(emb("Ticket Closed", reason, RED)));
    if !anon {
        e = author(e, &staff.name, avatar(staff));
    }
    best_effort(
        app.http.create_message(dm).embeds(&[e.build()]).await,
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

// ---- commands ----

const STAFF_CMDS: &[&str] = &[
    "reply",
    "r",
    "areply",
    "ar",
    "close",
    "c",
    "aclose",
    "ac",
    "closeall",
    "acloseall",
    "blacklist",
    "block",
    "whitelist",
    "unblock",
    "blacklistclear",
    "viewblacklist",
    "snippet",
    "s",
    "asnippet",
    "as",
    "snippetadd",
    "snippetremove",
    "snippetclear",
    "viewsnippet",
    "viewsnippets",
    "snippetlist",
    "vs",
];
const ADMIN_CMDS: &[&str] = &[
    "ticketsetup",
    "category",
    "accessrole",
    "modrole",
    "supportrole",
    "pingrole",
    "mentionrole",
    "logging",
    "logs",
    "commandonly",
    "commandrequired",
    "greetingmessage",
    "welcomemessage",
    "greetmessage",
    "closingmessage",
    "goodbyemessage",
    "closemessage",
    "loggingplus",
    "advancedlogging",
    "advancedlogs",
    "anonymous",
    "toggle",
    "enable",
    "disable",
    "viewconfig",
];
/// Short names other commands already use; they mean the ticket command only inside a ticket.
const TICKET_ONLY: &[&str] = &["r", "c", "s", "whitelist"];

/// Staff talk in a ticket is relayed to the member; ticket commands run anywhere. True when handled.
pub async fn on_message(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    rest: Option<&str>,
) -> Result<bool> {
    let cfg = app.cfg(gid.get()).await?;
    let word = rest.map(|r| {
        let r = r.trim_start();
        let (w, a) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
        (w.to_lowercase(), a.trim())
    });
    let Some((word, args)) = word else {
        // Plain message: relayed when in a ticket, unless it starts with a bot mention.
        let me = [format!("<@{}>", app.me), format!("<@!{}>", app.me)];
        if cfg.tickets.command_only
            || me.iter().any(|p| m.content.starts_with(p.as_str()))
            || (m.content.trim().is_empty() && m.attachments.is_empty())
        {
            return Ok(false);
        }
        let Some(user) = ticket_here(app, &cfg.tickets, gid, m.channel_id).await? else {
            return Ok(false);
        };
        relay(app, m, gid, user, &m.content, cfg.tickets.anonymous, false).await?;
        return Ok(true);
    };
    let w = word.as_str();
    let admin = ADMIN_CMDS.contains(&w);
    if !admin && !STAFF_CMDS.contains(&w) {
        return Ok(false);
    }
    let ticket = ticket_here(app, &cfg.tickets, gid, m.channel_id).await?;
    if TICKET_ONLY.contains(&w) && ticket.is_none() {
        return Ok(false);
    }
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, m.author.id).await?;
    let ch = m.channel_id;
    let roles = m.member.as_ref().map_or(&[][..], |mem| &mem.roles);
    let allowed = if admin {
        level.immune()
    } else {
        level.has("tickets") || roles.iter().any(|r| cfg.tickets.access_roles.contains(r))
    };
    if !allowed {
        say(app, ch, error("You do not have access to this command.")).await?;
        return Ok(true);
    }
    let pfx = cfg.misc.prefix.clone();
    let t = &cfg.tickets;
    if w != "ticketsetup" && w != "category" && t.category.is_none() {
        let text = format!("Your server has not been set up yet. Use `{pfx}ticketsetup` first.");
        say(app, ch, error(&text)).await?;
        return Ok(true);
    }
    let needs_ticket = matches!(
        w,
        "reply"
            | "r"
            | "areply"
            | "ar"
            | "close"
            | "c"
            | "aclose"
            | "ac"
            | "snippet"
            | "s"
            | "asnippet"
            | "as"
    );
    if needs_ticket && ticket.is_none() {
        say(app, ch, error("This channel is not a ticket channel.")).await?;
        return Ok(true);
    }
    run(app, m, gid, &cfg, ticket, w, args).await?;
    Ok(true)
}

/// Ids typed as mentions or numbers; `Err` when a word is neither.
fn ids(args: &str) -> Result<Vec<u64>, ()> {
    args.split_whitespace()
        .map(|t| {
            let d = t
                .trim_start_matches(['<', '@', '!', '&', '#'])
                .trim_end_matches('>');
            d.parse::<u64>().ok().filter(|n| *n > 0).ok_or(())
        })
        .collect()
}

async fn save(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    f: impl FnOnce(&mut Value),
) -> Result<bool> {
    let r = settings::save(app, gid, m.author.id.get(), &["tickets"], |doc| {
        if !doc["tickets"].is_object() {
            doc["tickets"] = json!({});
        }
        f(&mut doc["tickets"]);
        Ok(())
    })
    .await?;
    if let Err(e) = r {
        say(app, m.channel_id, error(&e)).await?;
        return Ok(false);
    }
    Ok(true)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    x.as_str()
                        .map(String::from)
                        .or_else(|| x.as_u64().map(|n| n.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn blocked_page(t: &TicketsCfg, page: usize) -> (Embed, usize) {
    let pages = t.blocked.len().div_ceil(25).max(1);
    let page = page.min(pages - 1);
    let lines: Vec<String> = t
        .blocked
        .iter()
        .skip(page * 25)
        .take(25)
        .map(|u| format!("<@{u}> ({u})"))
        .collect();
    (emb("Blacklist", &lines.join("\n"), BLUE).build(), pages)
}

pub fn snippets_page(t: &TicketsCfg, page: usize) -> (Embed, usize) {
    let pages = t.snippets.len().div_ceil(10).max(1);
    let page = page.min(pages - 1);
    let mut e = emb("Snippets", "", BLUE);
    for s in t.snippets.iter().skip(page * 10).take(10) {
        e = e.field(EmbedFieldBuilder::new(
            clip(&s.name, 256),
            clip(&s.content, 100),
        ));
    }
    (e.build(), pages)
}

async fn paged(app: &App, m: &MessageCreate, kind: &str, (e, pages): (Embed, usize)) -> Result<()> {
    let rows: Vec<Component> = (pages > 1)
        .then(|| settings::page_row(m.author.id.get(), kind, 0, pages))
        .into_iter()
        .collect();
    app.http
        .create_message(m.channel_id)
        .embeds(&[e])
        .components(&rows)
        .await?;
    Ok(())
}

async fn run(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    cfg: &crate::app::GuildCfg,
    ticket: Option<Id<UserMarker>>,
    w: &str,
    args: &str,
) -> Result<()> {
    let ch = m.channel_id;
    let t = &cfg.tickets;
    let pfx = &cfg.misc.prefix;
    let ok = |text: String| say(app, ch, info(&text));
    let no = |text: String| say(app, ch, error(&text));
    match w {
        "reply" | "r" | "areply" | "ar" => {
            if args.is_empty() && m.attachments.is_empty() {
                return no(format!("Usage: `{pfx}{w} <message>`")).await;
            }
            let Some(user) = ticket else { return Ok(()) };
            relay(app, m, gid, user, args, w.starts_with('a'), false).await
        }
        "snippet" | "s" | "asnippet" | "as" => {
            let Some(user) = ticket else { return Ok(()) };
            let name = args.to_lowercase();
            match t.snippets.iter().find(|s| s.name == name) {
                Some(s) => relay(app, m, gid, user, &s.content, w.starts_with('a'), true).await,
                None => no("The snippet was not found.".into()).await,
            }
        }
        "close" | "c" | "aclose" | "ac" => {
            let Some(user) = ticket else { return Ok(()) };
            close(app, gid, ch, user, &m.author, args, w.starts_with('a')).await
        }
        "closeall" | "acloseall" => {
            let anon = w == "acloseall";
            let open: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT channel_id, user_id FROM tickets WHERE guild_id = $1 AND closed_at IS NULL AND channel_id IS NOT NULL",
            )
            .bind(gid.get() as i64)
            .fetch_all(&app.pool)
            .await?;
            for (c, u) in open {
                if let Err(e) = close(
                    app,
                    gid,
                    Id::new(c as u64),
                    Id::new(u as u64),
                    &m.author,
                    args,
                    anon,
                )
                .await
                {
                    tracing::warn!(?e, channel = c, "closing a ticket failed");
                }
            }
            let text = if anon {
                "All tickets are successfully closed anonymously."
            } else {
                "All tickets are successfully closed."
            };
            best_effort(
                app.http.create_message(ch).embeds(&[info(text)]).await,
                "closeall reply",
            );
            Ok(())
        }
        "blacklist" | "block" | "whitelist" | "unblock" => {
            let add = matches!(w, "blacklist" | "block");
            let Ok(mut users) = ids(args) else {
                return no("The user(s) are not found. Please try again.".into()).await;
            };
            if users.is_empty() {
                users.extend(ticket.map(|u| u.get()));
            }
            if users.is_empty() {
                return no("You must provide users or run this command in a ticket.".into()).await;
            }
            let saved = save(app, m, gid, |tk| {
                let mut list = strings(&tk["blocked"]);
                for u in users.iter().map(u64::to_string) {
                    list.retain(|x| *x != u);
                    if add {
                        list.push(u);
                    }
                }
                tk["blocked"] = json!(list);
            })
            .await?;
            if !saved {
                return Ok(());
            }
            ok(format!(
                "The user(s) are {} successfully.",
                if add { "blacklisted" } else { "whitelisted" }
            ))
            .await
        }
        "blacklistclear" => {
            if save(app, m, gid, |tk| tk["blocked"] = json!([])).await? {
                ok("The blacklist is cleared successfully.".into()).await?;
            }
            Ok(())
        }
        "viewblacklist" if t.blocked.is_empty() => ok("No one is blacklisted.".into()).await,
        "viewblacklist" => paged(app, m, "tkblock", blocked_page(t, 0)).await,
        "snippetadd" => {
            let (name, content) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
            let (name, content) = (name.to_lowercase(), content.trim());
            if name.is_empty() || content.is_empty() {
                return no(format!("Usage: `{pfx}snippetadd <name> <content>`")).await;
            }
            if name.chars().count() > 100 {
                return no("The snippet name cannot exceed 100 characters.".into()).await;
            }
            if content.chars().count() > 1000 {
                return no("The snippet content cannot exceed 1000 characters.".into()).await;
            }
            if t.snippets.iter().any(|s| s.name == name) {
                return no("A snippet with that name already exists.".into()).await;
            }
            let author = m.author.id.to_string();
            let entry = json!({"name": name, "content": content, "author": author});
            if save(app, m, gid, |tk| {
                let mut list = tk["snippets"].as_array().cloned().unwrap_or_default();
                list.push(entry);
                tk["snippets"] = json!(list);
            })
            .await?
            {
                ok("The snippet was added successfully.".into()).await?;
            }
            Ok(())
        }
        "snippetremove" => {
            let name = args.to_lowercase();
            if !t.snippets.iter().any(|s| s.name == name) {
                return no("A snippet with that name was not found.".into()).await;
            }
            if save(app, m, gid, |tk| {
                let mut list = tk["snippets"].as_array().cloned().unwrap_or_default();
                list.retain(|s| s["name"].as_str() != Some(name.as_str()));
                tk["snippets"] = json!(list);
            })
            .await?
            {
                ok("The snippet was removed successfully.".into()).await?;
            }
            Ok(())
        }
        "snippetclear" => {
            if save(app, m, gid, |tk| tk["snippets"] = json!([])).await? {
                ok("All snippets were removed successfully.".into()).await?;
            }
            Ok(())
        }
        "viewsnippet" | "viewsnippets" | "snippetlist" | "vs" if !args.is_empty() => {
            let name = args.to_lowercase();
            let Some(s) = t.snippets.iter().find(|s| s.name == name) else {
                return no("A snippet with that name was not found.".into()).await;
            };
            let by = s
                .author
                .map_or("*Unknown*".to_string(), |a| format!("<@{a}>"));
            let e = emb("Snippet", "", BLUE)
                .field(EmbedFieldBuilder::new("Name", clip(&s.name, 1024)))
                .field(EmbedFieldBuilder::new("Content", clip(&s.content, 1024)))
                .field(EmbedFieldBuilder::new("Author", by));
            say(app, ch, e.build()).await
        }
        "viewsnippet" | "viewsnippets" | "snippetlist" | "vs" if t.snippets.is_empty() => {
            ok("No snippet has been added yet.".into()).await
        }
        "viewsnippet" | "viewsnippets" | "snippetlist" | "vs" => {
            paged(app, m, "tksnip", snippets_page(t, 0)).await
        }
        "ticketsetup" | "category" => setup(app, m, gid, t, w, args).await,
        "accessrole" | "modrole" | "supportrole" | "pingrole" | "mentionrole" => {
            let ping = matches!(w, "pingrole" | "mentionrole");
            let mut here = false;
            let mut list = vec![];
            for tok in args.split_whitespace() {
                match tok.trim_start_matches('@').to_lowercase().as_str() {
                    "everyone" if ping => list.push(gid.to_string()),
                    "here" if ping => here = true,
                    _ => match ids(tok) {
                        Ok(v) => list.extend(v.iter().map(u64::to_string)),
                        Err(()) => {
                            return no("The role(s) are not found. Please try again.".into()).await
                        }
                    },
                }
            }
            if list.len() + usize::from(here) > 10 {
                return no("There can at most be 10 roles. Try using the command again but specify fewer roles.".into()).await;
            }
            if save(app, m, gid, |tk| {
                if ping {
                    tk["ping_roles"] = json!(list);
                    tk["ping_here"] = json!(here);
                } else {
                    tk["access_roles"] = json!(list);
                }
            })
            .await?
            {
                ok("The role(s) are updated successfully.".into()).await?;
            }
            Ok(())
        }
        "logging" | "logs" => {
            let given = match ids(args) {
                Ok(v) => v.first().copied(),
                Err(()) => return no("The channel is not found. Please try again.".into()).await,
            };
            if t.log_channel.is_some() && given.is_none() {
                if save(app, m, gid, |tk| tk["log_channel"] = Value::Null).await? {
                    ok("Ticket logging is disabled. You may delete the channel.".into()).await?;
                }
                return Ok(());
            }
            let Some(cat) = t.category else { return Ok(()) };
            let log = match given {
                Some(c) => c,
                None => {
                    let ows = overwrites(gid, app.me.get(), &staff_roles(app, gid, t).await?);
                    app.http
                        .create_guild_channel(gid, "ticket-logs")
                        .kind(ChannelType::GuildText)
                        .parent_id(cat)
                        .permission_overwrites(&ows)
                        .await?
                        .model()
                        .await?
                        .id
                        .get()
                }
            };
            if save(app, m, gid, |tk| tk["log_channel"] = json!(log.to_string())).await? {
                ok("Ticket logging is enabled.".into()).await?;
            }
            Ok(())
        }
        "commandonly" | "commandrequired" | "anonymous" | "loggingplus" | "advancedlogging"
        | "advancedlogs" => {
            let (key, now, text) = match w {
                "commandonly" | "commandrequired" => {
                    ("command_only", !t.command_only, "Command only mode")
                }
                "anonymous" => ("anonymous", !t.anonymous, "Anonymous messaging"),
                _ => ("full_log", !t.full_log, "Advanced logging"),
            };
            if save(app, m, gid, |tk| tk[key] = json!(now)).await? {
                ok(format!(
                    "{text} is {}.",
                    if now { "enabled" } else { "disabled" }
                ))
                .await?;
            }
            Ok(())
        }
        "greetingmessage" | "welcomemessage" | "greetmessage" | "closingmessage"
        | "goodbyemessage" | "closemessage" => {
            let (key, what) = if matches!(w, "greetingmessage" | "welcomemessage" | "greetmessage")
            {
                ("greeting", "greeting")
            } else {
                ("closing", "closing")
            };
            if save(app, m, gid, |tk| tk[key] = json!(args)).await? {
                ok(format!("The {what} message is set successfully.")).await?;
            }
            Ok(())
        }
        "toggle" | "enable" | "disable" => {
            let pause = !t.paused;
            if save(app, m, gid, |tk| {
                tk["paused"] = json!(pause);
                tk["pause_reason"] = json!(if pause { args } else { "" });
            })
            .await?
            {
                ok(format!(
                    "Ticket creation is {}.",
                    if pause { "disabled" } else { "enabled" }
                ))
                .await?;
            }
            Ok(())
        }
        "viewconfig" => say(app, ch, view_config(t, pfx, gid)).await,
        _ => Ok(()),
    }
}

fn view_config(t: &TicketsCfg, pfx: &str, gid: Id<GuildMarker>) -> Embed {
    let or_unset = |s: String| {
        if s.is_empty() {
            "*Not set*".to_string()
        } else {
            s
        }
    };
    let on = |b: bool| if b { "Enabled" } else { "Disabled" }.to_string();
    let access: Vec<String> = t.access_roles.iter().map(|r| format!("<@&{r}>")).collect();
    let mut ping: Vec<String> = t
        .ping_roles
        .iter()
        .map(|r| {
            if r.get() == gid.get() {
                "@everyone".into()
            } else {
                format!("<@&{r}>")
            }
        })
        .collect();
    if t.ping_here {
        ping.push("@here".into());
    }
    let reason = if t.pause_reason.is_empty() {
        "No reason was provided."
    } else {
        &t.pause_reason
    };
    let creation = if t.paused {
        format!("Disabled ({})", clip(reason, 989))
    } else {
        "Enabled".into()
    };
    let f = |n: &str, v: String| EmbedFieldBuilder::new(n, v).inline();
    emb("Server Configurations", "", BLUE)
        .field(f("Prefix", pfx.to_string()))
        .field(f(
            "Category",
            t.category.map_or("*Not set*".into(), |c| format!("<#{c}>")),
        ))
        .field(f("Access Roles", or_unset(access.join(" "))))
        .field(f("Ping Roles", or_unset(ping.join(" "))))
        .field(f(
            "Logging",
            t.log_channel
                .map_or("*Not set*".into(), |c| format!("<#{c}>")),
        ))
        .field(f("Advanced Logging", on(t.full_log)))
        .field(f("Anonymous Messaging", on(t.anonymous)))
        .field(f("Command Only", on(t.command_only)))
        .field(f("Ticket Creation", creation))
        .field(EmbedFieldBuilder::new(
            "Greeting Message",
            or_unset(clip(&t.greeting, 1000)),
        ))
        .field(EmbedFieldBuilder::new(
            "Closing Message",
            or_unset(clip(&t.closing, 1000)),
        ))
        .build()
}

/// `ticketsetup` makes the category and the log channel; `category [name]` only a new category.
async fn setup(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    t: &TicketsCfg,
    w: &str,
    args: &str,
) -> Result<()> {
    let ch = m.channel_id;
    let pfx = app.cfg(gid.get()).await?.misc.prefix.clone();
    let channels = &app.guild_info(gid).await?.channels;
    let exists = t.category.is_some_and(|c| channels.contains(&c));
    let name = if w == "category" && !args.is_empty() {
        args
    } else {
        "Tickets"
    };
    if name.chars().count() > 100 {
        return say(
            app,
            ch,
            error("The category name cannot be longer than 100 characters"),
        )
        .await;
    }
    if exists {
        let text = if w == "category" {
            "A ticket category already exists. Please delete that category and try again."
        } else {
            "The bot has already been set up."
        };
        return say(app, ch, error(text)).await;
    }
    let ows = overwrites(gid, app.me.get(), &staff_roles(app, gid, t).await?);
    let category = app
        .http
        .create_guild_channel(gid, name)
        .kind(ChannelType::GuildCategory)
        .permission_overwrites(&ows)
        .await?
        .model()
        .await?
        .id;
    let log = if w == "ticketsetup" {
        let c = app
            .http
            .create_guild_channel(gid, "ticket-logs")
            .kind(ChannelType::GuildText)
            .parent_id(category)
            .permission_overwrites(&ows)
            .await?
            .model()
            .await?
            .id;
        Some(c.to_string())
    } else {
        None
    };
    let saved = save(app, m, gid, |tk| {
        tk["enabled"] = json!(true);
        tk["category"] = json!(category.to_string());
        if let Some(l) = log {
            tk["log_channel"] = json!(l);
        }
    })
    .await?;
    if !saved {
        return Ok(());
    }
    if w == "category" {
        return say(app, ch, info("Successfully created the category.")).await;
    }
    let text = format!("Everything has been set up! Next up, you can give your staff access to ticket commands using `{pfx}accessrole [roles]` or a permit with Manage Tickets (the server owner, Extra Owners and Trusted Admins always have full access). You can also test things out by direct messaging me. Check out more information and configurations with `{pfx}help`.");
    say(app, ch, emb("Setup", &text, BLUE).build()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_is_hidden_from_everyone_but_bot_and_staff() {
        let o = overwrites(Id::new(9), 2, &[3]);
        assert_eq!(o[0].id.get(), 9);
        assert_eq!(o[0].deny, Permissions::VIEW_CHANNEL);
        assert_eq!(
            o.iter()
                .filter(|w| w.allow.contains(Permissions::VIEW_CHANNEL))
                .map(|w| w.id.get())
                .collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn channel_names_drop_punctuation_and_fall_back_to_the_id() {
        assert_eq!(channel_name("Tom.Bot!", Id::new(5)), "tombot");
        assert_eq!(channel_name("...", Id::new(5)), "5");
    }

    #[test]
    fn dm_commands_and_message_bodies() {
        let me = Id::new(7);
        assert_eq!(
            dm_command("w!New hi there", me),
            Some(("new".into(), "hi there"))
        );
        assert_eq!(
            dm_command("<@7> confirmation", me),
            Some(("confirmation".into(), ""))
        );
        assert_eq!(dm_command("hello", me), None);
        assert_eq!(body("w!send 123 hi there", me), "hi there");
        assert_eq!(body("w!switch hi", me), "hi");
        assert_eq!(body("plain text", me), "plain text");
    }

    #[test]
    fn history_names_the_speaker_of_relay_embeds() {
        assert_eq!(
            speaker("Message Received", None, Some("ann | 12")).as_deref(),
            Some("ann (User)")
        );
        assert_eq!(
            speaker("Message Sent", Some("bob (Anonymous)"), None).as_deref(),
            Some("bob (Staff)")
        );
        assert_eq!(speaker("New Ticket", None, None), None);
    }

    #[test]
    fn ids_take_mentions_and_numbers_only() {
        assert_eq!(ids("<@1> <@!2> <@&3> 4"), Ok(vec![1, 2, 3, 4]));
        assert_eq!(ids("bob"), Err(()));
        assert_eq!(ids(""), Ok(vec![]));
    }

    #[test]
    fn pings_cover_everyone_here_and_roles() {
        let t = TicketsCfg {
            ping_roles: vec![Id::new(9), Id::new(4)],
            ping_here: true,
            ..Default::default()
        };
        let (line, am) = pings(Id::new(9), &t);
        assert_eq!(line, "@everyone <@&4> @here");
        assert_eq!(am.roles, vec![Id::new(4)]);
        assert_eq!(am.parse, vec![MentionType::Everyone]);
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn one_open_ticket_per_member_and_enabled_servers_are_found() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let g = 4_100_000_000_000_000_000 + std::process::id() as u64;
        let gid = Id::new(g);
        let first = claim_number(&pool, gid, Id::new(1)).await.unwrap();
        assert!(first.is_some());
        assert_eq!(claim_number(&pool, gid, Id::new(1)).await.unwrap(), None);
        assert_eq!(
            claim_number(&pool, gid, Id::new(2)).await.unwrap(),
            first.map(|n| n + 1)
        );
        for (v, on) in [(1, false), (2, true)] {
            sqlx::query("INSERT INTO guild_config (guild_id, version, config) VALUES ($1, $2, $3)")
                .bind(g as i64)
                .bind(v)
                .bind(json!({"tickets": {"enabled": on}}))
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(ticket_guilds(&pool).await.unwrap().contains(&(g as i64)));
        sqlx::query("DELETE FROM tickets WHERE guild_id = $1")
            .bind(g as i64)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM guild_config WHERE guild_id = $1")
            .bind(g as i64)
            .execute(&pool)
            .await
            .unwrap();
    }
}
