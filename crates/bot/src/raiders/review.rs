//! Staff and bot owner side: alert buttons, the bot owner's DMs, the hourly sweep and the
//! `raider` command.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Mutex};
use tommybot_core::{permits::Level, raiders, sections::RaiderCategory};
use tommybot_logging::{embed, emoji::WARNING};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::component::{ActionRow, Component},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};

use super::{
    cat_name, fan,
    join::{entry, exempt, listed},
    member_roles, parse_cat, BUTTON, PREFIX, STRONG,
};
use crate::{
    app::{now_secs, App},
    commands::{self, ModKind},
    modcmds::{self, ModCmd},
    owner::live_owner,
    reply::{self, md},
};

/// `raider:{ban|kick|quarantine|allow}:{user}` under an alert. The id is data only: the server is
/// where the button was pressed, the clicker's permit there decides, and the DB is read again.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(BUTTON) else {
        return Ok(false);
    };
    let clicked = async {
        let (action, user) = rest.split_once(':')?;
        let user = user.parse().ok().and_then(Id::new_checked)?;
        let here = (i.guild_id?, i.channel.as_ref()?.id);
        Some(click(app, here, i.author_id()?, action, user).await)
    };
    reply::answer_click(app, i, clicked).await?;
    Ok(true)
}

/// What a click needs: the permit for the action (any of the three for Allow).
pub(super) fn click_permit(level: &Level, action: &str) -> Option<bool> {
    Some(match action {
        "ban" | "kick" | "quarantine" => level.has(action),
        "allow" => ["ban", "kick", "quarantine"].iter().any(|p| level.has(p)),
        _ => return None,
    })
}

pub(super) async fn click(
    app: &App,
    (gid, channel): (Id<GuildMarker>, Id<ChannelMarker>),
    clicker: Id<UserMarker>,
    action: &str,
    user: Id<UserMarker>,
) -> Result<String> {
    let owner = live_owner(app, gid)
        .await?
        .context("the server is not visible")?;
    let level = app.level(gid, owner, clicker).await?;
    match click_permit(&level, action) {
        None => return Ok("This button does not work here.".into()),
        Some(false) if action == "allow" => {
            return Ok(
                "You need the ban, kick or quarantine permit in this server to do this.".into(),
            )
        }
        Some(false) => {
            return Ok(format!(
                "You need the {action} permit in this server to do this."
            ))
        }
        Some(true) => {}
    }
    let row: Option<String> = sqlx::query_scalar(
        "SELECT action FROM raider_actions WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    if row.as_deref() != Some("alert") {
        return Ok("This alert was already handled.".into());
    }
    if action == "allow" {
        // Like the other buttons: refused during panic's mod lock, and counted for the rate limit
        // under the permit that lets the clicker press it.
        let permit = ["ban", "kick", "quarantine"]
            .into_iter()
            .find(|p| level.has(p))
            .unwrap_or("ban");
        if let Some(why) = commands::gated(app, gid, clicker, &level, permit).await? {
            return Ok(why);
        }
        let (g, u, c) = (gid.get() as i64, user.get() as i64, clicker.get() as i64);
        raiders::allow(&app.pool, g, u, c).await?;
        return Ok(format!(
            "<@{user}> is allowed here: the Raider List will not act on them in this server again."
        ));
    }
    let Some(entry) = listed(app, &app.cfg(gid.get()).await?.raider_list, user).await? else {
        return Ok(
            "This account is no longer on the Raider List here, so nothing was done.".into(),
        );
    };
    // Their level may have changed since the alert (made staff, whitelisted).
    if exempt(app, gid, user, None).await? {
        return Ok(
            "This member is exempt here now (staff, whitelisted or allowed), so nothing was done."
                .into(),
        );
    }
    let why = format!("{PREFIX} {}", entry.category);
    let me = (clicker, &level);
    let (g, u) = (gid.get() as i64, user.get() as i64);
    // Claimed before acting, so two staff pressing at once cannot both ban (or ban and kick).
    let claimed = sqlx::query(
        "UPDATE raider_actions SET action = $3 WHERE guild_id = $1 AND user_id = $2 AND action = 'alert'",
    )
    .bind(g)
    .bind(u)
    .bind(action)
    .execute(&app.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Ok("This alert was already handled.".into());
    }
    let done = match action {
        "ban" | "kick" => {
            let kind = if action == "ban" {
                ModKind::Ban
            } else {
                ModKind::Kick
            };
            commands::apply(app, gid, owner, me, kind, user, Some(&why), None, 0).await
        }
        _ => {
            let cmd = ModCmd::Quarantine { user: user.get() };
            modcmds::run(app, gid, owner, me, channel, cmd, (Some(&why), None), false).await
        }
    };
    let case = done.as_ref().ok().and_then(|d| d.case);
    // With a case the claim stands; otherwise nothing was done and the buttons work again.
    let back = match case {
        Some(n) => sqlx::query(
            "UPDATE raider_actions SET case_no = $3 WHERE guild_id = $1 AND user_id = $2",
        )
        .bind(g)
        .bind(u)
        .bind(n),
        None => sqlx::query(
            "UPDATE raider_actions SET action = 'alert' WHERE guild_id = $1 AND user_id = $2",
        )
        .bind(g)
        .bind(u),
    };
    back.execute(&app.pool).await?;
    let done = done?;
    Ok(done.text)
}

