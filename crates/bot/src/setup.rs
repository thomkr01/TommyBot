//! `w!setup run`: the Quarantine role and log channels, built step by step in one embed that is
//! edited as each step finishes (Wick's setup, features §3). The dashboard wizard does more
//! (Member role, staff permits); this is the part that runs from chat.
use anyhow::Result;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use tommybot_logging::{
    embed::{BLUE, NO, YES},
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
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::App,
    settings::{apply, save, Edit},
};

/// Wick's quarantine overwrite: no View Channel, Send Messages, Add Reactions or Connect.
const QUARANTINE_DENY: u64 = 1_051_712;

/// `misc.enforce_quarantine_perms`: puts the quarantine overwrite back on a channel that lacks it.
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
    if !ok {
        let o = PermissionOverwrite {
            id: role.cast(),
            kind: PermissionOverwriteType::Role,
            allow: None,
            deny: Some(want),
        };
        app.http.update_channel_permission(c.id, &o).await?;
    }
    Ok(())
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
            _ if i == done => "▶",
            _ => "▫",
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
            app.http
                .update_message(m.channel_id, m.id)
                .embeds(Some(&[e]))
                .await
                .ok();
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
    let role = match cfg
        .antinuke
        .quarantine_role
        .filter(|r| roles.iter().any(|x| x.id == *r))
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
    for c in http.guild_channels(gid).await?.models().await? {
        let o = PermissionOverwrite {
            id: role.cast(),
            kind: PermissionOverwriteType::Role,
            allow: None,
            deny: Some(deny),
        };
        // shortcut: a channel the bot cannot manage is skipped; run setup again after fixing access.
        http.update_channel_permission(c.id, &o).await.ok();
    }
    tick(6).await;
    // Directly under TommyBot's top role, so it can still manage the role.
    if let Some(pos) = u64::try_from(info.bot_top - 1).ok().filter(|p| *p > 0) {
        http.update_role_positions(
            gid,
            &[RolePosition {
                id: role,
                position: pos,
            }],
        )
        .await
        .ok();
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
    fn progress_marks_done_running_and_failed() {
        let text = |d, f| progress(d, f).build().description.unwrap();
        let t = text(2, None);
        assert!(t.starts_with(&format!("{YES} Initializing Setup\n{YES} Checking")));
        assert!(t.contains("▶ Checking TommyBot's role position"));
        assert!(text(3, Some("boom")).contains(&format!("{NO} Ensuring Quarantine Role")));
        assert!(text(9, None).contains("good to go"));
    }
}
