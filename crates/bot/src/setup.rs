//! `t!setup run`: the Quarantine role and log channels, built step by step in one embed that is
//! edited as each step finishes (setup, features §3). The dashboard wizard does more
//! (Member role, staff permits); this is the part that runs from chat.
use anyhow::Result;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use tommybot_logging::{
    embed::{BLUE, NO, YES},
    emoji::{RUNNING, WAITING},
    Log,
};
use twilight_model::{
    channel::permission_overwrite::{
        PermissionOverwrite as ModelOverwrite, PermissionOverwriteType as ModelOverwriteType,
    },
    channel::Channel,
    gateway::payload::incoming::MessageCreate,
    guild::{Permissions, RolePosition},
    http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    id::{
        marker::{GuildMarker, RoleMarker},
        Id,
    },
};

use twilight_http::request::AuditLogReason;

use crate::{
    app::App,
    settings::{apply, save, Edit},
};

use tommybot_core::sections::QUARANTINE_DENY;

/// `misc.enforce_quarantine_perms`: puts the quarantine overwrite back on a channel that lacks it.
/// The verification channel is left alone: members in the join quarantine must see it.
pub async fn enforce_quarantine(app: &App, c: &Channel) -> Result<()> {
    let Some(gid) = c.guild_id else {
        return Ok(());
    };
    let cfg = app.cfg(gid.get()).await?;
    let Some(role) = cfg
        .antinuke
        .quarantine_role
        .filter(|_| cfg.misc.enforce_quarantine_perms)
    else {
        return Ok(());
    };
    let want = Permissions::from_bits_truncate(QUARANTINE_DENY);
    let ok =
        c.permission_overwrites.iter().flatten().any(|o| {
            o.id.get() == role.get() && o.deny.contains(want) && !o.allow.intersects(want)
        });
    if ok {
        return Ok(());
    }
    let verification = {
        // The bot makes and saves a verification channel under this lock, and its ChannelCreate
        // can come in before the save: wait for it, then read the saved channel.
        let _made = crate::verify::CHANNEL_LOCK.lock().await;
        let saved = app.cfg(gid.get()).await?.verification.channel;
        // The dashboard's Setup Channel saves after it made the channel, without the lock: until
        // then its exact overwrite (read, not send) marks it.
        let ows = c.permission_overwrites.as_deref().unwrap_or_default();
        saved == Some(c.id) || (saved.is_none() && verification_overwrite(ows, role))
    };
    if verification {
        return Ok(());
    }
    let o = PermissionOverwrite {
        id: role.cast(),
        kind: PermissionOverwriteType::Role,
        allow: None,
        deny: Some(want),
    };
    app.http
        .update_channel_permission(c.id, &o)
        .reason("Sanitizing the channel: quarantine permissions were missing")
        .await?;
    Ok(())
}

/// The quarantine role's overwrite in a new verification channel: read, but not send.
fn verification_overwrite(overwrites: &[ModelOverwrite], role: Id<RoleMarker>) -> bool {
    let read = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
    overwrites
        .iter()
        .any(|o| o.id == role.cast() && o.allow == read && o.deny == Permissions::SEND_MESSAGES)
}

const STEPS: [&str; 9] = [
    "Initializing Setup",
    "Checking for permissions",
    "Checking TommyBot's role position",
    "Ensuring Quarantine Role",
    "Setting up Quarantine role across all channels",
    "Ensuring Quarantine role is placed properly",
    "Ensuring #tommybot-logs channel",
    "Ensuring #mod-logs channel",
    "Import new changes",
];

