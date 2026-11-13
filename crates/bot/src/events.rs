//! Gateway event dispatch: logging first, then commands, automod and anti-nuke.
use anyhow::Result;
use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Instant,
};
use tommybot_logging::{embed::RED, Log, LogConfig, LogKind, LogMessage};
use twilight_gateway::Event;
use twilight_model::{
    channel::{
        message::{
            component::{ActionRow, Button, ButtonStyle, Component},
            AllowedMentions, Embed,
        },
        Channel,
    },
    guild::Permissions,
    http::attachment::Attachment,
    id::{
        marker::{ChannelMarker, GuildMarker, MessageMarker, RoleMarker},
        Id,
    },
};

use crate::{
    antinuke, app::App, automod, commands, confirm, metrics, prefix, reports, slash, tickets,
    verify,
};

fn guild_of(ev: &Event) -> Option<u64> {
    let g = match ev {
        Event::MessageCreate(m) => m.guild_id,
        Event::MessageUpdate(m) => m.0.guild_id,
        Event::MessageDelete(m) => m.guild_id,
        Event::MessageDeleteBulk(m) => m.guild_id,
        Event::MemberAdd(m) => Some(m.guild_id),
        Event::MemberRemove(m) => Some(m.guild_id),
        Event::GuildAuditLogEntryCreate(e) => e.0.guild_id,
        Event::AutoModerationActionExecution(e) => Some(e.guild_id),
        Event::VoiceStateUpdate(v) => v.0.guild_id,
        _ => None,
    };
    g.map(|g| g.get())
}

/// Channel and role updates drop the cached answer sooner.
const PUBLIC_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a member can view a channel: base permissions, then the channel's (allow, deny)
/// overwrites in order (@everyone first, then the role's).
fn sees(base: Permissions, overwrites: &[(Permissions, Permissions)]) -> bool {
    base.contains(Permissions::ADMINISTRATOR)
        || overwrites
            .iter()
            .fold(base, |p, (allow, deny)| (p - *deny) | *allow)
            .contains(Permissions::VIEW_CHANNEL)
}

/// Logs carry deleted messages and staff actions, so they never go where @everyone, or a main
/// role every member gets, can read them.
async fn public(app: &App, channel: Id<ChannelMarker>) -> Result<bool> {
    let cached = app.log_public.lock().unwrap().get(&channel).copied();
    if let Some((p, _)) = cached.filter(|(_, at)| at.elapsed() < PUBLIC_TTL) {
        return Ok(p);
    }
    let c = app.http.channel(channel).await?.model().await?;
    let p = match c.guild_id {
        Some(gid) => {
            // By the permissions a lock saved: the channel is public again once it ends (D62).
            let info = crate::lockdown::unlocked_info(app, gid).await?;
            let main = app.cfg(gid.get()).await?.lockdown.main_roles.clone();
            let perms = |r: u64| {
                Permissions::from_bits_truncate(
                    info.role_perms.get(&Id::new(r)).copied().unwrap_or(0),
                )
            };
            let ows = c.permission_overwrites.unwrap_or_default();
            let ow = |r: u64| {
                ows.iter()
                    .find(|o| o.id.get() == r)
                    .map_or((Permissions::empty(), Permissions::empty()), |o| {
                        (o.allow, o.deny)
                    })
            };
            let g = gid.get();
            sees(perms(g), &[ow(g)])
                || main
                    .iter()
                    .any(|r| sees(perms(g) | perms(r.get()), &[ow(g), ow(r.get())]))
        }
        None => false,
    };
    app.log_public
        .lock()
        .unwrap()
        .insert(channel, (p, Instant::now()));
    Ok(p)
}

