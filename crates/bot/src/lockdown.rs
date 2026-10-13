//! Runs automod lockdown plans against Discord and restores them from the stored snapshot.
use anyhow::Result;
use tommybot_automod::{
    discord_api::MAX_INVITE_PAUSE_MS,
    lockdown::{plan, unlock_ops, JoinLock, LockKind, Op, RoleInfo, Snapshot},
    store,
};
use twilight_model::{
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{marker::GuildMarker, Id},
};

use crate::app::{now_secs, App};

async fn exec(app: &App, gid: Id<GuildMarker>, op: &Op) -> Result<()> {
    match op {
        Op::SetRolePerms { role, perms } => {
            let perms = Permissions::from_bits_truncate(*perms);
            app.http
                .update_role(gid, Id::new(*role))
                .permissions(perms)
                .await?;
        }
        Op::DenyChannelSend { channel, role } => {
            let o = PermissionOverwrite {
                id: Id::new(*role),
                kind: PermissionOverwriteType::Role,
                allow: None,
                deny: Some(Permissions::SEND_MESSAGES),
            };
            app.http
                .update_channel_permission(Id::new(*channel), &o)
                .await?;
        }
        Op::CreateLockedChannel => {
            app.http.create_guild_channel(gid, "server-locked").await?;
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
    Ok(())
}

/// Locks the guild unless a lock is already stored (a second snapshot would save locked perms).
pub async fn lock(app: &App, gid: Id<GuildMarker>, kind: LockKind) -> Result<bool> {
    let (locked,): (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM lock_snapshots WHERE guild_id = $1)")
            .bind(gid.get() as i64)
            .fetch_one(&app.pool)
            .await?;
    if locked {
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
    let plan = plan(&kind, gid.get(), &main_roles, &info, bot_position);
    // Saved first so a failure halfway can still be unlocked exactly.
    store::save_lock(&app.pool, gid.get(), &plan.snapshot).await?;
    for op in &plan.ops {
        exec(app, gid, op).await?;
    }
    Ok(true)
}

/// Join lock of the guild's stored snapshot, if it has one.
pub async fn join_lock(app: &App, gid: Id<GuildMarker>) -> Result<Option<JoinLock>> {
    load_join_lock(&app.pool, gid.get()).await
}

async fn load_join_lock(pool: &sqlx::PgPool, guild: u64) -> Result<Option<JoinLock>> {
    let row: Option<(sqlx::types::Json<Snapshot>,)> =
        sqlx::query_as("SELECT snapshot FROM lock_snapshots WHERE guild_id = $1")
            .bind(guild as i64)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(s,)| s.0.kind.as_ref().and_then(join_lock_of)))
}

fn join_lock_of(kind: &LockKind) -> Option<JoinLock> {
    match kind {
        LockKind::Join(j) => Some(*j),
        LockKind::All => Some(JoinLock::Ban),
        _ => None,
    }
}

pub async fn unlock(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    let Some(snap) = store::take_lock(&app.pool, gid.get()).await? else {
        return Ok(());
    };
    for op in unlock_ops(&snap) {
        exec(app, gid, &op).await?;
    }
    if snap.invites_paused {
        app.api.pause_invites(gid.get(), None).await?;
    }
    for (channel, role) in &snap.channel_denies {
        app.http
            .delete_channel_permission(Id::new(*channel))
            .role(Id::new(*role))
            .await?;
    }
    Ok(())
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
        store::take_lock(&pool, gid).await.unwrap();
        assert_eq!(load_join_lock(&pool, gid).await.unwrap(), None);
    }
}
