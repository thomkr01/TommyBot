//! Thank-you message posted once when the bot joins a server, and data deletion after it leaves.
use anyhow::Result;
use tommybot_logging::emoji::YES;
use twilight_model::{
    channel::{
        message::component::{ActionRow, Button, ButtonStyle, Component},
        ChannelType,
    },
    gateway::payload::incoming::{GuildCreate, GuildDelete},
    guild::audit_log::AuditLogEventType,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::embed::EmbedBuilder;

use crate::app::App;

/// Only a join in the last two minutes counts, so the GuildCreate burst at startup stays quiet.
const FRESH_SECS: i64 = 120;

/// The dashboard routes by URL hash, so a path like `/guilds/<id>` would 404.
fn wizard_url(base: &str, guild: impl std::fmt::Display) -> String {
    format!("{}/#/{guild}/wizard", base.trim_end_matches('/'))
}

pub(crate) fn link(label: &str, url: String) -> Component {
    Component::Button(tommybot_logging::emoji::LINK.on(Button {
        id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style: ButtonStyle::Link,
        url: Some(url),
        sku_id: None,
    }))
}

/// Days a removed server's data is kept, in case the bot is added back (PRIVACY.md).
const KEEP_DAYS: i32 = 30;

/// Starts the 30-day countdown when the bot is removed; an outage (`unavailable`) is not a removal.
pub async fn on_guild_delete(app: &App, ev: &GuildDelete) -> Result<()> {
    if ev.unavailable != Some(true) {
        sqlx::query("INSERT INTO guild_left (guild_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(ev.id.get() as i64)
            .execute(&app.pool)
            .await?;
        crate::bansync::on_removed(app, ev.id).await?;
    }
    Ok(())
}

/// Starts the countdown for servers that removed the bot while it was offline. One shard, so
/// `Ready` lists every server the bot is in.
pub async fn on_ready(
    app: &App,
    ev: &twilight_model::gateway::payload::incoming::Ready,
) -> Result<()> {
    tracing::info!(guilds = ev.guilds.len(), "gateway ready");
    let ids: Vec<i64> = ev.guilds.iter().map(|g| g.id.get() as i64).collect();
    // Before the first await: a second `Ready` meanwhile adds to the floor, never replaces it.
    let floor = std::mem::take(&mut *app.audit_floor.lock().unwrap());
    sqlx::query(
        "INSERT INTO guild_left (guild_id) SELECT guild_id FROM guild_config \
         WHERE guild_id <> ALL($1) UNION SELECT guild_id FROM guild_owners WHERE guild_id <> ALL($1) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&ids)
    .execute(&app.pool)
    .await?;
    // Ban Sync groups skip servers in guild_left; their guild_owners rows stay, so an owner
    // change while the bot was out is caught at the next GuildCreate.
    // A new session (not a resume) or a restart gets no replay of the audit entries made in the
    // gap: anti-nuke reads them from the audit log (D20). The floor was noted before this task ran.
    let (todo, gone): (Vec<(u64, u64)>, Vec<_>) = floor
        .into_iter()
        .partition(|(g, _)| ids.contains(&(*g as i64)));
    {
        let mut pinned = app.audit_pinned.lock().unwrap();
        for (g, _) in gone {
            pinned.remove(&g);
        }
    }
    catch_up_retrying(app, todo).await;
    Ok(())
}

/// In the gateway loop, before the `Ready` task is spawned: where each guild's catch-up starts.
/// Taken later, a live entry right after `Ready` could raise it past the gap (D20).
/// A gap not read yet (its catch-up still retrying, or failed) keeps its older start, here and in
/// what `flush_audit` saves (`audit_pinned`), so neither a reconnect nor a restart skips it.
pub fn note_ready(app: &App) {
    let seen = app.last_audit.lock().unwrap().clone();
    let mut pinned = app.audit_pinned.lock().unwrap();
    let mut floor = app.audit_floor.lock().unwrap();
    for (g, last) in seen {
        let start = pinned.get(&g).map_or(last, |p| (*p).min(last));
        let f = floor.entry(g).or_insert(start);
        *f = (*f).min(start);
        pinned.insert(g, *f);
    }
}

/// True the first time an audit entry is seen this run, false for one already handled: the live
/// event and the catch-up can bring the same entry, and anti-nuke must count it once (D20).
/// Memory only; `done_audit` saves it for a restart after it was handled.
pub fn saw_audit(app: &App, guild: u64, id: u64) -> Option<InFlight<'_>> {
    let mut recent = app.audit_recent.lock().unwrap();
    let ids = recent.entry(guild).or_default();
    if ids.contains(&id) {
        return None;
    }
    // Larger than a whole catch-up plus the live entries meanwhile (CATCH_UP_MAX + 100).
    if ids.len() >= 2500 {
        ids.pop_front();
    }
    ids.push_back(id);
    app.audit_inflight
        .lock()
        .unwrap()
        .entry(guild)
        .or_default()
        .insert(id, std::time::Instant::now());
    let mut last = app.last_audit.lock().unwrap();
    let l = last.entry(guild).or_default();
    *l = (*l).max(id);
    Some(InFlight { app, guild, id })
}

/// An audit entry being handled: `flush_audit` saves nothing past it until this drops, when its
/// handling ends or its task panics.
pub struct InFlight<'a> {
    app: &'a App,
    guild: u64,
    id: u64,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Ok(mut m) = self.app.audit_inflight.lock() {
            if let Some(f) = m.get_mut(&self.guild) {
                f.remove(&self.id);
            }
        }
    }
}

