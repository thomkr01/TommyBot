//! Tickets: a member DMs the bot, picks a server they share with it, and the bot opens a channel
//! in that server's ticket category. Messages are relayed both ways until staff close the ticket,
//! which logs it (with the whole conversation as a file) to the ticket log.
use anyhow::Result;
use serde_json::{json, Value};
use tommybot_core::{permits, sections::TicketsCfg};
use tommybot_logging::embed::{avatar, clip, Log, BLUE, GREEN, ORANGE, RED};
use tommybot_logging::emoji::{TICKET, TICKET_BLOCK, TICKET_CLOSE, TICKET_NEW, TICKET_REPLY};
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
pub(crate) struct Server {
    pub(crate) id: Id<GuildMarker>,
    pub(crate) name: String,
    pub(crate) icon: Option<String>,
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

mod commands;
mod dm;
mod msgs;
mod staff;

pub(crate) use msgs::*;

pub use commands::{blocked_page, on_message, snippets_page};
pub use dm::{handle, on_dm};

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
