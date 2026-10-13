//! Runs automod lockdown plans against Discord and restores them from the stored snapshot.
use anyhow::Result;
use tommybot_automod::{
    lockdown::{plan, unlock_ops, LockKind, Op, RoleInfo},
    store,
};
use twilight_model::{
    guild::Permissions,
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{marker::GuildMarker, Id},
};

use crate::app::App;

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
        Op::EnableJoinLock(j) => {
            app.join_locks.lock().unwrap().insert(gid.get(), *j);
        }
        // shortcut: no twilight call for the INVITES_DISABLED feature, kick-lock still kicks joiners.
        Op::PauseInvites => {}
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

pub async fn unlock(app: &App, gid: Id<GuildMarker>) -> Result<()> {
    app.join_locks.lock().unwrap().remove(&gid.get());
    let Some(snap) = store::take_lock(&app.pool, gid.get()).await? else {
        return Ok(());
    };
    for op in unlock_ops(&snap) {
        exec(app, gid, &op).await?;
    }
    for (channel, role) in &snap.channel_denies {
        app.http
            .delete_channel_permission(Id::new(*channel))
            .role(Id::new(*role))
            .await?;
    }
    Ok(())
}