/// After anti-nuke handled an entry: records it in `audit_handled`, so no catch-up runs it again
/// (a distinct row per entry, no shared row lock), then `done_audit`. Not recorded when
/// anti-nuke did nothing with it: anti-nuke off, or the bot's own entry.
pub async fn handled_audit(
    app: &App,
    guild: u64,
    e: &twilight_model::guild::audit_log::AuditLogEntry,
) {
    let id = e.id.get();
    let on = app.cfg(guild).await.is_ok_and(|c| c.antinuke.enabled);
    if !on || e.user_id == Some(app.me) {
        return done_audit(app, guild, id);
    }
    let kept = sqlx::query(
        "INSERT INTO audit_handled (guild_id, audit_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(guild as i64)
    .bind(id as i64)
    .execute(&app.pool)
    .await;
    if let Err(e) = kept {
        tracing::warn!(?e, "recording a handled audit entry failed");
    }
    done_audit(app, guild, id);
}

/// Queues a handled entry's id for `audit_seen`. The write is batched by
/// `flush_audit`, so a nuke's entries never wait on that row's lock (D20).
pub fn done_audit(app: &App, guild: u64, id: u64) {
    let mut m = app.audit_done.lock().unwrap();
    let d = m.entry(guild).or_default();
    *d = (*d).max(id);
}

/// Writes the queued last audit ids. A failed write is queued again; it only means a restart
/// reads from an older entry, which the catch-up handles.
pub async fn flush_audit(app: &App) {
    let done = std::mem::take(&mut *app.audit_done.lock().unwrap());
    for (guild, id) in done {
        // Never past an older entry still being handled, nor past an unread gap: a restart
        // reads from here, and `action_log.audit_id` skips what was already counted.
        // `InFlight` clears its id, a panic included; the expiry only bounds a leak.
        let inflight = app
            .audit_inflight
            .lock()
            .unwrap()
            .get_mut(&guild)
            .and_then(|f| {
                f.retain(|_, at| at.elapsed() < std::time::Duration::from_secs(1800));
                f.keys().next().copied()
            });
        let pinned = app.audit_pinned.lock().unwrap().get(&guild).copied();
        let safe = [Some(id), inflight.map(|i| i - 1), pinned]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(id);
        if safe < id {
            // Kept for the next flush, once the older entry is done or the gap read.
            done_audit(app, guild, id);
        }
        let id = safe;
        let kept = sqlx::query(
            "INSERT INTO audit_seen (guild_id, last_id) VALUES ($1, $2)
             ON CONFLICT (guild_id) DO UPDATE SET last_id = GREATEST(audit_seen.last_id, EXCLUDED.last_id)",
        )
        .bind(guild as i64)
        .bind(id as i64)
        .execute(&app.pool)
        .await;
        if let Err(e) = kept {
            tracing::warn!(?e, "saving the last audit entry failed");
            done_audit(app, guild, id);
        }
    }
}

/// Background: `flush_audit` every 2 s.
pub async fn flush_audit_loop(app: std::sync::Arc<App>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        flush_audit(&app).await;
    }
}

