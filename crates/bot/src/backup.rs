//! Server backups (spec 4.3): snapshot roles and channels into Postgres, and restore by syncing
//! the live server to an image. Restore uses its own HTTP client (own rate-limit bucket) so a
//! flood of normal traffic can't slow it down during a nuke.
use anyhow::Result;
use sqlx::types::Json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tommybot_antinuke::{
    describe, map_overwrites, plan, Action, Change, ChannelImg, Image, OverwriteImg, Plan, RoleImg,
    Settings, WebhookImg, CATEGORY, DANGEROUS_PERMS,
};
use twilight_http::Client;
use twilight_model::{
    channel::{
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
        Channel, ChannelType,
    },
    guild::{
        DefaultMessageNotificationLevel, ExplicitContentFilter, Permissions, Role, RolePosition,
        VerificationLevel,
    },
    id::{marker::GuildMarker, Id},
};

use crate::app::App;
use tommybot_core::sections::BackupsCfg;
use tommybot_logging::embed::{NO, YES};

/// How often the auto-backup loop looks at each guild; the guild's own interval decides.
const TICK: Duration = Duration::from_secs(600);

// Role::color is deprecated upstream in favour of `colors`, but create/update_role still take it.
#[allow(deprecated)]
pub fn role_img(r: Role) -> RoleImg {
    RoleImg {
        id: r.id.get(),
        name: r.name,
        color: r.color,
        permissions: r.permissions.bits(),
        position: r.position,
        hoist: r.hoist,
        mentionable: r.mentionable,
        managed: r.managed,
    }
}

pub fn channel_img(c: Channel) -> ChannelImg {
    ChannelImg {
        id: c.id.get(),
        name: c.name.unwrap_or_default(),
        kind: u8::from(c.kind),
        parent: c.parent_id.map(|p| p.get()),
        position: c.position.map_or(0, i64::from),
        topic: c.topic,
        nsfw: c.nsfw.unwrap_or(false),
        overwrites: c
            .permission_overwrites
            .unwrap_or_default()
            .into_iter()
            .map(|o| OverwriteImg {
                target: o.id.get(),
                is_role: o.kind == PermissionOverwriteType::Role,
                allow: o.allow.bits(),
                deny: o.deny.bits(),
            })
            .collect(),
    }
}

pub async fn snapshot(http: &Client, gid: Id<GuildMarker>) -> Result<Image> {
    let g = http.guild(gid).await?.model().await?;
    let roles = crate::hierarchy::ranked(gid, http.roles(gid).await?.models().await?);
    let channels = http.guild_channels(gid).await?.models().await?;
    // Needs Manage Webhooks; without it the image has none and restores leave webhooks alone.
    let webhooks = match http.guild_webhooks(gid).await {
        Ok(r) => Some(
            r.models()
                .await?
                .into_iter()
                // twilight 0.18: channel_id is nullable; a webhook without a channel can't be restored.
                .filter_map(|w| {
                    Some(WebhookImg {
                        id: w.id.get(),
                        name: w.name.unwrap_or_default(),
                        channel: w.channel_id?.get(),
                    })
                })
                .collect(),
        ),
        Err(e) => {
            tracing::warn!(?e, "backup: listing webhooks failed");
            None
        }
    };
    Ok(Image {
        guild: gid.get(),
        webhooks,
        settings: Some(Settings {
            name: g.name.clone(),
            verification: u8::from(g.verification_level),
            notifications: u8::from(g.default_message_notifications),
            content_filter: u8::from(g.explicit_content_filter),
            afk_channel: g.afk_channel_id.map(|c| c.get()),
            afk_timeout: g.afk_timeout.get().into(),
            system_channel: g.system_channel_id.map(|c| c.get()),
            locale: g.preferred_locale,
        }),
        name: g.name,
        roles: roles.into_iter().map(role_img).collect(),
        channels: channels.into_iter().map(channel_img).collect(),
    })
}

