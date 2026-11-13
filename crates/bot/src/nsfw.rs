//! NSFW image scan (NSFW card, off by default). Posted images, link previews and GIF previews are
//! scored by a small model that runs inside the bot (GantMan nsfw_model, MobileNetV2, MIT; see
//! `models/README.md`). Images are only held in memory; `nsfw_scans` keeps a hash and the scores.
//!
//! Two queue lanes: live (new posts, previews, edits) always goes first, and a live job that waited
//! too long moves to the backlog (the catch-up sweep's lane) instead of holding up newer posts.
use crate::app::App;
use crate::events::send_log_rows;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tommybot_core::limits::NSFW_THRESHOLD;
use tommybot_logging::{
    embed::{button, ALERT},
    Log, LogKind,
};
use tract_onnx::prelude::*;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::{
        message::component::{ActionRow, ButtonStyle, Component},
        ChannelType, Message,
    },
    guild::PartialMember,
    id::{marker::GuildMarker, Id},
};

static MODEL_BYTES: &[u8] = include_bytes!("../../../models/nsfw.onnx");
const SIDE: usize = 224;
/// Classes in the model's output order.
const CLASSES: [&str; 5] = ["drawings", "hentai", "neutral", "porn", "sexy"];
const MAX_BYTES: usize = 8 << 20;
const PER_MESSAGE: usize = 4;
const LIVE_CAP: usize = 256;
const BACKLOG_CAP: usize = 2_000;
/// A live job older than this goes to the backlog, so a burst never leaves new posts waiting.
const LIVE_MAX_AGE_MS: i64 = 120_000;
const CACHE_CAP: usize = 20_000;
/// Catch-up sweep: how far back, how many messages per channel, and the pause between channels.
const SWEEP_MS: i64 = 24 * 3_600_000;
const SWEEP_PER_CHANNEL: u16 = 100;
const SWEEP_GAP: Duration = Duration::from_secs(2);
pub const BUTTON: &str = "nsfw:";

type Model = TypedSimplePlan;

pub struct Job {
    msg: Box<Message>,
    urls: Vec<String>,
    at_ms: i64,
}

#[derive(Default)]
pub struct Scanner {
    /// Attachments already queued: an edit of a webhook or app message rescans only new files.
    seen: crate::automod::Seen,
    /// (live, backlog).
    lanes: Mutex<(VecDeque<Job>, VecDeque<Job>)>,
    wake: tokio::sync::Notify,
    /// Image hash -> scores, shared by every server (the threshold is applied per server).
    cache: Mutex<HashMap<[u8; 32], [f32; 5]>>,
    model: OnceLock<Option<Arc<Model>>>,
}

/// Porn and hentai count fully, sexy half, so a swimsuit photo alone does not reach a high threshold.
pub fn score(s: &[f32; 5]) -> f32 {
    (s[1] + s[3] + 0.5 * s[4]).min(1.0)
}

/// Only Discord's own media proxy: never the poster's server (no SSRF, and the bot's address stays hidden).
fn discord_media(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or("");
    u.scheme() == "https"
        && (host == "media.discordapp.net"
            || host == "cdn.discordapp.com"
            || (host.starts_with("images-ext-") && host.ends_with(".discordapp.net")))
}

/// Image URLs worth scanning: attachments (not on an edit, which cannot add any) and embed images,
/// also of a forwarded message.
fn image_urls(m: &Message, edit: bool) -> Vec<String> {
    let files = crate::automod::attachments(m)
        .filter(|_| !edit)
        .filter(|a| a.size <= MAX_BYTES as u64)
        .filter(|a| {
            a.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
        })
        .map(|a| a.proxy_url.clone());
    let forwarded = m.message_snapshots.iter().flat_map(|s| &s.message.embeds);
    let embeds = m.embeds.iter().chain(forwarded).flat_map(|e| {
        [
            e.image.as_ref().and_then(|i| i.proxy_url.clone()),
            e.thumbnail.as_ref().and_then(|t| t.proxy_url.clone()),
        ]
        .into_iter()
        .flatten()
    });
    let mut seen = HashSet::new();
    files
        .chain(embeds)
        .filter(|u| discord_media(u) && seen.insert(u.clone()))
        .take(PER_MESSAGE)
        .collect()
}