/// One line per step: done, running (`>`), or waiting.
fn progress(done: usize, failed: Option<&str>) -> Log {
    let mut log = Log::new("TommyBot Setup", BLUE);
    for (i, s) in STEPS.iter().enumerate() {
        let mark = match i {
            _ if i < done => YES,
            _ if i == done && failed.is_some() => NO,
            _ if i == done => RUNNING,
            _ => WAITING,
        };
        log = log.text(&format!("{mark} {s}"));
    }
    match failed {
        Some(why) => log.text(&format!("**{why}**")),
        None if done >= STEPS.len() => log.text("**Looks like you are good to go!**"),
        None => log,
    }
}

pub async fn run(app: &App, msg: &MessageCreate, gid: Id<GuildMarker>) -> Result<()> {
    let m = app
        .http
        .create_message(msg.channel_id)
        .embeds(&[progress(0, None).build()])
        .await?
        .model()
        .await?;
    let shown = AtomicUsize::new(0);
    let tick = |n: usize| {
        shown.store(n, Relaxed);
        let e = progress(n, None).build();
        async move {
            let r = app
                .http
                .update_message(m.channel_id, m.id)
                .embeds(Some(&[e]))
                .await;
            crate::http_err::best_effort(r, "update setup progress");
        }
    };
    let log = match steps(app, gid, tick).await {
        Ok(()) => progress(STEPS.len(), None),
        Err(e) => progress(shown.load(Relaxed), Some(&e.to_string())),
    };
    app.http
        .update_message(m.channel_id, m.id)
        .embeds(Some(&[log.build()]))
        .await?;
    Ok(())
}

async fn steps<F, Fut>(app: &App, gid: Id<GuildMarker>, tick: F) -> Result<()>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let http = &app.http;
    tick(1).await;
    let info = app.guild_info(gid).await?;
    let me = http.guild_member(gid, app.me).await?.model().await?;
    let perms = std::iter::once(Id::new(gid.get()))
        .chain(me.roles.iter().copied())
        .filter_map(|r| info.role_perms.get(&r))
        .fold(0, |a, b| a | b);
    let perms = Permissions::from_bits_truncate(perms);
    tick(2).await;
    if !perms.contains(Permissions::ADMINISTRATOR)
        && !perms.contains(Permissions::MANAGE_ROLES | Permissions::MANAGE_CHANNELS)
    {
        anyhow::bail!(
            "TommyBot lacks certain permissions to function properly. Give it Manage Roles and Manage Channels, or Administrator."
        );
    }
    tick(3).await;
    let cfg = app.cfg(gid.get()).await?;
    let roles = http.roles(gid).await?.models().await?;
    tick(4).await;
    // A saved role that is gone, or one that cannot be a quarantine role, is replaced.
    let role = match cfg
        .antinuke
        .quarantine_role
        .filter(|r| roles.iter().any(|x| x.id == *r) && info.unfit(gid, *r).is_none())
    {
        Some(r) => r,
        None => {
            http.create_role(gid)
                .name("Quarantine")
                .permissions(Permissions::empty())
                .await?
                .model()
                .await?
                .id
        }
    };
    tick(5).await;
    let deny = Permissions::from_bits_truncate(QUARANTINE_DENY);
    // Not the verification channel: members in the join quarantine must see it.
    let channels = http.guild_channels(gid).await?.models().await?;
    for c in channels
        .iter()
        .filter(|c| cfg.verification.channel != Some(c.id))
    {
        let o = PermissionOverwrite {
            id: role.cast(),
            kind: PermissionOverwriteType::Role,
            allow: None,
            deny: Some(deny),
        };
        // shortcut: a channel the bot cannot manage is skipped; run setup again after fixing access.
        let r = http.update_channel_permission(c.id, &o).await;
        crate::http_err::best_effort(r, "deny quarantine role in channel");
    }
    tick(6).await;
    if let Some(pos) = u64::try_from(info.bot_top - 1).ok().filter(|p| *p > 0) {
        let r = http
            .update_role_positions(
                gid,
                &[RolePosition {
                    id: role,
                    position: pos,
                }],
            )
            .await;
        crate::http_err::best_effort(r, "move quarantine role");
    }
    tick(7).await;
    let main = match cfg.logging.fallback {
        Some(c) => c,
        None => log_channel(app, gid, "tommybot-logs").await?,
    };
    tick(8).await;
    let modlog = match cfg
        .logging
        .routes
        .get(&tommybot_logging::LogKind::ModCommand)
    {
        Some(c) => *c,
        None => log_channel(app, gid, "mod-logs").await?,
    };
    tick(9).await;
    let saved = save(app, gid, app.me.get(), &["antinuke", "logging"], |doc| {
        apply(
            doc,
            "antinuke.quarantine_role",
            Edit::Put(json!(role.to_string())),
        );
        apply(doc, "logging.fallback", Edit::Put(json!(main.to_string())));
        apply(
            doc,
            "logging.routes.mod_command",
            Edit::Put(json!(modlog.to_string())),
        );
        Ok(())
    })
    .await?;
    saved.map(drop).map_err(anyhow::Error::msg)
}