/// The dashboard review queue, optionally opened on one user and decision.
fn queue_url(user: Option<(u64, &str)>) -> Option<String> {
    let base = std::env::var("DASHBOARD_URL")
        .ok()
        .filter(|b| !b.is_empty())?;
    let base = base.trim_end_matches('/');
    Some(match user {
        Some((u, d)) => format!("{base}/#/raiders?user={u}&do={d}"),
        None => format!("{base}/#/raiders"),
    })
}

/// The unix time a Discord id was made.
fn id_secs(id: u64) -> i64 {
    (((id >> 22) + 1_420_070_400_000) / 1000) as i64
}

/// One evidence row of the DM: (server, owner-less) server, kind, ref, detail, time.
type EvRow = (i64, String, String, Value, i64);

/// Everything the bot owner needs to verify an automatic listing, as DM text.
pub(crate) fn listed_text(
    user: u64,
    who: Option<(&str, bool)>,
    owners: usize,
    rows: &[(String, Option<u64>, EvRow)],
) -> String {
    let (name, bot) = who.map_or(("unknown".into(), "unknown"), |(n, b)| {
        (md(n), if b { "yes" } else { "no" })
    });
    let mut t = format!(
        "{WARNING} **Raider List: <@{user}> was listed by itself.**\nUser: `{user}` {name}, account made <t:{}:R>, bot: {bot}\nStrong evidence from **{owners}** different server owners, so every server with the list on now acts on them. Check it and press **Verify** or **Remove** (both need 2FA on the dashboard).\n**Evidence:**",
        id_secs(user)
    );
    for (server, members, (g, kind, r, detail, at)) in rows.iter().take(10) {
        let members = members.map_or(String::new(), |m| format!(", {m} members"));
        let mut extra = vec![format!("ref `{}`", md(r))];
        for (k, label) in [
            ("case", "case"),
            ("case_no", "case"),
            ("count", "count"),
            ("filter", "by"),
        ] {
            if let Some(v) = detail.get(k).filter(|v| !v.is_null()) {
                let v = v.as_str().map_or(v.to_string(), str::to_string);
                extra.push(format!("{label} {}", md(&v)));
            }
        }
        t.push_str(&format!(
            "\n• **{}** (`{g}`{members}): `{kind}` <t:{at}:R>, {}",
            md(&embed::clip(server, 60)),
            extra.join(", ")
        ));
    }
    if rows.len() > 10 {
        t.push_str(&format!("\n• +{} more on the dashboard", rows.len() - 10));
    }
    embed::clip(&t, 1900)
}

