//! What staff and the owner start by hand: the Ban here and Send the rest buttons, the dashboard's
//! import, and the text under the `bansync` panel.
use super::*;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::{AllowedMentions, MessageFlags},
    http::interaction::InteractionResponseType,
};

/// The other servers of the group, confirmed live; a failed lookup leaves a server out.
async fn siblings(app: &App, gid: Id<GuildMarker>) -> Result<Vec<Id<GuildMarker>>> {
    let mut out = vec![];
    for g in group(app, gid).await? {
        match same_owner(app, gid, g).await {
            Ok(true) => out.push(g),
            Ok(false) => {}
            Err(e) => tracing::warn!(?e, %gid, %g, "ban sync: group check failed"),
        }
    }
    Ok(out)
}

/// Below the `bansync` panel, for Trusted Admins and up as on the dashboard: the group and the
/// last 10 copies between this server and the servers still in it.
pub async fn panel_text(app: &App, gid: Id<GuildMarker>, admin: bool) -> Result<String> {
    if !admin {
        return Ok(String::new());
    }
    let mut names = BTreeMap::new();
    for g in siblings(app, gid).await? {
        names.insert(g.get(), md(&verify::guild_name(app, g).await));
    }
    let mut out = match names
        .values()
        .map(|n| format!("**{n}**"))
        .collect::<Vec<_>>()
    {
        v if v.is_empty() => "**Group:** no other server of this owner has TommyBot".to_string(),
        v => format!("**Group:** {}", v.join(", ")),
    };
    let ids: Vec<i64> = names.keys().map(|g| *g as i64).collect();
    let rows: Vec<(i64, i64, i64, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT guild_id, source_guild, user_id, state, skip_reason, extract(epoch FROM created_at)::bigint
         FROM ban_sync_links
         WHERE (guild_id = $1 AND source_guild = ANY($2)) OR (source_guild = $1 AND guild_id = ANY($2))
         ORDER BY created_at DESC LIMIT 10",
    )
    .bind(gid.get() as i64)
    .bind(&ids)
    .fetch_all(&app.pool)
    .await?;
    out.push_str("\n**Last copies:**");
    if rows.is_empty() {
        out.push_str(" none yet");
    }
    for (to, src, user, state, why, at) in rows {
        let way = if to as u64 == gid.get() {
            format!("from **{}**", names[&(src as u64)])
        } else {
            format!("to **{}**", names[&(to as u64)])
        };
        let why = why.map_or(String::new(), |w| {
            format!(" ({})", md(&embed::clip(&w, 60)))
        });
        out.push_str(&format!("\n`{state}`{why} <@{user}> {way} <t:{at}:R>"));
    }
    Ok(out)
}

/// `bansync:{src}:{user}` (Ban here, in the receiving server) and `bansync:rest:{src}` (Send the
/// rest, in the owner's DM). The id only points at a row; the clicker's rights and the DB decide.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix(BUTTON) else {
        return Ok(false);
    };
    let client = app.http.interaction(Id::new(app.me.get()));
    let defer = verify::ephemeral(
        InteractionResponseType::DeferredChannelMessageWithSource,
        None,
    );
    client.create_response(i.id, &i.token, &defer).await?;
    fn id<T>(s: &str) -> Option<Id<T>> {
        s.parse().ok().and_then(Id::new_checked)
    }
    let done = match (rest.strip_prefix(REST), i.author_id()) {
        (Some(src), Some(clicker)) => match id(src) {
            Some(src) => send_rest_click(app, clicker, src).await,
            None => Ok("This button does not work here.".into()),
        },
        (None, Some(clicker)) => match (i.guild_id, rest.split_once(':')) {
            (Some(gid), Some((s, u))) => match (id(s), id(u)) {
                (Some(src), Some(user)) => ban_click(app, gid, clicker, (src, user)).await,
                _ => Ok("This button does not work here.".into()),
            },
            _ => Ok("This button does not work here.".into()),
        },
        _ => Ok("This button does not work here.".into()),
    };
    let text = done.unwrap_or_else(|e| {
        tracing::warn!(?e, "ban sync button failed");
        format!("Something went wrong: {}", http_err::why(&e))
    });
    client
        .create_followup(&i.token)
        .content(&text)
        .flags(MessageFlags::EPHEMERAL)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(true)
}