/// A text channel hidden from @everyone that the bot can write to.
async fn log_channel(
    app: &App,
    gid: Id<GuildMarker>,
    name: &str,
) -> Result<Id<twilight_model::id::marker::ChannelMarker>> {
    let hide = ModelOverwrite {
        id: gid.cast(),
        kind: ModelOverwriteType::Role,
        allow: Permissions::empty(),
        deny: Permissions::VIEW_CHANNEL,
    };
    let me = ModelOverwrite {
        id: app.me.cast(),
        kind: ModelOverwriteType::Member,
        allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS,
        deny: Permissions::empty(),
    };
    Ok(app
        .http
        .create_guild_channel(gid, name)
        .permission_overwrites(&[hide, me])
        .await?
        .model()
        .await?
        .id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_verification_overwrite_marks_a_new_verification_channel() {
        let q = Id::new(5);
        let ow = |id: u64, allow: Permissions, deny: Permissions| ModelOverwrite {
            id: Id::new(id),
            kind: ModelOverwriteType::Role,
            allow,
            deny,
        };
        let read = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
        let send = Permissions::SEND_MESSAGES;
        assert!(verification_overwrite(&[ow(5, read, send)], q));
        assert!(!verification_overwrite(&[ow(6, read, send)], q)); // another role
        assert!(!verification_overwrite(
            &[ow(5, read | send, Permissions::empty())],
            q
        ));
        assert!(!verification_overwrite(
            &[ow(5, Permissions::VIEW_CHANNEL, send)],
            q
        ));
        assert!(!verification_overwrite(&[], q));
    }

    #[test]
    fn progress_marks_done_running_and_failed() {
        let text = |d, f| progress(d, f).build().description.unwrap();
        let t = text(2, None);
        assert!(t.starts_with(&format!("{YES} Initializing Setup\n{YES} Checking")));
        assert!(t.contains("▶ Checking TommyBot's role position"));
        assert!(text(3, Some("boom")).contains(&format!("{NO} Ensuring Quarantine Role")));
        assert!(text(9, None).contains("good to go"));
    }

    #[test]
    fn quarantine_denies_access_invites_and_threads() {
        let p = Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::ADD_REACTIONS
            | Permissions::CONNECT
            | Permissions::CREATE_INVITE
            | Permissions::SEND_MESSAGES_IN_THREADS
            | Permissions::CREATE_PUBLIC_THREADS
            | Permissions::CREATE_PRIVATE_THREADS;
        assert_eq!(p.bits(), QUARANTINE_DENY);
        // A sanitized channel blocks at least invites, viewing, sending and every thread kind.
        let must = Permissions::CREATE_INVITE
            | Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::CREATE_PUBLIC_THREADS
            | Permissions::CREATE_PRIVATE_THREADS
            | Permissions::SEND_MESSAGES_IN_THREADS;
        assert!(Permissions::from_bits_truncate(QUARANTINE_DENY).contains(must));
    }
}