/// DMs the bot owner the full evidence of an automatic listing, with Verify and Remove links.
pub(super) async fn listed_dm(app: &App, user: Id<UserMarker>) {
    let Some(owner) = app.bot_owner else { return };
    let rows: Vec<EvRow> = match sqlx::query_as(
        "SELECT source_guild, kind, ref, detail, extract(epoch FROM created_at)::bigint
         FROM raider_evidence WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user.get() as i64)
    .fetch_all(&app.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return tracing::warn!(?e, "raider list DM: reading the evidence failed"),
    };
    let owners = sqlx::query_scalar::<_, i64>(
        "SELECT count(DISTINCT source_owner) FROM raider_evidence WHERE user_id = $1 AND kind = ANY($2)",
    )
    .bind(user.get() as i64)
    .bind(STRONG.as_slice())
    .fetch_one(&app.pool)
    .await
    .unwrap_or(0);
    let mut named = vec![];
    for r in rows.into_iter().take(11) {
        let g = Id::new(r.0 as u64);
        let info = async { anyhow::Ok(app.http.guild(g).with_counts(true).await?.model().await?) };
        let (name, members) = match info.await {
            Ok(g) => (g.name, g.approximate_member_count),
            Err(_) => ("a server".into(), None),
        };
        named.push((name, members, r));
    }
    let who = async { anyhow::Ok(app.http.user(user).await?.model().await?) }
        .await
        .ok();
    let who = who.as_ref().map(|u| (u.name.as_str(), u.bot));
    let text = listed_text(user.get(), who, owners as usize, &named);
    let links: Vec<Component> = [("Verify", "verify"), ("Remove", "remove")]
        .into_iter()
        .filter_map(|(l, d)| Some(crate::welcome::link(l, queue_url(Some((user.get(), d)))?)))
        .collect();
    let rows = if links.is_empty() {
        vec![]
    } else {
        vec![Component::ActionRow(ActionRow {
            id: None,
            components: links,
        })]
    };
    reply::dm_buttons(app, owner, &text, &rows).await;
}

/// Hourly: scam listings past their end are removed (and their lifts queued), rejected entries go
/// after 30 days and unreviewed pending ones after 90, with their evidence; reviews stay. Then the
/// daily digest for the bot owner.
pub async fn sweep(app: &App) {
    if let Err(e) = expire(app).await {
        tracing::warn!(?e, "raider list: expiry failed");
    }
    if let Err(e) = digest(app).await {
        tracing::warn!(?e, "raider list: digest failed");
    }
}

pub(super) async fn expire(app: &App) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let ended: Vec<i64> = sqlx::query_scalar(
        "UPDATE raiders SET status = 'removed', updated_at = now()
         WHERE status = 'confirmed' AND expires_at <= now() RETURNING user_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for u in ended {
        sqlx::query(
            "INSERT INTO raider_reviews (actor, user_id, from_status, to_status, note)
             VALUES ($1, $2, 'confirmed', 'removed', 'scam listing expired')",
        )
        .bind(app.me.get() as i64)
        .bind(u)
        .execute(&mut *tx)
        .await?;
        fan(&mut *tx, "raider_lift", Id::new(u as u64)).await?;
    }
    tx.commit().await?;
    sqlx::query(
        "WITH old AS (DELETE FROM raiders
                      WHERE (status = 'rejected' AND COALESCE(reviewed_at, updated_at) < now() - interval '30 days')
                         OR (status = 'pending' AND reviewed_at IS NULL AND updated_at < now() - interval '90 days')
                      RETURNING user_id)
         DELETE FROM raider_evidence WHERE user_id IN (SELECT user_id FROM old)",
    )
    .execute(&app.pool)
    .await?;
    // One owner's raid or spam detection makes no entry, so its evidence goes by its own age.
    sqlx::query(
        "DELETE FROM raider_evidence e WHERE created_at < now() - interval '90 days'
           AND NOT EXISTS (SELECT 1 FROM raiders r WHERE r.user_id = e.user_id)",
    )
    .execute(&app.pool)
    .await?;
    Ok(())
}