/// Adds a role or channel made by a whitelisted member to the latest backup, so deleting it before
/// the next backup can't make it unrecoverable. Attackers' creations are never added, or a
/// restore would keep them. Does nothing without a backup.
pub async fn track_created(app: &App, gid: Id<GuildMarker>, action: Action, id: u64) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let row: Option<(i64, Json<Image>)> = sqlx::query_as(
        "SELECT id, image FROM backups WHERE guild_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1 FOR UPDATE",
    )
    .bind(gid.get() as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((row_id, Json(mut image))) = row else {
        return Ok(());
    };
    if action == Action::RoleCreate {
        let roles = crate::hierarchy::ranked(gid, app.http.roles(gid).await?.models().await?);
        match roles.into_iter().find(|r| r.id.get() == id) {
            Some(r) if !image.roles.iter().any(|x| x.id == id) => image.roles.push(role_img(r)),
            _ => return Ok(()),
        }
    } else {
        let c = app.http.channel(Id::new(id)).await?.model().await?;
        if image.channels.iter().any(|x| x.id == id) {
            return Ok(());
        }
        image.channels.push(channel_img(c));
    }
    sqlx::query("UPDATE backups SET image = $1 WHERE id = $2")
        .bind(Json(&image))
        .bind(row_id)
        .execute(&mut *tx)
        .await?;
    Ok(tx.commit().await?)
}

/// Takes a backup (`by` is None for automatic ones) and returns its 6-char code.
pub async fn save(app: &App, gid: Id<GuildMarker>, by: Option<u64>) -> Result<String> {
    let cfg = app.cfg(gid.get()).await?.backups.clone();
    if !cfg.enabled {
        anyhow::bail!("Backups are turned off for this server. Turn on Backups status on the dashboard first.");
    }
    let image = snapshot(&app.http, gid).await?;
    let (code,): (String,) = sqlx::query_as(
        "INSERT INTO backups (guild_id, image, created_by) VALUES ($1, $2, $3) RETURNING code",
    )
    .bind(gid.get() as i64)
    .bind(Json(&image))
    .bind(by.map(|u| u as i64))
    .fetch_one(&app.pool)
    .await?;
    sqlx::query(
        "DELETE FROM backups WHERE guild_id = $1 AND id NOT IN \
         (SELECT id FROM backups WHERE guild_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2)",
    )
    .bind(gid.get() as i64)
    .bind(cfg.max_images.max(1))
    .execute(&app.pool)
    .await?;
    Ok(code)
}