/// Ban here: the clicker's ban permit here, the group and the source ban still there. The row is
/// pending while the ban runs, and goes back to alerted when it does not happen.
pub(super) async fn ban_click(
    app: &App,
    gid: Id<GuildMarker>,
    clicker: Id<UserMarker>,
    (src, user): (Id<GuildMarker>, Id<UserMarker>),
) -> Result<String> {
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, clicker).await?;
    if !level.has("ban") {
        return Ok("You need the ban permit in this server to do this.".into());
    }
    let key = (gid, user, src);
    if !grouped(app, src, gid).await? {
        return Ok("Ban Sync no longer links these servers, so nothing was done.".into());
    }
    if http_err::ban(&app.http, src, user).await?.is_none() {
        mark(app, key, &["alerted"], "lifted", None).await?;
        return Ok("The ban in that server was lifted, so nothing was done.".into());
    }
    let Some(row) = load(app, key).await?.filter(|r| r.state == "alerted") else {
        return Ok("This alert was already handled.".into());
    };
    let duration = match remaining(row.expires) {
        Ok(d) => d,
        Err(why) => {
            mark(app, key, &["alerted"], "skipped", Some(why)).await?;
            return Ok("That temp ban has already ended, so nothing was done.".into());
        }
    };
    // Pending while the ban runs, so its audit event is not shared on as a new ban.
    if !mark(app, key, &["alerted"], "pending", None).await? {
        return Ok("This alert was already handled.".into());
    }
    let why = copy_reason(&verify::guild_name(app, src).await, src.get(), &row.reason);
    let done = commands::apply(
        app,
        gid,
        owner,
        (clicker, &level),
        ModKind::Ban,
        user,
        Some(&why),
        duration,
        0,
    )
    .await;
    let made = match &done {
        Ok(a) if a.case.is_some() => true,
        _ => banned_by_us(app, key).await.unwrap_or(false),
    };
    if made {
        finish_ban(app, key, done.as_ref().ok().and_then(|a| a.case), &why).await?;
    } else {
        mark(app, key, &["pending"], "alerted", None).await?;
    }
    Ok(done?.text)
}

/// Send the rest: only the live owner of the source server, with a fresh 2FA on the dashboard.
pub(super) async fn send_rest_click(
    app: &App,
    clicker: Id<UserMarker>,
    src: Id<GuildMarker>,
) -> Result<String> {
    if live_owner(app, src).await? != Some(clicker) {
        return Ok("Only the owner of that server can do this.".into());
    }
    if !crate::settings::fresh_2fa(app, clicker.get()).await? {
        return Ok(crate::settings::needs_2fa(src.get()));
    }
    let cfg = app.cfg(src.get()).await?;
    if !(cfg.ban_sync.enabled && cfg.ban_sync.share) {
        return Ok("Ban Sync no longer sends bans from that server.".into());
    }
    let job = json!({"source": src.get()});
    jobs::enqueue(
        &app.pool,
        src.get() as i64,
        "ban_sync_send_rest",
        job,
        now_secs(),
    )
    .await?;
    Ok(format!(
        "Sending the next {BAN_SYNC_HOURLY} held back bans."
    ))
}

/// (receiving server, actor, moderator, ban time) of a row sent by Send the rest.
type Held = (i64, Option<i64>, Option<i64>, Option<i64>);

/// Anti-nuke acted in the server in the last day.
async fn recent_incident(app: &App, gid: Id<GuildMarker>) -> Result<bool> {
    crate::antinuke::contained_since(app, gid, None, now_secs() - 86_400).await
}

/// A share of this user is still in its hold.
async fn share_waiting(app: &App, src: Id<GuildMarker>, user: Id<UserMarker>) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM scheduled_jobs WHERE guild_id = $1 AND kind = 'ban_sync_share'
                        AND payload->>'user' = $2)",
    )
    .bind(src.get() as i64)
    .bind(user.get().to_string())
    .fetch_one(&app.pool)
    .await?)
}