/// Asks the proxy for a small copy; the scan resizes again, so a proxy that ignores it only costs bandwidth.
fn small(url: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}width={SIDE}&height={SIDE}")
}

impl Scanner {
    /// Queues the message's images (the caller checked the filter is on). Never waits.
    pub fn enqueue(&self, m: &Message, edit: bool, backlog: bool) {
        // Files are marked once their scan row is saved (in `scan`).
        let fresh =
            crate::automod::attachments(m).any(|a| !self.seen.contains((m.id.get(), a.id.get())));
        let urls = image_urls(m, edit || !fresh);
        if urls.is_empty() {
            return;
        }
        // A user-installed app's reply is the member's post, as in automod. A server-installed
        // app's stays exempt, but its hit is logged under the member who ran the command.
        let msg = match (
            crate::automod::as_app_user(m),
            m.interaction_metadata.as_deref(),
        ) {
            (Some(u), _) => u,
            (None, Some(meta)) => Message {
                author: meta.user.clone(),
                member: None,
                ..m.clone()
            },
            (None, None) => m.clone(),
        };
        let job = Job {
            msg: Box::new(msg),
            urls,
            at_ms: crate::automod::now_ms(),
        };
        let mut l = self.lanes.lock().unwrap();
        let (live, old) = &mut *l;
        if backlog {
            push_capped(old, job, BACKLOG_CAP);
        } else {
            if live.len() >= LIVE_CAP {
                if let Some(j) = live.pop_front() {
                    push_capped(old, j, BACKLOG_CAP);
                }
            }
            live.push_back(job);
        }
        drop(l);
        self.wake.notify_one();
    }

    fn take(&self, now: i64) -> Option<Job> {
        let mut l = self.lanes.lock().unwrap();
        let (live, old) = &mut *l;
        while live
            .front()
            .is_some_and(|j| now - j.at_ms > LIVE_MAX_AGE_MS)
        {
            let j = live.pop_front().unwrap();
            push_capped(old, j, BACKLOG_CAP);
        }
        // Newest backlog first: what is still on screen matters more than what scrolled away.
        live.pop_front().or_else(|| old.pop_back())
    }

    async fn next(&self) -> Job {
        loop {
            let woken = self.wake.notified();
            if let Some(j) = self.take(crate::automod::now_ms()) {
                return j;
            }
            woken.await;
        }
    }

    /// Loads the bundled model on first use; None (logged once) if it does not load.
    fn model(&self) -> Option<Arc<Model>> {
        self.model
            .get_or_init(|| match load() {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::error!(?e, "NSFW model did not load, the image scan is off");
                    None
                }
            })
            .clone()
    }
}

fn push_capped(q: &mut VecDeque<Job>, j: Job, cap: usize) {
    if q.len() >= cap {
        q.pop_front();
        tracing::debug!("NSFW scan backlog full, dropped the oldest job");
    }
    q.push_back(j);
}

fn load() -> TractResult<Arc<Model>> {
    tract_onnx::onnx()
        .model_for_read(&mut std::io::Cursor::new(MODEL_BYTES))?
        .with_input_fact(0, f32::fact([1, SIDE, SIDE, 3]).into())?
        .into_optimized()?
        .into_runnable()
}