/// At most one DM a day while the queue holds entries backed by 2 or more owners.
async fn digest(app: &App) -> Result<()> {
    static LAST: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());
    let Some(owner) = app.bot_owner else {
        return Ok(());
    };
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM raiders r WHERE status = 'pending'
           AND (SELECT count(DISTINCT source_owner) FROM raider_evidence e WHERE e.user_id = r.user_id) >= 2",
    )
    .fetch_one(&app.pool)
    .await?;
    // shortcut: the day is kept in memory, so a restart can send a second digest that day.
    if n == 0 || !reply::alert_due(&mut LAST.lock().unwrap(), 0, now_secs(), 86_400) {
        return Ok(());
    }
    let text = format!("**Raider List:** {n} entries in the review queue have evidence from 2 or more server owners.");
    let rows: Vec<Component> = queue_url(None)
        .map(|u| crate::verify::row(crate::welcome::link("Open the queue", u)))
        .into_iter()
        .collect();
    reply::dm_buttons(app, owner, &text, &rows).await;
    Ok(())
}

/// `raider check @user` (any permit) and `raider report @user <category> <reason>` (owner, extra owner).
pub async fn command(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: Id<UserMarker>,
    level: &Level,
    words: &[&str],
) -> Result<String> {
    let user = words
        .get(1)
        .and_then(|w| w.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok())
        .and_then(Id::new_checked);
    let usage = "Use `raider check @user` or `raider report @user <nuke|raid|scam|spam> <reason>`.";
    match (words.first().copied(), user) {
        (Some("check"), Some(u)) => check_text(app, u).await,
        (Some("report"), Some(u)) => {
            if !matches!(level, Level::Owner | Level::ExtraOwner) {
                return Ok(
                    "Only the owner or an Extra Owner can report to the Raider List.".into(),
                );
            }
            let (Some(cat), reason) = (
                words.get(2).and_then(|c| parse_cat(c)),
                words[3.min(words.len())..].join(" "),
            ) else {
                return Ok(usage.into());
            };
            if reason.trim().is_empty() {
                return Ok(usage.into());
            }
            report(app, gid, invoker, u, cat, &reason).await
        }
        _ => Ok(usage.into()),
    }
}

/// Listed or not, the category and the public note; never the evidence or where it came from.
async fn check_text(app: &App, user: Id<UserMarker>) -> Result<String> {
    Ok(match entry(app, user).await? {
        Some(e) if e.note.is_empty() => {
            format!("<@{user}> is on the Raider List: **{}**.", e.category)
        }
        Some(e) => format!(
            "<@{user}> is on the Raider List: **{}**. Note: {}",
            e.category,
            md(&e.note)
        ),
        None => format!("<@{user}> is not on the Raider List."),
    })
}

/// The prefix command's side of a report: what only Discord knows, then the shared report.
async fn report(
    app: &App,
    gid: Id<GuildMarker>,
    invoker: Id<UserMarker>,
    user: Id<UserMarker>,
    cat: RaiderCategory,
    reason: &str,
) -> Result<String> {
    let case: Option<(i32, String)> = sqlx::query_as(
        "SELECT case_no, kind FROM cases WHERE guild_id = $1 AND user_id = $2 AND kind = ANY($3)
         ORDER BY case_no DESC LIMIT 1",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(raiders::REPORTABLE.as_slice())
    .fetch_optional(&app.pool)
    .await?;
    let guild = app.http.guild(gid).with_counts(true).await?.model().await?;
    let me = app.http.guild_member(gid, app.me).await?.model().await?;
    let days = me
        .joined_at
        .map_or(0, |t| (now_secs() - t.as_secs()) / 86_400);
    let (roles, bot) = member_roles(app, gid, user).await?;
    let r = raiders::Report {
        guild: gid.get() as i64,
        owner: guild.owner_id.get() as i64,
        user: user.get() as i64,
        reporter: invoker.get() as i64,
        case: case.as_ref().map(|(n, k)| (*n, k.as_str())),
        category: cat_name(cat),
        reason: Some(reason),
        members: guild.approximate_member_count.unwrap_or(0),
        days_with_bot: days,
        roles: &roles,
        bot,
        protected: [
            app.me.get() as i64,
            app.bot_owner.map_or(0, |b| b.get() as i64),
        ],
    };
    Ok(match raiders::report(&app.pool, &r).await? {
        Ok(()) => format!("Reported <@{user}> to the Raider List as **{}**. The bot owner reviews every report; a report never lists anyone by itself.", cat_name(cat)),
        Err(no) => no,
    })
}