pub async fn latest(app: &App, gid: Id<GuildMarker>) -> Result<Option<Image>> {
    let row: Option<(Json<Image>,)> = sqlx::query_as(
        "SELECT image FROM backups WHERE guild_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(gid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    Ok(row.map(|(Json(i),)| i))
}

/// Live ids a restore must never delete (`sections::restore_keep`, N2).
async fn keep_set(app: &App, gid: Id<GuildMarker>) -> Result<HashSet<u64>> {
    let g = gid.get() as i64;
    let cfg: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT config FROM guild_config WHERE guild_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(g)
    .fetch_optional(&app.pool)
    .await?;
    let cfg = cfg.map(|c| c.0).unwrap_or_default();
    Ok(tommybot_core::sections::restore_keep(&app.pool, g, &cfg).await?)
}

/// Panic-mode restore: sync to the latest image and keep the server lock. `None` means there is
/// no backup yet.
pub async fn restore_latest(app: &App, gid: Id<GuildMarker>) -> Result<Option<Summary>> {
    let Some(image) = latest(app, gid).await? else {
        return Ok(None);
    };
    let keep = keep_set(app, gid).await?;
    let filters = app.cfg(gid.get()).await?.backups.clone();
    Ok(Some(
        restore(app, gid, &image, &keep, Some(&filters)).await?,
    ))
}

/// The 10 latest backups as `(code, created_at unix secs, roles, channels, author)`, newest first.
pub async fn list(
    app: &App,
    gid: Id<GuildMarker>,
) -> Result<Vec<(String, i64, i32, i32, Option<i64>)>> {
    Ok(sqlx::query_as(
        "SELECT code, EXTRACT(EPOCH FROM created_at)::BIGINT, jsonb_array_length(image->'roles')::INT, \
         jsonb_array_length(image->'channels')::INT, created_by FROM backups \
         WHERE guild_id = $1 ORDER BY created_at DESC, id DESC LIMIT 10",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?)
}

/// `t!backups ID`: who took the backup, when, and what it holds.
pub async fn info(app: &App, gid: Id<GuildMarker>, id: &str) -> Result<String> {
    let id = id.trim_start_matches('#').to_lowercase();
    let row: Option<(String, i64, Option<i64>, Json<Image>)> = sqlx::query_as(
        "SELECT code, EXTRACT(EPOCH FROM created_at)::BIGINT, created_by, image FROM backups \
         WHERE guild_id = $1 AND (code = $2 OR id::text = $2)",
    )
    .bind(gid.get() as i64)
    .bind(&id)
    .fetch_optional(&app.pool)
    .await?;
    let Some((code, at, by, Json(img))) = row else {
        return Ok(format!("No backup `{id}`"));
    };
    Ok(info_text(&code, at, by, &img))
}

fn info_text(code: &str, at: i64, by: Option<i64>, img: &Image) -> String {
    let by = by.map_or("TommyBot".into(), |u| format!("<@{u}>"));
    let cats = img.channels.iter().filter(|c| c.kind == CATEGORY).count();
    let mark = |b: bool| if b { YES } else { NO };
    format!(
        "**Backup `{code}`**\nTaken by {by} <t:{at}:R>\nRoles: {} · Channels: {} · Categories: {cats} · Webhooks: {} · Server settings: {}",
        img.roles.len(),
        img.channels.len() - cats,
        img.webhooks.as_ref().map_or(0, Vec::len),
        mark(img.settings.is_some()),
    )
}

/// Deletes the backup with this code (or numeric dashboard id); `None` deletes all of them.
pub async fn delete(app: &App, gid: Id<GuildMarker>, key: Option<&str>) -> Result<bool> {
    let r = sqlx::query(
        "DELETE FROM backups WHERE guild_id = $1 AND ($2::text IS NULL OR code = $2 OR id::text = $2)",
    )
    .bind(gid.get() as i64)
    .bind(key.map(|k| k.trim_start_matches('#').to_lowercase()))
        .execute(&app.pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// `/backups load`: without `confirm` only describes what would change; with it, syncs the server
/// to the backup with this code (or numeric dashboard id).
pub async fn load(app: &App, gid: Id<GuildMarker>, id: &str, confirm: bool) -> Result<String> {
    let id = id.trim_start_matches('#').to_lowercase();
    let row: Option<(Json<Image>,)> = sqlx::query_as(
        "SELECT image FROM backups WHERE guild_id = $1 AND (code = $2 OR id::text = $2)",
    )
    .bind(gid.get() as i64)
    .bind(&id)
    .fetch_optional(&app.pool)
    .await?;
    let Some((Json(image),)) = row else {
        return Ok(format!("No backup #{id}"));
    };
    let keep = keep_set(app, gid).await?;
    if confirm {
        // A restore during a lock or panic would undo the locked permissions (D11).
        if locked_or_panic(app, gid).await? {
            return Ok(
                "Cannot load a backup while the server is locked or in panic mode. End it first."
                    .into(),
            );
        }
        let n = restore(app, gid, &image, &keep, None).await?.total();
        return Ok(format!("Restored backup #{id} ({n} changes)"));
    }
    let live = snapshot(&app.http, gid).await?;
    let p = plan(&image, &live, &keep, false);
    let lines = describe(&p, &live);
    if lines.is_empty() {
        return Ok(format!("Backup #{id} already matches the server"));
    }
    let shown = lines.len().min(15);
    let mut text = format!(
        "Loading backup #{id} would change {} things:\n{}",
        lines.len(),
        lines[..shown].join("\n")
    );
    if lines.len() > shown {
        text += format!("\n...and {} more", lines.len() - shown).as_str();
    }
    text += "\nRun again with confirm:true to apply.";
    Ok(text)
}

/// Anti-nuke: recreates the roles and channels with these backup ids (deleted by a nuker), each
/// from the newest image that has it, plus children re-parented to a recreated category. Nothing
/// else is touched.
pub async fn recreate(
    app: &App,
    gid: Id<GuildMarker>,
    ids: &HashSet<u64>,
    filters: Option<&BackupsCfg>,
) -> Result<Recreated> {
    let images: Vec<(Json<Image>,)> = sqlx::query_as(
        "SELECT image FROM backups WHERE guild_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(gid.get() as i64)
    .fetch_all(&app.pool)
    .await?;
    let Some((image, stripped)) = newest_with(images.into_iter().map(|(Json(i),)| i), ids) else {
        return Ok(Recreated::default());
    };
    let live = snapshot(&app.restore_http, gid).await?;
    let keep: HashSet<u64> = live
        .roles
        .iter()
        .map(|r| r.id)
        .chain(live.channels.iter().map(|c| c.id))
        .collect();
    let mut p = plan(&image, &live, &keep, true);
    p.settings = None;
    p.roles
        .retain(|c| matches!(c, Change::Create(r) if ids.contains(&r.id)));
    p.channels.retain(|c| match c {
        Change::Create(c) => ids.contains(&c.id),
        Change::Update { to, .. } => to.parent.is_some_and(|p| ids.contains(&p)),
        Change::Delete(_) => false,
    });
    if let Some(f) = filters {
        apply_filters(&mut p, f);
    }
    let (sum, id_map) = execute(app, gid, (&image, &live), p, true).await;
    crate::relink::relink(app, gid, &id_map, &image).await;
    crate::lockdown::relink_roles(app, gid, &id_map).await;
    let back: HashSet<u64> = ids
        .iter()
        .copied()
        .filter(|i| id_map.contains_key(i))
        .collect();
    Ok(Recreated {
        changes: sum.total(),
        stripped: stripped.intersection(&back).count(),
        back,
    })
}

/// What [`recreate`] did.
#[derive(Debug, Default)]
pub struct Recreated {
    pub changes: usize,
    /// Which of the ids exist again (recreated, or matched to a live object).
    pub back: HashSet<u64>,
    /// Roles that came back from an older image, without their dangerous permissions.
    pub stripped: usize,
}

/// The newest image (first), plus each of `ids` it lacks from the newest older image that has it:
/// a backup taken after the delete (or mid-nuke) must not make the object unrecoverable. A role
/// from an older image loses its dangerous permissions (it may predate staff taking them away);
/// their ids are the second value.
fn newest_with(
    mut images: impl Iterator<Item = Image>,
    ids: &HashSet<u64>,
) -> Option<(Image, HashSet<u64>)> {
    let mut image = images.next()?;
    let mut stripped = HashSet::new();
    for old in images {
        for mut r in old.roles {
            if ids.contains(&r.id) && !image.roles.iter().any(|x| x.id == r.id) {
                if r.permissions & DANGEROUS_PERMS != 0 {
                    r.permissions &= !DANGEROUS_PERMS;
                    stripped.insert(r.id);
                }
                image.roles.push(r);
            }
        }
        for c in old.channels {
            if ids.contains(&c.id) && !image.channels.iter().any(|x| x.id == c.id) {
                image.channels.push(c);
            }
        }
    }
    Some((image, stripped))
}

fn overwrites(list: &[OverwriteImg]) -> Vec<PermissionOverwrite> {
    list.iter()
        .map(|o| PermissionOverwrite {
            id: Id::new(o.target),
            kind: if o.is_role {
                PermissionOverwriteType::Role
            } else {
                PermissionOverwriteType::Member
            },
            allow: Permissions::from_bits_retain(o.allow),
            deny: Permissions::from_bits_retain(o.deny),
        })
        .collect()
}

/// Drops the creates and deletes the restore filters (`anr`) turned off; updates stay unless the
/// whole restore system is off.
fn apply_filters(p: &mut Plan, f: &BackupsCfg) {
    if !f.restore {
        *p = Plan::default();
        return;
    }
    fn keep<T>(c: &Change<T>, create: bool, delete: bool) -> bool {
        match c {
            Change::Create(_) => create,
            Change::Delete(_) => delete,
            Change::Update { .. } => true,
        }
    }
    p.roles.retain(|c| keep(c, f.restore_roles, f.delete_roles));
    p.channels
        .retain(|c| keep(c, f.restore_channels, f.delete_channels));
}

/// Syncs the server to `image` and returns how many changes were made. `filters` is set for panic
/// mode: it applies the restore filters and keeps live role permissions and channel overwrites
/// (panic mode locks the server first). One failed change is logged and skipped, so a single
/// missing permission can't abort the whole restore.
pub async fn restore(
    app: &App,
    gid: Id<GuildMarker>,
    image: &Image,
    keep: &HashSet<u64>,
    filters: Option<&BackupsCfg>,
) -> Result<Summary> {
    let respect_lock = filters.is_some();
    let http = &app.restore_http;
    let live = snapshot(http, gid).await?;
    // Created by lockdown::exec; restoring must not delete it while the server is locked.
    let mut keep = keep.clone();
    keep.extend(
        live.channels
            .iter()
            .filter(|c| c.name == "server-locked")
            .map(|c| c.id),
    );
    let mut p = plan(image, &live, &keep, respect_lock);
    if let Some(f) = filters {
        apply_filters(&mut p, f);
    }
    let (mut sum, id_map) = execute(app, gid, (image, &live), p, respect_lock).await;
    crate::relink::relink(app, gid, &id_map, image).await;
    crate::lockdown::relink_roles(app, gid, &id_map).await;
    // Webhooks that did not exist in the image were made since (old images have no list).
    if let (Some(f), Some(old)) = (filters, &image.webhooks) {
        if f.restore && f.delete_webhooks {
            let known: HashSet<u64> = old.iter().map(|w| w.id).collect();
            for w in live
                .webhooks
                .iter()
                .flatten()
                .filter(|w| !known.contains(&w.id))
            {
                let r = http.delete_webhook(Id::new(w.id)).await.map(drop);
                sum.webhooks_deleted += usize::from(count(r.map_err(Into::into), "webhook"));
            }
        }
    }
    Ok(sum)
}

/// Applies a plan. A failed step is logged and skipped, so one missing permission can't abort it.
async fn execute(
    app: &App,
    gid: Id<GuildMarker>,
    (image, live): (&Image, &Image),
    mut p: Plan,
    respect_lock: bool,
) -> (Summary, HashMap<u64, u64>) {
    let http = &app.restore_http;
    let mut done = Summary::default();
    let live_cats: HashSet<u64> = live
        .channels
        .iter()
        .filter(|c| c.kind == CATEGORY)
        .map(|c| c.id)
        .collect();
    let mut id_map = std::mem::take(&mut p.id_map);

    for ch in &p.roles {
        let r: Result<()> = async {
            match ch {
                Change::Create(r) => {
                    let new = http
                        .create_role(gid)
                        .name(&r.name)
                        .color(r.color)
                        .permissions(Permissions::from_bits_retain(r.permissions))
                        .hoist(r.hoist)
                        .mentionable(r.mentionable)
                        .await?
                        .model()
                        .await?;
                    id_map.insert(r.id, new.id.get());
                }
                Change::Update { live, to } => {
                    http.update_role(gid, Id::new(*live))
                        .name(Some(&to.name))
                        .color(Some(to.color))
                        .permissions(Permissions::from_bits_retain(to.permissions))
                        .hoist(to.hoist)
                        .mentionable(to.mentionable)
                        .await?;
                }
                Change::Delete(id) => {
                    http.delete_role(gid, Id::new(*id)).await?;
                }
            }
            Ok(())
        }
        .await;
        if count(r, "role") {
            done.roles.bump(ch);
        }
    }

    for ch in &p.channels {
        let r: Result<()> = async {
            match ch {
                Change::Create(c) => {
                    let ows = overwrites(&map_overwrites(&c.overwrites, &id_map));
                    let parent = c.parent.and_then(|p| id_map.get(&p)).map(|p| Id::new(*p));
                    let mut req = http
                        .create_guild_channel(gid, &c.name)
                        .kind(ChannelType::from(c.kind))
                        .nsfw(c.nsfw)
                        .permission_overwrites(&ows);
                    if let Some(p) = parent {
                        req = req.parent_id(p);
                    }
                    if let Some(t) = &c.topic {
                        req = req.topic(t);
                    }
                    let new = req.await?.model().await?;
                    id_map.insert(c.id, new.id.get());
                }
                Change::Update { live, to } => {
                    let ows = overwrites(&map_overwrites(&to.overwrites, &id_map));
                    let parent = to.parent.and_then(|p| id_map.get(&p)).map(|p| Id::new(*p));
                    let mut req = http
                        .update_channel(Id::new(*live))
                        .name(&to.name)
                        .nsfw(to.nsfw)
                        .parent_id(parent);
                    if !respect_lock {
                        req = req.permission_overwrites(&ows);
                    }
                    if let Some(t) = &to.topic {
                        req = req.topic(t);
                    }
                    req.await?;
                }
                Change::Delete(id) => {
                    http.delete_channel(Id::new(*id)).await?;
                }
            }
            Ok(())
        }
        .await;
        if count(r, "channel") {
            let cat = match ch {
                Change::Create(c) => c.kind == CATEGORY,
                Change::Update { to, .. } => to.kind == CATEGORY,
                Change::Delete(id) => live_cats.contains(id),
            };
            if cat {
                &mut done.categories
            } else {
                &mut done.channels
            }
            .bump(ch);
        }
    }

    if let Some(w) = p.settings.take() {
        let map = |c: Option<u64>| {
            c.and_then(|c| id_map.get(&c).copied().or(Some(c)))
                .map(Id::new)
        };
        let r = http
            .update_guild(gid)
            .name(&w.name)
            .verification_level(Some(VerificationLevel::from(w.verification)))
            .default_message_notifications(Some(DefaultMessageNotificationLevel::from(
                w.notifications,
            )))
            .explicit_content_filter(Some(ExplicitContentFilter::from(w.content_filter)))
            .afk_channel_id(map(w.afk_channel))
            .afk_timeout(w.afk_timeout)
            .system_channel(map(w.system_channel))
            .preferred_locale(Some(&w.locale))
            .await;
        done.settings = count(r.map(drop).map_err(Into::into), "server settings");
    }

    // Recreated roles land at the bottom; put every restored role back in its image order.
    let order: HashMap<u64, i64> = image.roles.iter().map(|r| (r.id, r.position)).collect();
    let positions: Vec<RolePosition> = id_map
        .iter()
        .filter_map(|(img, live)| {
            Some(RolePosition {
                id: Id::new(*live),
                position: u64::try_from(*order.get(img)?).ok()?,
            })
        })
        .collect();
    if !positions.is_empty() {
        if let Err(e) = http.update_role_positions(gid, &positions).await {
            tracing::warn!(?e, "restoring role positions failed");
        }
    }
    (done, id_map)
}

fn count(r: Result<()>, what: &str) -> bool {
    r.map_err(|e| tracing::warn!(?e, what, "restore step failed"))
        .is_ok()
}

/// Successful changes of one object type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub created: usize,
    pub deleted: usize,
    pub updated: usize,
}

impl Counts {
    fn bump<T>(&mut self, c: &Change<T>) {
        match c {
            Change::Create(_) => self.created += 1,
            Change::Delete(_) => self.deleted += 1,
            Change::Update { .. } => self.updated += 1,
        }
    }
}

/// What a restore changed, for the panic report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub roles: Counts,
    pub channels: Counts,
    pub categories: Counts,
    pub webhooks_deleted: usize,
    pub settings: bool,
}

impl Summary {
    pub fn total(&self) -> usize {
        [self.roles, self.channels, self.categories]
            .iter()
            .map(|c| c.created + c.deleted + c.updated)
            .sum::<usize>()
            + self.webhooks_deleted
            + usize::from(self.settings)
    }
}

/// Backs up every guild whose backups setting is on, once its own interval has passed since the
/// last backup.
pub async fn auto_backup(app: std::sync::Arc<App>) {
    let mut tick = tokio::time::interval(TICK);
    loop {
        tick.tick().await;
        match app.http.current_user_guilds().await {
            Ok(r) => {
                for g in r.models().await.unwrap_or_default() {
                    let Ok(cfg) = app.cfg(g.id.get()).await else {
                        continue;
                    };
                    if !cfg.backups.auto || !cfg.backups.enabled || under_attack(&app, g.id.get()) {
                        continue;
                    }
                    match due(&app, g.id, cfg.backups.interval_hours).await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => {
                            tracing::warn!(?e, guild = g.id.get(), "backup due check failed");
                            continue;
                        }
                    }
                    if let Err(e) = save(&app, g.id, None).await {
                        tracing::warn!(?e, guild = g.id.get(), "auto backup failed");
                    }
                }
            }
            Err(e) => tracing::warn!(?e, "listing guilds for backup failed"),
        }
    }
}

/// The server is in panic mode or has a lock on: a restore or a backup delete waits until it ends.
pub(crate) async fn locked_or_panic(app: &App, gid: Id<GuildMarker>) -> Result<bool> {
    Ok(app.panic_runs.lock().unwrap().contains_key(&gid.get())
        || !tommybot_automod::store::locks(&app.pool, gid.get())
            .await?
            .is_empty())
}

/// A nuke run or panic mode is on: a backup now would save the damage as the newest image.
fn under_attack(app: &App, gid: u64) -> bool {
    app.panic_runs.lock().unwrap().contains_key(&gid)
        || app.nuke_runs.lock().unwrap().keys().any(|k| k.0 == gid)
}

/// True when the newest backup is older than `hours` (or there is none).
async fn due(app: &App, gid: Id<GuildMarker>, hours: u64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM backups WHERE guild_id = $1 \
         AND created_at > now() - make_interval(hours => $2))",
    )
    .bind(gid.get() as i64)
    .bind(hours.max(1) as i32)
    .fetch_one(&app.pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_drop_creates_and_deletes_only() {
        let role = RoleImg {
            id: 1,
            name: "a".into(),
            color: 0,
            permissions: 0,
            position: 1,
            hoist: false,
            mentionable: false,
            managed: false,
        };
        let mut p = Plan {
            roles: vec![
                Change::Create(role.clone()),
                Change::Update { live: 1, to: role },
                Change::Delete(2),
            ],
            channels: vec![Change::Delete(3)],
            ..Default::default()
        };
        let f = BackupsCfg {
            restore_roles: false,
            delete_channels: false,
            ..Default::default()
        };
        apply_filters(&mut p, &f);
        assert_eq!(p.roles.len(), 2);
        assert!(matches!(p.roles[0], Change::Update { .. }));
        assert!(p.channels.is_empty());
    }

    #[test]
    fn a_deleted_object_comes_from_the_newest_image_that_has_it() {
        // Administrator (1 << 3) and Send Messages (1 << 11).
        let role = |id, name: &str| RoleImg {
            id,
            name: name.into(),
            color: 0,
            permissions: (1 << 3) | if id == 2 { 1 << 11 } else { 0 },
            position: 1,
            hoist: false,
            mentionable: false,
            managed: false,
        };
        let img = |roles| Image {
            roles,
            ..Default::default()
        };
        // Newest first: role 1 is in the newest image, role 2 only in the older ones.
        let images = vec![
            img(vec![role(1, "new")]),
            img(vec![role(1, "old"), role(2, "mid")]),
            img(vec![role(2, "oldest"), role(3, "not asked")]),
        ];
        let (got, stripped) = newest_with(images.into_iter(), &HashSet::from([1, 2])).unwrap();
        let names: Vec<_> = got.roles.iter().map(|r| (r.id, r.name.as_str())).collect();
        assert_eq!(names, [(1, "new"), (2, "mid")]);
        // Role 2 came from an older image: Administrator is gone, Send Messages stays.
        assert_eq!(got.roles[1].permissions, 1 << 11);
        assert_eq!(stripped, HashSet::from([2]));
        assert_eq!(
            got.roles[0].permissions,
            1 << 3,
            "the newest image keeps its perms"
        );
        assert!(newest_with(std::iter::empty(), &HashSet::from([1])).is_none());
    }

    #[test]
    fn info_counts_categories_apart() {
        let ch = |id, kind| ChannelImg {
            id,
            name: "c".into(),
            kind,
            parent: None,
            position: 0,
            topic: None,
            nsfw: false,
            overwrites: vec![],
        };
        let img = Image {
            channels: vec![ch(1, 4), ch(2, 0), ch(3, 2)],
            webhooks: Some(vec![]),
            ..Default::default()
        };
        let t = info_text("abc123", 10, None, &img);
        assert!(t.contains("Backup `abc123`") && t.contains("Taken by TommyBot <t:10:R>"));
        assert!(
            t.contains("Roles: 0 · Channels: 2 · Categories: 1 · Webhooks: 0 · Server settings: ")
        );
    }

    /// D10/D11: load and clear need owner level plus a fresh 2FA, and load is refused in panic.
    #[tokio::test]
    async fn load_and_clear_need_an_owner_with_2fa_and_no_panic() {
        use crate::{bansync::tests::T, commands::backup_cmd, slash::BackupCmd};
        use tommybot_core::permits::Level;
        let Some(t) = T::new(200).await else { return };
        let (g, user) = (t.g(0), Id::new(t.base + 1));
        t.server_config(
            0,
            t.owner(),
            serde_json::json!({"backups": {"enabled": true}}),
        )
        .await;
        t.exec("DELETE FROM backups WHERE guild_id = $1").await;
        t.exec(r#"INSERT INTO backups (guild_id, image, code) VALUES ($1, '{"guild": 0, "name": "x", "roles": [], "channels": []}', 'h3load')"#).await;
        let custom = Level::Custom {
            rank: 1,
            perms: vec!["backups".into()],
            whitelist: vec![],
            limits: Default::default(),
        };
        let clear = || BackupCmd::Clear;
        let load = |confirm| BackupCmd::Load {
            id: "h3load".into(),
            confirm,
        };
        let run = |l: Level, c| {
            let app = &t.app;
            async move { backup_cmd(app, g, (user, &l), c).await.unwrap() }
        };
        assert!(run(custom.clone(), clear())
            .await
            .starts_with("Only the owner"));
        assert!(run(custom.clone(), load(true))
            .await
            .starts_with("Only the owner"));
        assert!(run(Level::ExtraOwner, clear()).await.contains("needs 2FA"));
        t.exec("INSERT INTO dash_session (id, user_id, username, access_token, csrf, step_up_until, expires_at) VALUES ('h3-' || $1::text, $1 + 1, 'o', 'x', 'x', now() + interval '5 minutes', now() + interval '1 day')").await;
        t.app.panic_runs.lock().unwrap().insert(
            g.get(),
            crate::app::PanicRun {
                lock_mod_commands: false,
                starting: false,
            },
        );
        assert!(run(Level::ExtraOwner, load(true))
            .await
            .starts_with("Cannot load a backup while"));
        // A backup delete is a clear one at a time (S-h3 2): the same bar.
        let delete = || BackupCmd::Delete {
            id: "h3load".into(),
        };
        assert!(run(Level::ExtraOwner, delete())
            .await
            .starts_with("Cannot delete backups while"));
        assert!(run(Level::ExtraOwner, clear())
            .await
            .starts_with("Cannot delete backups while"));
        t.app.panic_runs.lock().unwrap().clear();
        assert!(run(custom.clone(), delete())
            .await
            .starts_with("Only the owner"));
        t.exec("UPDATE dash_session SET step_up_until = now() - interval '1 minute' WHERE id = 'h3-' || $1::text").await;
        assert!(run(Level::Owner, delete()).await.contains("needs 2FA"));
        t.exec("UPDATE dash_session SET step_up_until = now() + interval '5 minutes' WHERE id = 'h3-' || $1::text").await;
        assert_eq!(run(Level::Owner, delete()).await, "Deleted backup `h3load`");
        assert_eq!(run(Level::Owner, clear()).await, "Deleted all backups");
        t.exec("DELETE FROM backups WHERE guild_id = $1").await;
    }
}