/// Scores of one image; None when it does not decode.
fn infer(model: &Arc<Model>, bytes: &[u8]) -> Option<[f32; 5]> {
    let img = crate::qr::read_image(bytes)?
        .resize_exact(
            SIDE as u32,
            SIDE as u32,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();
    let x: Tensor = tract_ndarray::Array4::from_shape_fn((1, SIDE, SIDE, 3), |(_, y, x, c)| {
        f32::from(img[(x as u32, y as u32)][c]) / 255.0
    })
    .into();
    let out = model.run(tvec!(x.into())).ok()?;
    let v = out[0].to_plain_array_view::<f32>().ok()?;
    let mut s = [0.0; 5];
    for (d, v) in s.iter_mut().zip(v.iter()) {
        *d = *v;
    }
    Some(s)
}

/// Workers: each takes one job at a time; the model runs on a blocking thread.
pub async fn run(app: Arc<App>) {
    loop {
        let job = app.nsfw.next().await;
        if let Err(e) = scan(&app, job).await {
            tracing::warn!(?e, "NSFW scan failed");
        }
    }
}

async fn scan(app: &App, job: Job) -> Result<()> {
    let m = job.msg;
    let Some(gid) = m.guild_id else { return Ok(()) };
    let cfg = app.cfg(gid.get()).await?;
    let f = &cfg.automod.filters;
    // Read again now: the filter may have been switched off while the job waited.
    if !cfg.automod.enabled || !f.nsfw_images.enabled {
        return Ok(());
    }
    // Automod does not watch webhooks with Monitor Webhooks off: no scan, row or hit log either.
    if m.webhook_id.is_some() && !cfg.automod.monitor_webhooks {
        return Ok(());
    }
    let (parent, _) = app.channel_scope(gid, m.channel_id).await?;
    if app.guild_info(gid).await?.nsfw.contains(&parent) {
        return Ok(());
    }
    let Some(model) = app.nsfw.model() else {
        return Ok(());
    };
    let (lo, hi) = NSFW_THRESHOLD;
    let threshold = (i64::from(f.nsfw_threshold).clamp(lo, hi) as f32) / 100.0;
    let mut hit: Option<(i64, [f32; 5])> = None;
    for url in &job.urls {
        let Some(bytes) = app.sites.fetch(&small(url), MAX_BYTES).await else {
            continue;
        };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let cached = app.nsfw.cache.lock().unwrap().get(&hash).copied();
        let scores = match cached {
            Some(s) => s,
            None => {
                let model = model.clone();
                // A panic in the decoder or the model ends only this image.
                let Ok(Some(s)) = tokio::task::spawn_blocking(move || infer(&model, &bytes)).await
                else {
                    continue;
                };
                let mut c = app.nsfw.cache.lock().unwrap();
                // shortcut: past the cap the whole cache goes; reposts are scored again once.
                if c.len() >= CACHE_CAP {
                    c.clear();
                }
                c.insert(hash, s);
                s
            }
        };
        let row = record(
            &app.pool,
            (gid.get(), m.channel_id.get(), m.id.get(), m.author.id.get()),
            &hash,
            &scores,
            threshold,
        )
        .await?;
        if let Some(a) = crate::automod::attachments(&m).find(|a| a.proxy_url == *url) {
            app.nsfw.seen.first((m.id.get(), a.id.get()));
        }
        if let (Some(id), None) = (row, hit) {
            hit = Some((id, scores));
        }
    }
    let Some((id, scores)) = hit else {
        return Ok(());
    };
    let mut msg = *m;
    // Swept messages come without the member; whitelists and staff checks need their roles.
    if msg.member.is_none() {
        if let Ok(r) = app.http.guild_member(gid, msg.author.id).await {
            let mem = r.model().await?;
            msg.member = Some(PartialMember {
                avatar: mem.avatar,
                avatar_decoration_data: None,
                banner: None,
                communication_disabled_until: mem.communication_disabled_until,
                deaf: mem.deaf,
                flags: mem.flags,
                joined_at: mem.joined_at,
                mute: mem.mute,
                nick: mem.nick,
                permissions: None,
                premium_since: mem.premium_since,
                roles: mem.roles,
                user: None,
            });
        }
    }
    act(app, gid, &msg, (id, &scores)).await
}

/// Automod's action for a hit, then the hit log.
async fn act(
    app: &App,
    gid: Id<GuildMarker>,
    msg: &Message,
    (id, scores): (i64, &[f32; 5]),
) -> Result<()> {
    let acted = crate::automod::on_nsfw_image(app, msg).await;
    // Unless the punishment landed, a failure frees the message: all its rows and files, so the next
    // pass (sweep, preview update, edit) scans it whole again. A failed case, log or lockdown after
    // a landed punishment (or a saved strike) must not punish twice.
    let failed = |e: &anyhow::Error| e.downcast_ref::<crate::automod::Punished>().is_none();
    if acted.as_ref().is_err_and(failed) {
        sqlx::query("DELETE FROM nsfw_scans WHERE guild_id = $1 AND message_id = $2")
            .bind(gid.get() as i64)
            .bind(msg.id.get() as i64)
            .execute(&app.pool)
            .await?;
        app.nsfw
            .seen
            .forget(crate::automod::attachments(msg).map(|a| (msg.id.get(), a.id.get())));
        return acted;
    }
    hit_log(app, gid, msg, id, scores).await?;
    acted
}

/// Saves one scan; Some(id) only for a new hit. One row per (message, image), so a preview update and
/// the sweep never act twice on it, and an image staff marked "Not NSFW" in this server is no hit.
async fn record(
    pool: &sqlx::PgPool,
    (guild, channel, message, user): (u64, u64, u64, u64),
    hash: &[u8; 32],
    scores: &[f32; 5],
    threshold: f32,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO nsfw_scans (guild_id, channel_id, message_id, user_id, sha256, scores, score, threshold, hit)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
             $7 >= $8 AND NOT EXISTS (SELECT 1 FROM nsfw_scans WHERE guild_id = $1 AND sha256 = $5 AND feedback = 'fp'))
         ON CONFLICT (guild_id, message_id, sha256) DO NOTHING
         RETURNING CASE WHEN hit THEN id END",
    )
    .bind(guild as i64)
    .bind(channel as i64)
    .bind(message as i64)
    .bind(user as i64)
    .bind(&hash[..])
    .bind(&scores[..])
    .bind(score(scores))
    .bind(threshold)
    .fetch_optional(pool)
    .await?
    .flatten())
}