/// Startup, before the gateway: the last audit entry seen per guild before the restart, so the
/// first `Ready` reads what came in while the bot was down (D20).
pub async fn load_audit_seen(app: &App) -> Result<()> {
    let rows: Vec<(i64, i64)> = sqlx::query_as("SELECT guild_id, last_id FROM audit_seen")
        .fetch_all(&app.pool)
        .await?;
    app.last_audit
        .lock()
        .unwrap()
        .extend(rows.into_iter().map(|(g, l)| (g as u64, l as u64)));
    Ok(())
}

/// How many entries one catch-up reads at most. A nuke trips long before; past it, staff are told.
const CATCH_UP_MAX: usize = 1000;

/// `catch_up_audit` for each guild, the failed ones tried again 5 times over about 2.5 minutes.
/// When all fail, staff are told the gap was not checked, instead of a log line only.
async fn catch_up_retrying(app: &App, mut todo: Vec<(u64, u64)>) {
    for wait in [5, 10, 20, 40, 80, 0] {
        let mut failed = Vec::new();
        for (g, last) in todo {
            if let Err(e) = catch_up_audit(app, Id::new(g), last).await {
                tracing::warn!(?e, guild = g, "catching up on the audit log failed");
                failed.push((g, last));
            }
        }
        if failed.is_empty() {
            return;
        }
        if wait == 0 {
            for (g, _) in failed {
                // Declared unchecked: holding the saved id back for it would replay days of entries.
                app.audit_pinned.lock().unwrap().remove(&g);
                gap_alert(
                    app,
                    Id::new(g),
                    "TommyBot could not read the audit log after a restart or reconnect",
                )
                .await;
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        todo = failed;
    }
}

async fn gap_alert(app: &App, gid: Id<GuildMarker>, why: &str) {
    let log = tommybot_logging::Log::new("Anti Nuke", tommybot_logging::embed::ALERT)
        .text(&format!("**{why}.** Actions made while it was away were not all checked by anti-nuke. Check the audit log."));
    if let Err(e) =
        crate::events::send_log(app, gid.get(), tommybot_logging::LogKind::AntiNuke, log).await
    {
        tracing::warn!(?e, "audit gap alert failed");
    }
}

/// Runs anti-nuke on the entries after `last`, oldest first. Pages back from the newest with
/// `before`, which Discord always returns newest first, until it reaches `last` or
/// `CATCH_UP_MAX`; past the cap the newest entries are read and staff are told.
pub(crate) async fn catch_up_audit(app: &App, gid: Id<GuildMarker>, last: u64) -> Result<()> {
    let mut all = Vec::new();
    let mut before = None;
    let reached = loop {
        let mut req = app.http.audit_log(gid).limit(100);
        if let Some(b) = before {
            req = req.before(b);
        }
        let page = req.await?.model().await?.entries;
        let full = page.len() == 100;
        let oldest = page.iter().map(|e| e.id.get()).min();
        let done = !full || oldest.is_none_or(|o| o <= last);
        all.extend(page.into_iter().filter(|e| e.id.get() > last));
        if done {
            break true;
        }
        if all.len() >= CATCH_UP_MAX {
            break false;
        }
        before = oldest;
    };
    if !reached {
        tracing::warn!(
            guild = gid.get(),
            "more than {CATCH_UP_MAX} audit entries missed"
        );
        gap_alert(
            app,
            gid,
            &format!(
                "More than {CATCH_UP_MAX} audit log entries were made while TommyBot was away"
            ),
        )
        .await;
    }
    let entries = missed(all, last);
    // Handled before a crash or restart that lost the batched saved id; an entry counted by a
    // handling that crashed before it was recorded is in `action_log`.
    let ids: Vec<i64> = entries.iter().map(|e| e.id.get() as i64).collect();
    let handled: Vec<i64> = sqlx::query_scalar(
        "SELECT audit_id FROM audit_handled WHERE guild_id = $1 AND audit_id = ANY($2)
         UNION SELECT audit_id FROM action_log WHERE guild_id = $1 AND audit_id = ANY($2)",
    )
    .bind(gid.get() as i64)
    .bind(&ids)
    .fetch_all(&app.pool)
    .await?;
    let mut stale = 0;
    for mut e in entries {
        e.guild_id = Some(gid);
        let Some(_inflight) = saw_audit(app, gid.get(), e.id.get()) else {
            continue;
        };
        if handled.contains(&(e.id.get() as i64)) {
            done_audit(app, gid.get(), e.id.get());
            continue;
        }
        stale += usize::from(crate::antinuke::too_old(&e));
        if let Err(err) = crate::antinuke::on_replayed(app, &e).await {
            tracing::warn!(?err, "anti-nuke on a missed audit entry failed");
        }
        handled_audit(app, gid.get(), &e).await;
    }
    if stale > 0 {
        let why =
            format!("{stale} entries from the outage were older than an hour and were not counted");
        gap_alert(app, gid, &why).await;
    }
    app.audit_pinned.lock().unwrap().remove(&gid.get());
    Ok(())
}

/// The entries newer than `last`, oldest first, each once.
fn missed(
    mut entries: Vec<twilight_model::guild::audit_log::AuditLogEntry>,
    last: u64,
) -> Vec<twilight_model::guild::audit_log::AuditLogEntry> {
    entries.retain(|e| e.id.get() > last);
    entries.sort_by_key(|e| e.id);
    entries.dedup_by_key(|e| e.id);
    entries
}

/// Deletes the data of servers the bot left more than `KEEP_DAYS` ago, hourly.
pub async fn purge_left(app: std::sync::Arc<App>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
    let mut sticky_synced = std::collections::HashMap::new();
    loop {
        tick.tick().await;
        if let Err(e) = crate::sticky::sweep(&app, &mut sticky_synced).await {
            tracing::warn!(?e, "sticky roles sync and cleanup failed");
        }
        match tommybot_core::db::purge_left(&app.pool, KEEP_DAYS).await {
            Ok(ids) if !ids.is_empty() => tracing::info!(?ids, "purged data of removed servers"),
            Ok(_) => {}
            Err(e) => tracing::warn!(?e, "purging removed servers failed"),
        }
        // The insert trigger only prunes servers whose Join Gate still acts.
        let old = sqlx::query(
            "DELETE FROM joingate_actions WHERE created_at < now() - interval '30 days'",
        )
        .execute(&app.pool)
        .await;
        if let Err(e) = old {
            tracing::warn!(?e, "pruning old join gate records failed");
        }
        // Moderator stats keep one year (Thomas 2026-11-11).
        let old =
            sqlx::query("DELETE FROM mod_actions WHERE created_at < now() - interval '365 days'")
                .execute(&app.pool)
                .await;
        if let Err(e) = old {
            tracing::warn!(?e, "pruning old mod stats failed");
        }
        // Restart copies of the hour-long windows (N3).
        for q in [
            "DELETE FROM command_uses WHERE at < now() - interval '1 hour'",
            // A catch-up only reads past the saved id, which trails the handled entries by seconds.
            "DELETE FROM audit_handled WHERE at < now() - interval '1 day'",
            "DELETE FROM heat_panic WHERE until < extract(epoch FROM now())",
        ] {
            if let Err(e) = sqlx::query(q).execute(&app.pool).await {
                tracing::warn!(?e, q, "pruning restart windows failed");
            }
        }
        crate::raiders::sweep(&app).await;
    }
}

/// Members already in voice, so their next move is not logged or counted as a join. Called first
/// in the event's task, before any await, so voice updates handled after it are not wiped.
pub fn seed_voice(app: &App, ev: &GuildCreate) {
    if let GuildCreate::Available(g) = ev {
        let in_voice = g.voice_states.iter();
        let in_voice = in_voice.filter_map(|v| Some((v.user_id, v.channel_id?)));
        app.logger.lock().unwrap().seed_voice(g.id, in_voice);
    }
}

pub async fn on_guild_create(app: &App, ev: &GuildCreate) -> Result<()> {
    sqlx::query("DELETE FROM guild_left WHERE guild_id = $1")
        .bind(ev.id().get() as i64)
        .execute(&app.pool)
        .await?;
    let GuildCreate::Available(g) = ev else {
        return Ok(());
    };
    crate::bansync::owner_seen(app, g.id, g.owner_id).await?;
    let fresh = g
        .joined_at
        .is_some_and(|t| t.as_secs() > crate::app::now_secs() - FRESH_SECS);
    if !fresh {
        return Ok(());
    }
    let (new,): (bool,) = sqlx::query_as(
        "WITH i AS (INSERT INTO welcomed_guilds (guild_id) VALUES ($1)
                    ON CONFLICT DO NOTHING RETURNING 1)
         SELECT EXISTS (SELECT 1 FROM i)",
    )
    .bind(g.id.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    let channel = g.system_channel_id.or_else(|| {
        g.channels
            .iter()
            .find(|c| c.kind == ChannelType::GuildText)
            .map(|c| c.id)
    });
    if !new {
        return Ok(());
    }

    let dashboard = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let mut links = vec![];
    if let Some(url) = &dashboard {
        links.push(link("Setup Wizard", wizard_url(url, g.id)));
    }
    if let Some(url) = std::env::var("SUPPORT_URL").ok().filter(|u| !u.is_empty()) {
        links.push(link("Support", url));
    }
    let wizard = match &dashboard {
        Some(url) => format!(
            "You can set the bot up in the [dashboard]({}).",
            wizard_url(url, g.id)
        ),
        None => "Ask the bot owner for the dashboard link to set it up.".into(),
    };
    let embed = EmbedBuilder::new()
        .title("Thank you for choosing TommyBot!")
        .description(format!(
            "{YES} `TommyBot` has been added to `{}` successfully!\n\n{wizard} Every feature is free.",
            g.name
        ))
        .build();
    let embeds = [embed];
    // Discord rejects an empty action row.
    let rows: Vec<Component> = if links.is_empty() {
        vec![]
    } else {
        vec![Component::ActionRow(ActionRow {
            id: None,
            components: links,
        })]
    };
    // Tell whoever added the bot, and fall back to the server when their DMs are closed.
    if let Some(adder) = bot_adder(app, g.id, app.me).await {
        let dm = async {
            let ch = app
                .http
                .create_private_channel(adder)
                .await?
                .model()
                .await?;
            app.http
                .create_message(ch.id)
                .embeds(&embeds)
                .components(&rows)
                .await?;
            anyhow::Ok(())
        };
        if dm.await.is_ok() {
            return Ok(());
        }
    }
    let Some(channel) = channel else {
        return Ok(());
    };
    app.http
        .create_message(channel)
        .embeds(&embeds)
        .components(&rows)
        .await?;
    Ok(())
}

/// Who added `bot`, from the audit log; None when there is no entry or the lookup fails.
pub async fn bot_adder(
    app: &App,
    gid: Id<GuildMarker>,
    bot: Id<UserMarker>,
) -> Option<Id<UserMarker>> {
    find_bot_adder(app, gid, bot).await.ok().flatten()
}

/// [`bot_adder`] for the unauthorized-bot filter (D59): a failed lookup or a missing entry (the
/// audit log can lag the join) is tried once more after 1.5 s; Err when that fails too, and the
/// caller blocks the bot.
pub async fn bot_adder_retry(
    app: &App,
    gid: Id<GuildMarker>,
    bot: Id<UserMarker>,
) -> Result<Id<UserMarker>> {
    if let Ok(Some(a)) = find_bot_adder(app, gid, bot).await {
        return Ok(a);
    }
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    find_bot_adder(app, gid, bot)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no audit entry names who added the bot"))
}

/// One audit lookup, bounded: it runs on the join path (PITFALLS: a REST call on the join path).
async fn find_bot_adder(
    app: &App,
    gid: Id<GuildMarker>,
    bot: Id<UserMarker>,
) -> Result<Option<Id<UserMarker>>> {
    let read = async {
        let log = app
            .http
            .audit_log(gid)
            .action_type(AuditLogEventType::BotAdd)
            .limit(10)
            .await?
            .model()
            .await?;
        anyhow::Ok(
            log.entries
                .iter()
                .find(|e| e.target_id.is_some_and(|t| t.get() == bot.get()))
                .and_then(|e| e.user_id),
        )
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), read).await?
}

#[cfg(test)]
mod tests {
    #[test]
    fn missed_entries_replay_oldest_first() {
        let e = |id: u64| -> twilight_model::guild::audit_log::AuditLogEntry {
            serde_json::from_value(
                serde_json::json!({"action_type": 22, "changes": [], "id": id.to_string()}),
            )
            .unwrap()
        };
        let got: Vec<u64> = super::missed(vec![e(9), e(5), e(7), e(3)], 5)
            .iter()
            .map(|e| e.id.get())
            .collect();
        assert_eq!(got, [7, 9]);
    }

    #[test]
    fn wizard_link_uses_hash_route() {
        assert_eq!(
            super::wizard_url("https://d.example/", 5),
            "https://d.example/#/5/wizard"
        );
    }
}
