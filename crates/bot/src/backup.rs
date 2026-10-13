//! Server backups (spec 4.3): snapshot roles and channels into Postgres, and restore by syncing
//! the live server to an image. Restore uses its own HTTP client (own rate-limit bucket) so a
//! flood of normal traffic can't slow it down during a nuke.
use anyhow::Result;
use sqlx::types::Json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tommybot_antinuke::{map_overwrites, plan, Change, ChannelImg, Image, OverwriteImg, RoleImg};
use twilight_http::Client;
use twilight_model::{
    channel::{
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
        ChannelType,
    },
    guild::{Permissions, RolePosition},
    id::{marker::GuildMarker, Id},
};

use crate::app::App;

/// Backups kept per guild; older ones are dropped on save.
const KEEP: i64 = 10;
pub const INTERVAL: Duration = Duration::from_secs(3 * 3600);

// Role::color is deprecated upstream in favour of `colors`, but create/update_role still take it.
#[allow(deprecated)]
pub async fn snapshot(http: &Client, gid: Id<GuildMarker>) -> Result<Image> {
    let name = http.guild(gid).await?.model().await?.name;
    let roles = http.roles(gid).await?.models().await?;
    let channels = http.guild_channels(gid).await?.models().await?;
    Ok(Image {
        guild: gid.get(),
        name,
        roles: roles
            .into_iter()
            .map(|r| RoleImg {
                id: r.id.get(),
                name: r.name,
                color: r.color,
                permissions: r.permissions.bits(),
                position: r.position,
                hoist: r.hoist,
                mentionable: r.mentionable,
                managed: r.managed,
            })
            .collect(),
        channels: channels
            .into_iter()
            .map(|c| ChannelImg {
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
            })
            .collect(),
    })
}

pub async fn save(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let image = snapshot(&app.http, gid).await?;
    sqlx::query("INSERT INTO backups (guild_id, image) VALUES ($1, $2)")
        .bind(gid.get() as i64)
        .bind(Json(&image))
        .execute(&app.pool)
        .await?;
    sqlx::query(
        "DELETE FROM backups WHERE guild_id = $1 AND id NOT IN \
         (SELECT id FROM backups WHERE guild_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2)",
    )
    .bind(gid.get() as i64)
    .bind(KEEP)
    .execute(&app.pool)
    .await?;
    Ok(())
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

/// Panic-mode restore: sync to the latest image, never deleting the quarantine role, log channels
/// or the lock channel, and keeping the server lock. `None` means there is no backup yet.
pub async fn restore_latest(app: &App, gid: Id<GuildMarker>) -> Result<Option<usize>> {
    let Some(image) = latest(app, gid).await? else {
        return Ok(None);
    };
    let cfg = app.cfg(gid.get()).await?;
    let mut keep: HashSet<u64> = cfg
        .antinuke
        .quarantine_role
        .iter()
        .map(|r| r.get())
        .collect();
    keep.extend(
        cfg.logging
            .fallback
            .iter()
            .chain(cfg.logging.routes.values())
            .map(|c| c.get()),
    );
    Ok(Some(restore(app, gid, &image, &keep, true).await?))
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
            allow: Permissions::from_bits_truncate(o.allow),
            deny: Permissions::from_bits_truncate(o.deny),
        })
        .collect()
}

/// Syncs the server to `image` and returns how many changes were made. `respect_lock` keeps live
/// role permissions and channel overwrites (panic mode locks the server first). One failed change
/// is logged and skipped, so a single missing permission can't abort the whole restore.
pub async fn restore(
    app: &App,
    gid: Id<GuildMarker>,
    image: &Image,
    keep: &HashSet<u64>,
    respect_lock: bool,
) -> Result<usize> {
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
    let mut done = 0;
    let mut id_map = std::mem::take(&mut p.id_map);

    for ch in &p.roles {
        let r: Result<()> = async {
            match ch {
                Change::Create(r) => {
                    let new = http
                        .create_role(gid)
                        .name(&r.name)
                        .color(r.color)
                        .permissions(Permissions::from_bits_truncate(r.permissions))
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
                        .permissions(Permissions::from_bits_truncate(to.permissions))
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
        count(r, &mut done, "role");
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
        count(r, &mut done, "channel");
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
    Ok(done)
}

fn count(r: Result<()>, done: &mut usize, what: &str) {
    match r {
        Ok(()) => *done += 1,
        Err(e) => tracing::warn!(?e, what, "restore step failed"),
    }
}

/// Backs up every guild the bot is in, every 3 hours.
pub async fn auto_backup(app: std::sync::Arc<App>) {
    let mut tick = tokio::time::interval(INTERVAL);
    loop {
        tick.tick().await;
        match app.http.current_user_guilds().await {
            Ok(r) => {
                for g in r.models().await.unwrap_or_default() {
                    if let Err(e) = save(&app, g.id).await {
                        tracing::warn!(?e, guild = g.id.get(), "auto backup failed");
                    }
                }
            }
            Err(e) => tracing::warn!(?e, "listing guilds for backup failed"),
        }
    }
}