/// A held-back log is not only in the container log: the owner hears once a day per channel.
async fn warn_public(app: &App, channel: Id<ChannelMarker>) {
    static LAST: std::sync::Mutex<std::collections::BTreeMap<u64, i64>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    if !crate::reply::alert_due(
        &mut LAST.lock().unwrap(),
        channel.get(),
        crate::app::now_secs(),
        86_400,
    ) {
        return;
    }
    let gid = match app.http.channel(channel).await {
        Ok(r) => r.model().await.ok().and_then(|c| c.guild_id),
        Err(_) => None,
    };
    if let Some(gid) = gid {
        let text = format!("TommyBot did not post a log in <#{channel}>: every member can read that channel. Make it private, or press **Make private** under the security score on the dashboard.");
        crate::reply::dm_owner(app, gid, &text).await;
    }
}

/// A link button row under a log, or nothing.
fn link_row(link: Option<&(&str, String)>) -> Vec<Component> {
    link.map(|(label, url)| {
        Component::ActionRow(ActionRow {
            id: None,
            components: vec![Component::Button(tommybot_logging::emoji::LINK.on(
                Button {
                    id: None,
                    custom_id: None,
                    disabled: false,
                    emoji: None,
                    label: Some((*label).into()),
                    style: ButtonStyle::Link,
                    url: Some(url.clone()),
                    sku_id: None,
                },
            ))],
        })
    })
    .into_iter()
    .collect()
}

/// Logged text is user-controlled, so it is sent with mentions disabled.
async fn send(
    app: &App,
    channel: Id<ChannelMarker>,
    pings: &str,
    embed: &Embed,
    rows: &[Component],
    mentions: &AllowedMentions,
) -> Result<Id<MessageMarker>> {
    if public(app, channel).await? {
        warn_public(app, channel).await;
        anyhow::bail!(
            "not logging to {channel}: @everyone can read it, pick a private log channel"
        );
    }
    Ok(app
        .http
        .create_message(channel)
        .content(pings)
        .embeds(std::slice::from_ref(embed))
        .components(rows)
        .allowed_mentions(Some(mentions))
        .await?
        .model()
        .await?
        .id)
}

async fn send_entry(app: &App, m: &LogMessage) -> Result<()> {
    let mentions = AllowedMentions::default();
    let rows = link_row(m.link.as_ref());
    let id = send(app, m.channel_id, "", &m.embed, &rows, &mentions).await?;
    if let Some((name, text)) = &m.file {
        let file = Attachment::from_bytes(name.clone(), text.as_bytes().to_vec(), 0);
        reply_file(app, m.channel_id, id, file).await?;
    }
    Ok(())
}

/// Discord shows attachments above the embed, so the file is its own message below it, replying to it.
async fn reply_file(
    app: &App,
    channel: Id<ChannelMarker>,
    to: Id<MessageMarker>,
    file: Attachment,
) -> Result<Id<MessageMarker>> {
    Ok(app
        .http
        .create_message(channel)
        .reply(to)
        .fail_if_not_exists(false)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .attachments(&[file])
        .await?
        .model()
        .await?
        .id)
}

/// A finished log embed (ticket logs) in a private channel, with an optional file below it.
pub(crate) async fn send_private(
    app: &App,
    channel: Id<ChannelMarker>,
    embed: &Embed,
    file: Option<Attachment>,
) -> Result<(Id<MessageMarker>, Option<Id<MessageMarker>>)> {
    let id = send(app, channel, "", embed, &[], &AllowedMentions::default()).await?;
    let file = match file {
        Some(f) => Some(reply_file(app, channel, id, f).await?),
        None => None,
    };
    Ok((id, file))
}

/// Every log sent here ends with the date and time it happened.
pub async fn send_log(app: &App, guild: u64, kind: LogKind, embed: Log) -> Result<()> {
    match app.cfg(guild).await?.logging.target(kind, None) {
        Some(ch) => send(
            app,
            ch,
            "",
            &embed.time().build(),
            &[],
            &AllowedMentions::default(),
        )
        .await
        .map(drop),
        None => Ok(()),
    }
}

