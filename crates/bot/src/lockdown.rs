//! Runs automod lockdown plans against Discord and restores them from the stored snapshot.
use anyhow::Result;
use tommybot_automod::{
    discord_api::MAX_INVITE_PAUSE_MS,
    lockdown::{plan, unlock_ops, JoinLock, LockKind, Op, RoleInfo, Snapshot, SEND_MESSAGES},
    store,
};
use twilight_model::{
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::{now_secs, App},
    events::send_log,
};
use tommybot_logging::{
    embed::{ALERT, DONE, YES},
    Log, LogKind,
};

/// Runs one op; returns the channel it created, if any.
async fn exec(app: &App, gid: Id<GuildMarker>, op: &Op, snap: &Snapshot) -> Result<Option<u64>> {
    match op {
        Op::SetRolePerms { role, perms } => {
            let perms = Permissions::from_bits_truncate(*perms);
            app.http
                .update_role(gid, Id::new(*role))
                .permissions(perms)
                .await?;
        }
        Op::DenyChannelSend { channel, role } => {
            // Other bits of an existing overwrite stay; unlock puts the old one back.
            let (allow, deny) = snap
                .prior
                .iter()
                .find(|p| p.0 == *channel && p.1 == *role)
                .map_or((0, 0), |p| (p.2, p.3));
            let o = PermissionOverwrite {
                id: Id::new(*role),
                kind: PermissionOverwriteType::Role,
                allow: Some(Permissions::from_bits_truncate(allow & !SEND_MESSAGES)),
                deny: Some(Permissions::from_bits_truncate(deny | SEND_MESSAGES)),
            };
            app.http
                .update_channel_permission(Id::new(*channel), &o)
                .await?;
        }
        Op::CreateLockedChannel => {
            // @everyone may read it but not write, so members see why the server went quiet.
            let everyone = twilight_model::channel::permission_overwrite::PermissionOverwrite {
                id: gid.cast(),
                kind: twilight_model::channel::permission_overwrite::PermissionOverwriteType::Role,
                allow: Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
                deny: Permissions::SEND_MESSAGES,
            };
            let ch = app
                .http
                .create_guild_channel(gid, "server-locked")
                .permission_overwrites(&[everyone])
                .await?
                .model()
                .await?;
            app.http
                .create_message(ch.id)
                .content("🔒 **The server is locked down.** Staff are on it; channels open again when the lock ends.")
                .await?;
            return Ok(Some(ch.id.get()));
        }
        // No-op: the saved snapshot records the join lock and `join_lock` reads it from there.
        Op::EnableJoinLock(_) => {}
        Op::PauseInvites => {
            let now = now_secs() * 1000;
            app.api
                .pause_invites(gid.get(), Some((now, now + MAX_INVITE_PAUSE_MS)))
                .await?;
        }
    }
    Ok(None)
}

/// Locks the guild unless an active lock already covers the same part (its snapshot would save
/// locked perms).
pub async fn lock(app: &App, gid: Id<GuildMarker>, kind: LockKind) -> Result<bool> {
    let active = store::locks(&app.pool, gid.get()).await?;
    if active
        .iter()
        .filter_map(|s| s.kind.as_ref())
        .any(|k| k.overlaps(&kind))
    {
        return Ok(false);
    }
    let cfg = app.cfg(gid.get()).await?;
    let roles = app.http.roles(gid).await?.models().await?;
    let mine = app
        .http
        .guild_member(gid, app.me)
        .await?
        .model()
        .await?
        .roles;
    let bot_position = roles
        .iter()
        .filter(|r| mine.contains(&r.id))
        .map(|r| r.position as i32)
        .max()
        .unwrap_or(0);
    let info: Vec<RoleInfo> = roles
        .iter()
        .map(|r| RoleInfo {
            id: r.id.get(),
            perms: r.permissions.bits(),
            position: r.position as i32,
            managed: r.managed,
        })
        .collect();
    let main_roles: Vec<u64> = cfg.lockdown.main_roles.iter().map(|r| r.get()).collect();
    let mut plan = plan(&kind, gid.get(), &main_roles, &info, bot_position);
    if let LockKind::Channel(c) = kind {
        let ch = app.http.channel(Id::new(c)).await?.model().await?;
        plan.snapshot.prior = ch
            .permission_overwrites
            .unwrap_or_default()
            .iter()
            .filter(|o| plan.snapshot.channel_denies.contains(&(c, o.id.get())))
            .map(|o| (c, o.id.get(), o.allow.bits(), o.deny.bits()))
            .collect();
    }
    // Saved first so a failure halfway can still be unlocked exactly.
    store::save_lock(&app.pool, gid.get(), &plan.snapshot).await?;
    for op in &plan.ops {
        if let Some(created) = exec(app, gid, op, &plan.snapshot).await? {
            plan.snapshot.locked_channel = Some(created);
            store::save_lock(&app.pool, gid.get(), &plan.snapshot).await?;
        }
    }
    announce(
        app,
        gid,
        Log::new("Server Locked", ALERT)
            .text(&format!("{YES} **The server has been locked down.**")),
    )
    .await;
    Ok(true)
}