/// `ban_sync_import` {source, target}: the source's existing bans, through the copy path, the
/// share switches and the cap.
pub async fn import_job(app: &App, p: &Value) -> Result<()> {
    let (src, to) = (Id::new(need(p, "source")?), Id::new(need(p, "target")?));
    if !grouped(app, src, to).await? {
        return Ok(());
    }
    // Right after an attack the ban list may hold a hacked account's bans.
    if recent_incident(app, src).await? {
        tracing::warn!(%src, %to, "ban sync import refused: anti-nuke acted there in the last 24 hours");
        let name = md(&verify::guild_name(app, src).await);
        let text = format!("{WARNING} **Ban Sync did not import the bans of {name}.** Anti-nuke acted there in the last 24 hours, so its ban list may hold bans a hacked account made. Check them, then try again tomorrow.");
        dm_owner(app, src, &text, &[]).await;
        return Ok(());
    }
    let cfg = app.cfg(src.get()).await?;
    let mut sent = shared_last_hour(app, src).await?;
    let mut after = None;
    loop {
        let req = app.http.bans(src).limit(PAGE);
        let req = match after {
            Some(a) => req.after(a),
            None => req,
        };
        let page = req.await?.models().await?;
        for b in &page {
            let user = b.user.id;
            // A copy in the source never chains on; a ban in its hold is shared by its own job.
            if is_copy(app, src, user).await? || share_waiting(app, src, user).await? {
                continue;
            }
            let reason = b.reason.as_deref().unwrap_or_default();
            let case = last_case(app, src, user, &["ban", "tempban"], None).await?;
            let origin = origin(app, (src, user), None, case, None, reason).await?;
            if !sends(&cfg.ban_sync, origin) {
                continue;
            }
            let copy = Copy {
                origin,
                case: case.map(|c| c.0),
                reason: embed::clip(reason, 400),
                expires: temp_end(app, src, user).await?,
                actor: None,
                moderator: case.map(|c| c.1).filter(|m| *m as u64 != app.me.get()),
                banned_at: None,
            };
            if offer(app, (to, user, src), &copy, sent >= BAN_SYNC_HOURLY).await? {
                sent += 1;
            }
        }
        match page.last() {
            Some(b) if page.len() == usize::from(PAGE) => after = Some(b.user.id),
            _ => break,
        }
    }
    if sent >= BAN_SYNC_HOURLY {
        paused(app, src).await;
    }
    Ok(())
}

/// `ban_sync_send_rest` {source}: the next users held back by the cap, sent by the owner's choice.
/// A row whose source moderator was contained since is cancelled instead.
pub async fn send_rest_job(app: &App, p: &Value) -> Result<()> {
    let src = Id::new(need(p, "source")?);
    let users: Vec<i64> = sqlx::query_scalar(
        "SELECT user_id FROM ban_sync_links
         WHERE source_guild = $1 AND state = 'skipped' AND skip_reason = 'rate cap'
         GROUP BY user_id ORDER BY min(created_at) LIMIT $2",
    )
    .bind(src.get() as i64)
    .bind(BAN_SYNC_HOURLY)
    .fetch_all(&app.pool)
    .await?;
    for u in users {
        let user = Id::new(u as u64);
        let rows: Vec<Held> = sqlx::query_as(
            "UPDATE ban_sync_links SET state = 'pending', skip_reason = NULL
             WHERE source_guild = $1 AND user_id = $2 AND state = 'skipped' AND skip_reason = 'rate cap'
             RETURNING guild_id, actor_id, mod_id, extract(epoch FROM banned_at)::bigint",
        )
        .bind(src.get() as i64)
        .bind(u)
        .fetch_all(&app.pool)
        .await?;
        for (to, actor, moderator, at) in rows {
            let key = (Id::new(to as u64), user, src);
            let who = behind(app.me, actor.map(|a| a as u64), moderator);
            if stopped(app, src, who, at).await? {
                let why = Some("source moderator contained");
                mark(app, key, &["pending"], "cancelled", why).await?;
            } else {
                enqueue_copy(app, key.0, user, src).await?;
            }
        }
    }
    Ok(())
}

/// Over the cap the rest waits for the owner: a burst like that is how a hacked account looks.
pub(super) async fn paused(app: &App, src: Id<GuildMarker>) {
    static LAST: Mutex<BTreeMap<u64, i64>> = Mutex::new(BTreeMap::new());
    if !reply::alert_due(&mut LAST.lock().unwrap(), src.get(), now_secs(), 3600) {
        return;
    }
    let name = md(&verify::guild_name(app, src).await);
    let text = format!(
        "{WARNING} **Ban Sync paused for {name}.** More than {BAN_SYNC_HOURLY} bans were made there in the last hour, so the rest were not sent to your other servers. If you did not expect that many bans, check that server's staff first. If the bans are real, press **Send the rest** to send the next {BAN_SYNC_HOURLY} (it needs a fresh 2FA on the dashboard)."
    );
    let row = buttons(vec![button(
        "Send the rest",
        format!("{BUTTON}{REST}{src}"),
        ButtonStyle::Primary,
    )]);
    dm_owner(app, src, &text, &[row]).await;
}

pub(super) async fn dm_owner(app: &App, gid: Id<GuildMarker>, text: &str, rows: &[Component]) {
    match live_owner(app, gid).await {
        Ok(Some(owner)) => drop(reply::dm_buttons(app, owner, text, rows).await),
        Ok(None) => {}
        Err(e) => tracing::warn!(?e, %gid, "ban sync owner DM: guild lookup failed"),
    }
}