/// Like [`send_log`] but pings the given roles (panic mode warns staff roles).
pub async fn send_log_pinging(
    app: &App,
    guild: u64,
    kind: LogKind,
    embed: Log,
    roles: &[Id<RoleMarker>],
) -> Result<()> {
    let Some(ch) = app.cfg(guild).await?.logging.target(kind, None) else {
        return Ok(());
    };
    let pings: String = roles.iter().map(|r| format!("<@&{r}> ")).collect();
    let mentions = AllowedMentions {
        roles: roles.to_vec(),
        ..Default::default()
    };
    send(
        app,
        ch,
        pings.trim_end(),
        &embed.time().build(),
        &[],
        &mentions,
    )
    .await
    .map(drop)
}

/// Like [`send_log`] with a link button under the embed.
pub async fn send_log_link(
    app: &App,
    guild: u64,
    kind: LogKind,
    embed: Log,
    link: (&str, String),
) -> Result<()> {
    send_log_rows(app, guild, kind, embed, &link_row(Some(&link))).await
}

/// Like [`send_log`] with button rows under the embed.
pub async fn send_log_rows(
    app: &App,
    guild: u64,
    kind: LogKind,
    embed: Log,
    rows: &[Component],
) -> Result<()> {
    let Some(ch) = app.cfg(guild).await?.logging.target(kind, None) else {
        return Ok(());
    };
    let mentions = AllowedMentions::default();
    send(app, ch, "", &embed.time().build(), rows, &mentions)
        .await
        .map(drop)
}

/// A deleted log channel leaves the config, so its logs reach the main log channel, which says so.
async fn forget_log_channel(app: &App, gid: Id<GuildMarker>, c: &Channel) -> Result<()> {
    if !app.cfg(gid.get()).await?.logging.clone().forget(c.id) {
        return Ok(());
    }
    let saved = crate::settings::save(app, gid, app.me.get(), &["logging"], |doc| {
        let mut log: LogConfig =
            serde_json::from_value(doc["logging"].clone()).map_err(|e| e.to_string())?;
        log.forget(c.id);
        // Merged in, so keys LogConfig does not model (the dashboard's config_changes) survive.
        let new = serde_json::to_value(log).map_err(|e| e.to_string())?;
        if let (Some(old), serde_json::Value::Object(new)) = (doc["logging"].as_object_mut(), new) {
            old.extend(new);
        }
        Ok(())
    })
    .await?;
    saved.map_err(anyhow::Error::msg)?;
    let name = c.name.as_deref().unwrap_or("unknown");
    let embed = Log::new("Log Channel Deleted", RED)
        .line("Channel", &format!("#{name} (`{}`)", c.id))
        .text("Its logs now go to the main log channel. Pick a new channel on the dashboard.");
    send_log(app, gid.get(), LogKind::Server, embed).await
}

/// Sends held delete logs once their wait is over. `handle` only flushes when an event arrives,
/// so a quiet guild would otherwise never log its deletes.
pub async fn flush_logs(app: Arc<App>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        let due = app.logger.lock().unwrap().flush(Instant::now());
        for m in due {
            if let Err(e) = send_entry(&app, &m).await {
                tracing::warn!(?e, channel = %m.channel_id, "log send failed");
            }
        }
    }
}

/// What the raw gateway frame carries that twilight's model drops.
#[derive(Default)]
pub struct Extra {
    /// Discord's "likely spammer" user flag on a member join.
    pub spammer: bool,
}

pub async fn handle(app: Arc<App>, ev: Event, extra: Extra) {
    metrics::EVENTS.fetch_add(1, Relaxed);
    metrics::IN_FLIGHT.fetch_add(1, Relaxed);
    // Every log line from the handler carries the server id, so a failed Discord call can be traced to it.
    let span = tracing::info_span!("event", guild = ev.guild_id().map(|g| g.get()));
    if let Err(e) = tracing::Instrument::instrument(run(&app, &ev, extra), span).await {
        tracing::warn!(?e, kind = ?ev.kind(), guild = ev.guild_id().map(|g| g.get()), "event handler failed");
        metrics::EVENT_ERRORS.fetch_add(1, Relaxed);
    }
    metrics::IN_FLIGHT.fetch_sub(1, Relaxed);
}