/// Kinds of the guild's active locks, oldest first.
pub async fn active(app: &App, gid: Id<GuildMarker>) -> Result<Vec<LockKind>> {
    Ok(store::locks(&app.pool, gid.get())
        .await?
        .into_iter()
        .filter_map(|s| s.kind)
        .collect())
}

/// Join lock of the guild's stored snapshot, if it has one.
pub async fn join_lock(app: &App, gid: Id<GuildMarker>) -> Result<Option<JoinLock>> {
    load_join_lock(&app.pool, gid.get()).await
}

async fn load_join_lock(pool: &sqlx::PgPool, guild: u64) -> Result<Option<JoinLock>> {
    let locks = store::locks(pool, guild).await?;
    Ok(locks
        .iter()
        .find_map(|s| s.kind.as_ref().and_then(join_lock_of)))
}

fn join_lock_of(kind: &LockKind) -> Option<JoinLock> {
    kind.covers().2
}

async fn announce(app: &App, gid: Id<GuildMarker>, log: Log) {
    let on = app
        .cfg(gid.get())
        .await
        .is_ok_and(|c| c.lockdown_prefs.updates);
    if on {
        if let Err(e) = send_log(app, gid.get(), LogKind::ModCommand, log).await {
            tracing::warn!(?e, "lockdown update log failed");
        }
    }
}

/// Lifts the active locks that overlap `which`, or all of them; returns how many were lifted.
pub async fn unlock(app: &App, gid: Id<GuildMarker>, which: Option<&LockKind>) -> Result<usize> {
    let mut lifted = 0;
    for snap in store::locks(&app.pool, gid.get()).await? {
        let hit = match (which, &snap.kind) {
            (Some(w), Some(k)) => w.overlaps(k),
            (Some(_), None) => false,
            (None, _) => true,
        };
        if !hit || !store::take_lock(&app.pool, gid.get(), &snap).await? {
            continue;
        }
        let current: Vec<(u64, u64)> = app
            .http
            .roles(gid)
            .await?
            .models()
            .await?
            .iter()
            .map(|r| (r.id.get(), r.permissions.bits()))
            .collect();
        for op in unlock_ops(&snap, &current) {
            exec(app, gid, &op, &snap).await?;
        }
        if snap.invites_paused {
            app.api.pause_invites(gid.get(), None).await?;
        }
        for &(channel, role) in &snap.channel_denies {
            match snap.prior.iter().find(|p| p.0 == channel && p.1 == role) {
                Some(&(_, _, allow, deny)) => {
                    let o = PermissionOverwrite {
                        id: Id::new(role),
                        kind: PermissionOverwriteType::Role,
                        allow: Some(Permissions::from_bits_truncate(allow)),
                        deny: Some(Permissions::from_bits_truncate(deny)),
                    };
                    app.http
                        .update_channel_permission(Id::new(channel), &o)
                        .await?;
                }
                None => {
                    app.http
                        .delete_channel_permission(Id::new(channel))
                        .role(Id::new(role))
                        .await?;
                }
            }
        }
        if let Some(c) = snap.locked_channel {
            app.http.delete_channel(Id::new(c)).await.ok();
        }
        lifted += 1;
    }
    if lifted > 0 {
        announce(
            app,
            gid,
            Log::new("Server Unlocked", DONE)
                .text(&format!("{YES} **The server has been unlocked.**")),
        )
        .await;
    }
    Ok(lifted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_lock_maps_from_kind() {
        assert_eq!(
            join_lock_of(&LockKind::Join(JoinLock::Kick)),
            Some(JoinLock::Kick)
        );
        assert_eq!(join_lock_of(&LockKind::All), Some(JoinLock::Ban));
        assert_eq!(join_lock_of(&LockKind::Server), None);
        assert_eq!(join_lock_of(&LockKind::Channel(1)), None);
    }

    /// Runs only when DATABASE_URL points at a scratch Postgres.
    #[tokio::test]
    async fn join_lock_reads_saved_snapshot() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = tommybot_core::db::connect(&url).await.unwrap();
        let gid = 4_000_000_000_000_000_000 + std::process::id() as u64;
        let p = plan(&LockKind::Join(JoinLock::Kick), gid, &[], &[], 0);
        store::save_lock(&pool, gid, &p.snapshot).await.unwrap();
        assert_eq!(
            load_join_lock(&pool, gid).await.unwrap(),
            Some(JoinLock::Kick)
        );
        store::take_lock(&pool, gid, &p.snapshot).await.unwrap();
        assert_eq!(load_join_lock(&pool, gid).await.unwrap(), None);
    }
}