/// Scores and the feedback buttons; the action itself is in the automod log. Never the image.
async fn hit_log(
    app: &App,
    gid: Id<GuildMarker>,
    m: &Message,
    id: i64,
    s: &[f32; 5],
) -> Result<()> {
    let pct = |i: usize| format!("{} {:.0}%", CLASSES[i], s[i] * 100.0);
    let log = Log::new("NSFW Image Found", ALERT)
        .user("Member", m.author.id.get())
        .line("Channel", &format!("<#{}>", m.channel_id))
        .line(
            "Score",
            &format!(
                "`{:.0}%` ({}, {}, {})",
                score(s) * 100.0,
                pct(3),
                pct(1),
                pct(4)
            ),
        )
        .line("Scan", &format!("`{id}`"));
    let row = Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            button(
                "Not NSFW",
                &format!("{BUTTON}fp:{id}"),
                ButtonStyle::Secondary,
            ),
            button("Correct", &format!("{BUTTON}ok:{id}"), ButtonStyle::Danger),
        ],
    });
    send_log_rows(app, gid.get(), LogKind::AutoMod, log, &[row]).await
}

/// `nsfw:fp:{scan}` / `nsfw:ok:{scan}` on the hit log: needs the purge permit (timeout and up).
/// Not NSFW lets that exact image through in this server from now on.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(BUTTON) else {
        return Ok(false);
    };
    let clicked = async {
        let (gid, clicker) = (i.guild_id?, i.author_id()?);
        let (kind, id) = rest.split_once(':')?;
        let id: i64 = id.parse().ok()?;
        let feedback = match kind {
            "fp" => "fp",
            "ok" => "ok",
            _ => return None,
        };
        Some(async move {
            let owner = app.guild_info(gid).await?.owner;
            if !app.level(gid, owner, clicker).await?.has("purge") {
                return Ok("You need the purge permit (or timeout and up) in this server.".into());
            }
            let done = sqlx::query(
                "UPDATE nsfw_scans SET feedback = $3, feedback_by = $4 WHERE id = $1 AND guild_id = $2",
            )
            .bind(id)
            .bind(gid.get() as i64)
            .bind(feedback)
            .bind(clicker.get() as i64)
            .execute(&app.pool)
            .await?
            .rows_affected();
            Ok(match (done, feedback) {
                (0, _) => "This scan is gone (scans are kept for 30 days).".into(),
                (_, "fp") => "Marked as not NSFW. This image is allowed in this server from now on. Undo the member's punishment by hand if it should not stand.".into(),
                _ => "Marked as correct. Thanks, this helps tune the scan.".into(),
            })
        }
        .await)
    };
    crate::bansync::answer_click(app, i, clicked).await?;
    Ok(true)
}