/// Last step before exit: lets running handlers finish (strikes and locks are written as they
/// happen, so nothing else is pending), sends the held logs and closes the pool.
pub async fn shutdown(app: &App) {
    for _ in 0..50 {
        if metrics::IN_FLIGHT.load(Relaxed) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let held = app
        .logger
        .lock()
        .unwrap()
        .flush(Instant::now() + std::time::Duration::from_secs(3600));
    for m in held {
        if let Err(e) = send_entry(app, &m).await {
            tracing::warn!(?e, "log send failed during shutdown");
        }
    }
    app.pool.close().await;
}

async fn run(app: &App, ev: &Event, extra: Extra) -> Result<()> {
    // Before any await: each event runs in its own task, so an await here would let a later voice
    // update of the same member (or one after a GuildCreate) take the logger lock first.
    match ev {
        Event::GuildCreate(g) => {
            crate::welcome::seed_voice(app, g);
            if let twilight_model::gateway::payload::incoming::GuildCreate::Available(g) = &**g {
                for c in &g.channels {
                    app.note_category(c.id.get(), c.parent_id.map(|p| p.get()), true);
                }
            }
        }
        // Before any await too: the delete of a channel just moved must see the move (D4).
        Event::ChannelCreate(c) => {
            app.note_category(c.id.get(), c.parent_id.map(|p| p.get()), true)
        }
        Event::ChannelUpdate(c) => {
            app.note_category(c.id.get(), c.parent_id.map(|p| p.get()), false)
        }
        Event::MemberUpdate(m) => app.forget_level(m.guild_id.get(), m.user.id.get()),
        Event::MemberRemove(m) => app.forget_level(m.guild_id.get(), m.user.id.get()),
        _ => {}
    }
    // A voice member's channel before this event, read as the update is recorded.
    let voice = match ev {
        Event::VoiceStateUpdate(v) => v.0.guild_id.map(|g| {
            let old = app
                .logger
                .lock()
                .unwrap()
                .voice_update(g, v.0.user_id, v.0.channel_id);
            (v.0.user_id, (old, v.0.channel_id))
        }),
        _ => None,
    };
    let voice_old = voice.and_then(|v| v.1 .0);
    if let Some(guild) = guild_of(ev) {
        let cfg = app.cfg(guild).await?;
        let msgs = {
            let mut logger = app.logger.lock().unwrap();
            match voice {
                Some((user, moved)) => {
                    logger.handle_voice(&cfg.logging, user, moved, Instant::now())
                }
                None => logger.handle(&cfg.logging, ev, Instant::now()),
            }
        };
        for m in msgs {
            if let Err(e) = send_entry(app, &m).await {
                tracing::warn!(?e, channel = %m.channel_id, "log send failed");
            }
        }
    }
    match ev {
        // Webhook and app posts count as bots but still reach automod (never commands).
        Event::MessageCreate(m)
            if !m.author.bot || m.webhook_id.is_some() || m.interaction_metadata.is_some() =>
        {
            if crate::catch::on_message(app, m).await? {
                return Ok(());
            }
            if let (false, None) = (m.author.bot, m.guild_id) {
                return tickets::on_dm(app, m).await;
            }
            if let (false, Some(gid)) = (m.author.bot, m.guild_id) {
                let pfx = app.cfg(gid.get()).await?.misc.prefix.clone();
                let rest = m.content.strip_prefix(&pfx).filter(|r| !r.is_empty());
                if tickets::on_message(app, m, gid, rest).await? {
                    // Relayed or a ticket command; automod still sees it below.
                } else if let Some(rest) = rest {
                    // An ignored command still goes through automod below.
                    if !commands::ignored(app, m, gid, rest).await?
                        && !crate::general::try_run(app, m, rest).await?
                        && !crate::settings::try_run(app, m, rest).await?
                    {
                        if let Some(parsed) = prefix::parse(rest) {
                            commands::run(app, m, parsed).await?;
                        }
                    }
                }
            }
            automod::on_message(app, m).await?;
        }
        // A webhook or app can change its text or files without edited_timestamp (a deferred fill,
        // a button update): new text or files are filtered (unseen files scanned and QR-read),
        // the same ones never twice.
        Event::MessageUpdate(m) if app_post(app, &m.0) => {
            if automod::new_text(&m.0) {
                automod::on_edit(app, &m.0).await?;
            } else if !m.embeds.is_empty() {
                scan_preview(app, &m.0).await?;
            }
        }
        // Discord also sends updates for link previews; only real edits are checked again.
        Event::MessageUpdate(m) if m.edited_timestamp.is_some() && !m.author.bot => {
            automod::on_edit(app, &m.0).await?;
        }
        // A link preview or GIF arrived after the post: only the image scan looks at it.
        Event::MessageUpdate(m)
            if m.edited_timestamp.is_none() && !m.embeds.is_empty() && !m.author.bot =>
        {
            scan_preview(app, &m.0).await?;
        }
        Event::RoleCreate(r) => app.invalidate_guild(r.guild_id),
        Event::RoleUpdate(r) => {
            app.invalidate_guild(r.guild_id);
            app.log_public.lock().unwrap().clear();
        }
        Event::RoleDelete(r) => app.invalidate_guild(r.guild_id),
        Event::ChannelCreate(c) => {
            app.note_parent(&c.0);
            c.guild_id.into_iter().for_each(|g| app.invalidate_guild(g));
            if let Err(e) = crate::setup::enforce_quarantine(app, c).await {
                tracing::warn!(?e, channel = %c.id, "quarantine overwrite failed");
            }
        }
        Event::ChannelUpdate(c) => {
            app.log_public.lock().unwrap().remove(&c.id);
            c.guild_id.into_iter().for_each(|g| app.invalidate_guild(g));
            if let Err(e) = crate::setup::enforce_quarantine(app, c).await {
                tracing::warn!(?e, channel = %c.id, "quarantine overwrite failed");
            }
            // A slowmode staff change in Discord's own UI while an auto slowmode starts wins.
            if let Err(e) = crate::slowmode::channel_updated(app, c.id.get()).await {
                tracing::warn!(?e, channel = %c.id, "checking the auto slowmode failed");
            }
            crate::catch::keep_top(app, c).await?;
        }
        Event::ChannelDelete(c) => {
            app.note_parent(&c.0);
            tickets::on_channel_delete(app, c.id).await?;
            if let Err(e) = verify::deleted::on_channel_delete(app, c).await {
                tracing::warn!(?e, "remaking the verification channel failed");
            }
            if let Some(g) = c.guild_id {
                app.invalidate_guild(g);
                forget_log_channel(app, g, &c.0).await?;
            }
        }
        Event::GuildUpdate(g) => {
            app.invalidate_guild(g.id);
            crate::bansync::on_owner(app, g.id, g.owner_id).await?
        }
        Event::GuildCreate(g) => crate::welcome::on_guild_create(app, g).await?,
        Event::GuildDelete(g) => crate::welcome::on_guild_delete(app, g).await?,
        Event::Ready(r) => crate::welcome::on_ready(app, r).await?,
        Event::MemberUpdate(m) if m.user.id == app.me => app.invalidate_guild(m.guild_id),
        Event::MemberUpdate(m) => {
            if let Err(e) = antinuke::enforce_permits(app, m).await {
                tracing::warn!(?e, "enforcing permits failed");
            }
            if let Err(e) = crate::timeout_roles::on_member_update(app, m).await {
                tracing::warn!(?e, "removing main roles on timeout failed");
            }
            if let Err(e) = verify::on_member_update(app, m).await {
                tracing::warn!(?e, "pausing the verification time limit failed");
            }
            if let Err(e) = crate::sticky::on_member_update(app, m).await {
                tracing::warn!(?e, "saving sticky roles failed");
            }
            if !crate::joingate::on_member_update(app, m).await? {
                let (user, nick) = (&m.user, m.nick.as_deref());
                let r = crate::dehoist::check(app, m.guild_id, user, nick, &m.roles).await;
                if let Err(e) = r {
                    tracing::warn!(?e, "auto-dehoist failed");
                }
            }
        }
        Event::MemberAdd(m) => crate::joingate::on_join(app, m, extra.spammer).await?,
        Event::MemberRemove(m) => crate::sticky::on_leave(app, m.guild_id, m.user.id).await?,
        Event::BanAdd(b) => crate::sticky::on_ban(app, b.guild_id, b.user.id).await?,
        Event::InteractionCreate(i) => {
            if !verify::handle(app, &i.0).await?
                && !tickets::handle(app, &i.0).await?
                && !confirm::handle_confirm(app, &i.0).await?
                && !reports::handle(app, &i.0).await?
                && !crate::appeals::handle(app, &i.0).await?
                && !crate::settings::handle(app, &i.0).await?
                && !crate::whitelist::handle(app, &i.0).await?
                && !crate::bansync::handle(app, &i.0).await?
                && !crate::raiders::handle(app, &i.0).await?
                && !crate::nsfw::handle(app, &i.0).await?
            {
                slash::handle(app, &i.0).await?
            }
        }
        // Anti-nuke first: the verification check can wait for the gateway delete.
        Event::GuildAuditLogEntryCreate(e) => {
            if let Some(g) = e.0.guild_id {
                crate::welcome::saw_audit(app, g.get(), e.0.id.get()).await;
            }
            let nuke = antinuke::on_audit(app, &e.0).await;
            let ban = crate::bansync::on_ban(app, &e.0).await;
            let unban = crate::bansync::on_unban(app, &e.0).await;
            let allow = crate::raiders::on_unban(app, &e.0).await;
            if let Err(err) = verify::deleted::on_audit(app, &e.0).await {
                tracing::warn!(?err, "verification channel delete check failed");
            }
            // Stats are a side effect: their failure never stops protection.
            if let Err(err) = crate::modstats::on_audit(app, &e.0).await {
                tracing::warn!(?err, "recording a mod action failed");
            }
            nuke.and(ban).and(unban).and(allow)?
        }
        // Voice raid first: the anti-nuke recheck can wait out its fetch gap.
        Event::VoiceStateUpdate(v) => {
            if let Err(e) = crate::voice_raid::on_voice(app, &v.0, voice_old).await {
                tracing::warn!(?e, "voice raid check failed");
            }
            crate::voice_nuke::on_voice(app, &v.0, voice_old).await
        }
        Event::AutoModerationActionExecution(e) => crate::native::on_native_hit(app, e).await?,
        Event::ReactionAdd(r) => crate::activity::on_reaction(app, r).await?,
        Event::ThreadCreate(t) => crate::activity::on_thread(app, &t.0).await?,
        Event::ThreadUpdate(t) => crate::activity::on_thread_update(app, &t.0).await?,
        Event::ThreadListSync(s) => s
            .threads
            .iter()
            .filter_map(|t| Some((t.id, t.parent_id?)))
            .for_each(|(t, p)| app.note_thread(t, p)),
        _ => {}
    }
    Ok(())
}

/// A webhook's or another app's message (not TommyBot's own).
fn app_post(app: &App, m: &twilight_model::channel::Message) -> bool {
    m.author.id != app.me && automod::app_message(m)
}

/// Only the image scan reads a preview update.
async fn scan_preview(app: &App, m: &twilight_model::channel::Message) -> Result<()> {
    if let Some(gid) = m.guild_id {
        let am = &app.cfg(gid.get()).await?.automod;
        if am.enabled && am.filters.nsfw_images.enabled {
            app.nsfw.enqueue(m, !automod::files_can_be_new(m), false);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_follows_base_and_overwrites() {
        let (view, none) = (Permissions::VIEW_CHANNEL, Permissions::empty());
        assert!(sees(view, &[]));
        assert!(!sees(view, &[(none, view)]));
        assert!(!sees(none, &[]));
        assert!(sees(none, &[(view, none)]));
        assert!(sees(Permissions::ADMINISTRATOR, &[(none, view)]));
        // Hidden from @everyone but opened for a main role.
        assert!(sees(view, &[(none, view), (view, none)]));
    }
}