/// Every 5 minutes: servers that turned the scan on (or all of them after a start) get their last
/// 24 hours read into the backlog, and scans past 30 days are deleted.
pub async fn sweep(app: Arc<App>) {
    let mut swept: HashSet<u64> = HashSet::new();
    loop {
        if let Err(e) = sweep_once(&app, &mut swept).await {
            tracing::warn!(?e, "NSFW sweep failed");
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

async fn sweep_once(app: &App, swept: &mut HashSet<u64>) -> Result<()> {
    sqlx::query("DELETE FROM nsfw_scans WHERE at < now() - interval '30 days'")
        .execute(&app.pool)
        .await?;
    let rows: Vec<i64> = sqlx::query_scalar(
        "SELECT guild_id FROM
           (SELECT DISTINCT ON (guild_id) guild_id, config FROM guild_config ORDER BY guild_id, version DESC) c
         WHERE c.config->'automod'->'filters'->'nsfw_images'->>'enabled' = 'true'
           AND guild_id NOT IN (SELECT guild_id FROM guild_left)",
    )
    .fetch_all(&app.pool)
    .await?;
    let mut on = HashSet::new();
    for g in rows {
        let cfg = app.cfg(g as u64).await?;
        if cfg.automod.enabled && cfg.automod.filters.nsfw_images.enabled {
            on.insert(g as u64);
        }
    }
    // Switched off: forget it, so switching it on again sweeps again.
    swept.retain(|g| on.contains(g));
    for g in on {
        if swept.insert(g) {
            if let Err(e) = sweep_guild(app, Id::new(g)).await {
                tracing::warn!(?e, guild = g, "NSFW sweep of a server failed");
            }
        }
    }
    Ok(())
}

async fn sweep_guild(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let since = crate::automod::now_ms() - SWEEP_MS;
    let info = app.guild_info(gid).await?;
    let channels = app.http.guild_channels(gid).await?.models().await?;
    // shortcut: threads are not swept, only their parent channels; live scans still cover them.
    for c in channels.iter().filter(|c| {
        matches!(
            c.kind,
            ChannelType::GuildText | ChannelType::GuildAnnouncement
        ) && !info.nsfw.contains(&c.id)
    }) {
        let Ok(r) = app
            .http
            .channel_messages(c.id)
            .limit(SWEEP_PER_CHANNEL)
            .await
        else {
            continue; // no access to this channel
        };
        let msgs = r.models().await.unwrap_or_default();
        let ids: Vec<i64> = msgs.iter().map(|m| m.id.get() as i64).collect();
        // Already scanned (live, or by an earlier sweep): not downloaded again.
        let done: HashSet<i64> = sqlx::query_scalar(
            "SELECT DISTINCT message_id FROM nsfw_scans WHERE guild_id = $1 AND message_id = ANY($2)",
        )
        .bind(gid.get() as i64)
        .bind(&ids)
        .fetch_all(&app.pool)
        .await?
        .into_iter()
        .collect();
        for mut m in msgs {
            if m.timestamp.as_micros() / 1000 < since
                || (m.author.bot && m.webhook_id.is_none() && m.interaction_metadata.is_none())
                || done.contains(&(m.id.get() as i64))
            {
                continue;
            }
            m.guild_id = Some(gid);
            app.nsfw.enqueue(&m, false, true);
        }
        tokio::time::sleep(SWEEP_GAP).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Only a punishment that did not land frees the message's rows; a lockdown failing after the
    /// strike keeps them, so the next pass does not punish again.
    #[tokio::test]
    async fn only_a_failed_punishment_frees_the_scan_rows() {
        let Some(t) = crate::bansync::tests::T::new(96).await else {
            return;
        };
        let gid = t.g(0);
        let porn = [0.0, 0.0, 0.05, 0.9, 0.05];
        let rows = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM nsfw_scans WHERE guild_id = $1 AND message_id = 100",
            )
            .bind(gid.get() as i64)
            .fetch_one(&t.app.pool)
            .await
            .unwrap()
        };
        // A new author each time: the heat claim allows one punishment per member in 10 s.
        let try_once = |filter: serde_json::Value, author: u64, strikes: bool| {
            let t = &t;
            async move {
                let msg = crate::automod::more_tests::msg(json!({"guild_id": gid.to_string(),
                    "author": {"id": author.to_string(), "username": "x", "discriminator": "0", "avatar": null}}));
                t.cleanup().await;
                sqlx::query("DELETE FROM nsfw_scans WHERE guild_id = $1")
                    .bind(gid.get() as i64)
                    .execute(&t.app.pool)
                    .await
                    .unwrap();
                t.app.forget_cfg(gid.get());
                let cfg = json!({"automod": {"enabled": true, "auto_lockdown": {"enabled": !strikes, "strikes": 1},
                    "strike": {"enabled": strikes}, "filters": {"nsfw_images": filter}}});
                t.server_config(0, t.owner(), cfg).await;
                let ids = (gid.get(), 7, 100, 50);
                let id = record(&t.app.pool, ids, &[1; 32], &porn, 0.8)
                    .await
                    .unwrap();
                record(&t.app.pool, ids, &[2; 32], &[0.0, 0.0, 1.0, 0.0, 0.0], 0.8)
                    .await
                    .unwrap();
                act(&t.app, gid, &msg, (id.unwrap(), &porn)).await
            }
        };
        // The filter's own timeout fails at the mock: nothing landed, both rows go.
        let failed = try_once(
            json!({"enabled": true, "action": "timeout", "duration_secs": 60}),
            t.base + 5,
            false,
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(rows().await, 0);
        // A strike without a timeout, then the strike-cap lockdown fails: the rows stay.
        let locked = try_once(json!({"enabled": true}), t.base + 6, false).await;
        assert!(locked.is_err());
        assert_eq!(rows().await, 2);
        // The strike is saved, then its timeout fails: the rows stay, so no pass adds a second strike.
        let timed = try_once(json!({"enabled": true}), t.base + 7, true).await;
        assert!(timed.is_err());
        assert_eq!(rows().await, 2);
        // An error before any punishment (a held raider's timeout fails): the rows go, to retry.
        let raider = t.base + 8;
        t.app
            .heat_raiders
            .lock()
            .unwrap()
            .insert(gid.get(), ([raider].into(), crate::app::now_secs() + 600));
        let early = try_once(json!({"enabled": true}), raider, true).await;
        assert!(early.is_err());
        assert_eq!(rows().await, 0);
        sqlx::query("DELETE FROM nsfw_scans WHERE guild_id = $1")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        t.exec("DELETE FROM automod_strikes WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.exec("DELETE FROM lock_snapshots WHERE guild_id BETWEEN $1 AND $1 + 9")
            .await;
        t.cleanup().await;
    }

    /// An edit of a webhook message queues only files not queued before.
    #[test]
    fn an_edit_rescans_only_new_files() {
        let file = |id: &str| {
            json!({"id": id, "filename": "a.png", "size": 10, "url": "https://cdn.discordapp.com/a.png",
                   "proxy_url": format!("https://media.discordapp.net/{id}.png"), "content_type": "image/png"})
        };
        let s = Scanner::default();
        let hook = |files: serde_json::Value| {
            crate::automod::more_tests::msg(json!({"webhook_id": "9", "attachments": files}))
        };
        s.enqueue(&hook(json!([file("1")])), false, false);
        assert_eq!(s.take(0).unwrap().urls.len(), 1);
        // Not scanned yet (dropped, or the scan stopped early): queued again.
        s.enqueue(&hook(json!([file("1")])), false, false);
        assert_eq!(s.take(0).unwrap().urls.len(), 1);
        // `scan` marks a file once its row is saved.
        s.seen.first((100, 1));
        s.enqueue(&hook(json!([file("1")])), false, false);
        assert!(s.take(0).is_none(), "same file again");
        s.enqueue(&hook(json!([file("1"), file("2")])), false, false);
        assert_eq!(s.take(0).unwrap().urls.len(), 2, "a new file came");
        // A forward keeps the attachment id but is another message: scanned.
        let fwd = crate::automod::more_tests::msg(
            json!({"id": "101", "webhook_id": "9", "attachments": [file("1")]}),
        );
        s.enqueue(&fwd, false, false);
        assert_eq!(s.take(0).unwrap().urls.len(), 1);
    }

    /// With Monitor Webhooks off a webhook's image is not scanned, saved or logged.
    #[tokio::test]
    async fn unmonitored_webhooks_are_not_scanned() {
        let Some(t) = crate::bansync::tests::T::new(94).await else {
            return;
        };
        let gid = t.g(0);
        let job = |hook: bool| Job {
            msg: Box::new(crate::automod::more_tests::msg(json!({
                "guild_id": gid.to_string(), "webhook_id": if hook { json!("9") } else { json!(null) },
            }))),
            urls: vec!["https://media.discordapp.net/a.png".into()],
            at_ms: 0,
        };
        let on = json!({"enabled": true, "monitor_webhooks": false, "filters": {"nsfw_images": {"enabled": true}}});
        t.server_config(0, t.owner(), json!({ "automod": on }))
            .await;
        scan(&t.app, job(true)).await.unwrap();
        assert!(t.mock.lock().unwrap().log.is_empty(), "nothing fetched");
        // A member's post in the same server is looked at (the mock refuses the channel lookup).
        let _ = scan(&t.app, job(false)).await;
        assert!(!t.mock.lock().unwrap().log.is_empty());
        t.cleanup().await;
    }

    /// A user-installed app's reply is queued as the member's post, a server-installed app's not at
    /// all; a forward's images are scanned.
    #[test]
    fn app_replies_and_forwards_are_queued_as_the_member_s() {
        let file = json!({"id": "1", "filename": "a.png", "size": 10, "url": "https://cdn.discordapp.com/a.png",
                          "proxy_url": "https://media.discordapp.net/a.png", "content_type": "image/png"});
        let meta = |owners: serde_json::Value| {
            json!({"id": "5", "type": 2, "authorizing_integration_owners": owners,
                   "user": {"id": "60", "username": "u", "discriminator": "0", "avatar": null}})
        };
        let s = Scanner::default();
        let server = crate::automod::more_tests::msg(json!({
            "attachments": [file], "interaction_metadata": meta(json!({"0": "1"}))}));
        s.enqueue(&server, false, false);
        // Log-only: still exempt (metadata kept), but credited to the member who ran it.
        let j = s.take(0).unwrap();
        assert_eq!(j.msg.author.id.get(), 60);
        assert!(j.msg.interaction_metadata.is_some());
        let file = json!({"id": "2", "filename": "a.png", "size": 10, "url": "https://cdn.discordapp.com/a.png",
                          "proxy_url": "https://media.discordapp.net/b.png", "content_type": "image/png"});
        let user_app = crate::automod::more_tests::msg(json!({
            "attachments": [file], "interaction_metadata": meta(json!({"1": "60"}))}));
        s.enqueue(&user_app, false, false);
        assert_eq!(s.take(0).unwrap().msg.author.id.get(), 60);
        let file = json!({"id": "3", "filename": "a.png", "size": 10, "url": "https://cdn.discordapp.com/a.png",
                          "proxy_url": "https://media.discordapp.net/c.png", "content_type": "image/png"});
        let forward = crate::automod::more_tests::msg(json!({"message_snapshots": [{"message": {
            "attachments": [file], "content": "", "embeds": [], "mentions": [], "mention_roles": [],
            "type": 0, "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null,
        }}]}));
        assert_eq!(image_urls(&forward, false).len(), 1);
    }

    #[test]
    fn bundled_model_is_the_pinned_one_and_scores_a_plain_image_neutral() {
        let hash: String = Sha256::digest(MODEL_BYTES)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hash,
            "e448e86df0d976da123f746cbf4b926713a774064a69ce9b25cff04d58abadbb"
        );
        let model = load().unwrap();
        let mut png = vec![];
        image::RgbImage::from_pixel(300, 200, image::Rgb([40, 120, 200]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let s = infer(&model, &png).unwrap();
        assert!((s.iter().sum::<f32>() - 1.0).abs() < 0.01, "{s:?}");
        assert!(score(&s) < 0.5, "{s:?}");
        assert!(infer(&model, b"not an image").is_none());
    }

    #[test]
    fn only_discord_media_is_fetched() {
        assert!(discord_media(
            "https://media.discordapp.net/attachments/1/2/a.png?ex=1"
        ));
        assert!(discord_media(
            "https://cdn.discordapp.com/attachments/1/2/a.png"
        ));
        assert!(discord_media(
            "https://images-ext-1.discordapp.net/external/x/https/evil.com/a.png"
        ));
        for bad in [
            "http://media.discordapp.net/a.png",
            "https://evil.com/a.png",
            "https://media.discordapp.net.evil.com/a.png",
            "https://images-ext-1.discordapp.net.evil.com/a.png",
            "https://127.0.0.1/a.png",
            "not a url",
        ] {
            assert!(!discord_media(bad), "{bad}");
        }
        assert_eq!(
            small("https://x/a.png?ex=1"),
            "https://x/a.png?ex=1&width=224&height=224"
        );
        assert_eq!(
            small("https://x/a.png"),
            "https://x/a.png?width=224&height=224"
        );
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn a_scan_acts_once_and_not_nsfw_allows_the_image() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let g = 3_800_000_000_000 + std::process::id() as u64 * 10;
        let (bad, fine) = ([1u8; 32], [2u8; 32]);
        let porn = [0.0, 0.0, 0.05, 0.9, 0.05];
        let ids = |m| (g, 1, m, 7);
        let first = record(&pool, ids(100), &bad, &porn, 0.8).await.unwrap();
        assert!(first.is_some());
        assert_eq!(
            record(&pool, ids(100), &bad, &porn, 0.8).await.unwrap(),
            None,
            "same message and image: no second action"
        );
        assert_eq!(
            record(&pool, ids(101), &fine, &[0.0, 0.0, 1.0, 0.0, 0.0], 0.8)
                .await
                .unwrap(),
            None,
            "neutral"
        );
        assert_eq!(
            record(&pool, ids(102), &bad, &porn, 0.95).await.unwrap(),
            None,
            "below a higher threshold"
        );
        sqlx::query("UPDATE nsfw_scans SET feedback = 'fp' WHERE id = $1")
            .bind(first.unwrap())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            record(&pool, ids(103), &bad, &porn, 0.8).await.unwrap(),
            None,
            "allowed after Not NSFW"
        );
        assert!(
            record(&pool, (g + 1, 1, 104, 7), &bad, &porn, 0.8)
                .await
                .unwrap()
                .is_some(),
            "only in that server"
        );
        sqlx::query("DELETE FROM nsfw_scans WHERE guild_id IN ($1, $2)")
            .bind(g as i64)
            .bind(g as i64 + 1)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn score_weighs_sexy_half() {
        assert_eq!(score(&[0.0, 0.0, 1.0, 0.0, 0.0]), 0.0);
        assert!((score(&[0.0, 0.1, 0.0, 0.5, 0.4]) - 0.8).abs() < 1e-6);
        assert_eq!(score(&[0.0, 0.0, 0.0, 1.0, 1.0]), 1.0);
    }

    fn job(at_ms: i64, n: u64) -> Job {
        let m: Message = serde_json::from_value(serde_json::json!({
            "id": n.to_string(), "channel_id": "1", "author": {"id": "2", "username": "u", "discriminator": "0", "avatar": null},
            "content": "", "timestamp": "2026-01-01T00:00:00+00:00", "edited_timestamp": null, "tts": false,
            "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
            "pinned": false, "type": 0
        }))
        .unwrap();
        Job {
            msg: Box::new(m),
            urls: vec![],
            at_ms,
        }
    }

    #[test]
    fn live_goes_first_and_old_live_jobs_move_to_the_backlog() {
        let s = Scanner::default();
        {
            let mut l = s.lanes.lock().unwrap();
            l.1.push_back(job(0, 1)); // backlog
            l.1.push_back(job(0, 2));
            l.0.push_back(job(0, 3)); // live, will be stale at now = 200_000
            l.0.push_back(job(150_000, 4)); // live, fresh
        }
        let now = 200_000;
        let order: Vec<u64> = std::iter::from_fn(|| s.take(now))
            .map(|j| j.msg.id.get())
            .collect();
        // Fresh live first, then the backlog newest first (the stale live job went to its end).
        assert_eq!(order, [4, 3, 2, 1]);
    }

    #[test]
    fn full_lanes_drop_the_oldest() {
        let mut q = VecDeque::new();
        for i in 1..=3 {
            push_capped(&mut q, job(0, i), 2);
        }
        let ids: Vec<u64> = q.iter().map(|j| j.msg.id.get()).collect();
        assert_eq!(ids, [2, 3]);
    }
}
