use super::*;

/// `t!word args` or `@bot word args` in a DM.
fn dm_command(content: &str, me: Id<UserMarker>) -> Option<(String, &str)> {
    let rest = [
        DM_PREFIX.to_string(),
        format!("<@{me}> "),
        format!("<@!{me}> "),
    ]
    .iter()
    .find_map(|p| content.strip_prefix(p.as_str()))?
    .trim_start();
    let (word, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((word.to_lowercase(), args.trim()))
}

/// What a DM asks to send: the text after `new` or `send ID`, else the whole message.
fn body(content: &str, me: Id<UserMarker>) -> String {
    match dm_command(content, me) {
        Some((w, a)) if matches!(w.as_str(), "new" | "create" | "switch" | "change") => a.into(),
        Some((w, a)) if w == "send" => a
            .split_once(char::is_whitespace)
            .map_or("", |x| x.1)
            .trim()
            .into(),
        _ => content.into(),
    }
}

pub async fn on_dm(app: &App, m: &Message) -> Result<()> {
    let dm = m.channel_id;
    if let Some((word, args)) = dm_command(&m.content, app.me) {
        return match word.as_str() {
            "new" | "create" | "switch" | "change"
                if args.is_empty() && m.attachments.is_empty() =>
            {
                say(
                    app,
                    dm,
                    error(&format!("Usage: `{DM_PREFIX}new <message>`")),
                )
                .await
            }
            "new" | "create" | "switch" | "change" => pick(app, m).await,
            "send" => match args
                .split_whitespace()
                .next()
                .and_then(|g| g.parse().ok())
                .and_then(Id::new_checked)
            {
                Some(g) if !body(&m.content, app.me).is_empty() || !m.attachments.is_empty() => {
                    send_mail(app, m, g).await
                }
                _ => {
                    say(
                        app,
                        dm,
                        error(&format!("Usage: `{DM_PREFIX}send <server ID> <message>`")),
                    )
                    .await
                }
            },
            "confirmation" => confirmation(app, m).await,
            _ => Ok(()),
        };
    }
    if m.content.trim().is_empty() && m.attachments.is_empty() {
        return Ok(());
    }
    match user_state(app, m.author.id).await? {
        (true, Some(g)) => match server(app, g).await? {
            Some(s) => ask(app, m, &s).await,
            None => pick(app, m).await,
        },
        (false, Some(g)) => send_mail(app, m, g).await,
        (_, None) => pick(app, m).await,
    }
}

async fn confirmation(app: &App, m: &Message) -> Result<()> {
    let (on, _) = user_state(app, m.author.id).await?;
    sqlx::query(
        "INSERT INTO ticket_users (user_id, confirmation) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET confirmation = $2",
    )
    .bind(m.author.id.get() as i64)
    .bind(!on)
    .execute(&app.pool)
    .await?;
    let text = if on {
        format!("Confirmation messages are disabled. To send messages to another server, either use `{DM_PREFIX}new <message>` or `{DM_PREFIX}send <server ID> <message>`.")
    } else {
        "Confirmation messages are enabled.".into()
    };
    say(app, m.channel_id, info(&text)).await
}

async fn ask(app: &App, m: &Message, s: &Server) -> Result<()> {
    let (e, row) = confirm(s, m.id);
    app.http
        .create_message(m.channel_id)
        .embeds(&[e])
        .components(&[row])
        .await?;
    Ok(())
}

/// Servers the member shares with the bot that take tickets, and whether a ticket is open there.
async fn mutual(app: &App, uid: Id<UserMarker>) -> Result<Vec<(Server, bool)>> {
    let guilds = ticket_guilds(&app.pool).await?;
    let open: Vec<i64> =
        sqlx::query_scalar("SELECT guild_id FROM tickets WHERE user_id = $1 AND closed_at IS NULL")
            .bind(uid.get() as i64)
            .fetch_all(&app.pool)
            .await?;
    let mut out = vec![];
    for g in guilds.into_iter().filter_map(|g| Id::new_checked(g as u64)) {
        // A server the bot left answers with an error; it is simply not shared.
        if !matches!(http_err::member(&app.http, g, uid).await, Ok(Some(_))) {
            continue;
        }
        if let Some(s) = server(app, g).await? {
            out.push((s, open.contains(&(g.get() as i64))));
        }
        if out.len() == 25 {
            break;
        }
    }
    Ok(out)
}

/// The server menu, or the error to show instead.
async fn pick_view(
    app: &App,
    uid: Id<UserMarker>,
    msg: Id<MessageMarker>,
) -> Result<(Embed, Vec<Component>)> {
    let list = mutual(app, uid).await?;
    if list.is_empty() {
        return Ok((
            error("Oops, something strange happened. No server found."),
            vec![],
        ));
    }
    let options = list
        .iter()
        .map(|(s, open)| SelectMenuOption {
            default: false,
            description: Some(
                if *open {
                    "Existing ticket."
                } else {
                    "Create a new ticket."
                }
                .into(),
            ),
            emoji: None,
            label: clip(&s.name, 100),
            value: s.id.to_string(),
        })
        .collect();
    let menu = Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id: format!("tk:pick:{msg}"),
        default_values: None,
        disabled: false,
        kind: SelectMenuType::Text,
        max_values: Some(1),
        min_values: Some(1),
        options: Some(options),
        placeholder: Some("Select a server".into()),
        required: None,
    });
    let e = emb(
        &format!("{TICKET} Select Server"),
        "Please select the server you want to send this message to.",
        BLUE,
    );
    Ok((e.build(), vec![action_row(vec![menu])]))
}

async fn pick(app: &App, m: &Message) -> Result<()> {
    // The menu asks Discord about every ticket server, so one menu per member every few seconds.
    if !app.cooled_down(0, m.author.id.get()) {
        return Ok(());
    }
    let (e, rows) = pick_view(app, m.author.id, m.id).await?;
    app.http
        .create_message(m.channel_id)
        .embeds(&[e])
        .components(&rows)
        .await?;
    Ok(())
}

/// Sends a member's DM to a server, opening a ticket there first when none is open.
async fn send_mail(app: &App, m: &Message, gid: Id<GuildMarker>) -> Result<()> {
    let dm = m.channel_id;
    let Some(s) = server(app, gid).await? else {
        return say(app, dm, error("The server was not found.")).await;
    };
    let Some(member) = http_err::member(&app.http, gid, m.author.id).await? else {
        return say(
            app,
            dm,
            error("You are not in that server, and the message is not sent."),
        )
        .await;
    };
    let category = match ensure_category(app, gid).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            let text = "That server does not take tickets right now.";
            return say(app, dm, error(text)).await;
        }
        Err(e) => {
            tracing::warn!(?e, %gid, "making the ticket category failed");
            let text = "A ticket category is not found and TommyBot could not make one. Please contact an admin on the server.";
            return say(app, dm, error(text)).await;
        }
    };
    let cfg = app.cfg(gid.get()).await?;
    let t = &cfg.tickets;
    if t.blocked.contains(&m.author.id) {
        let text = "That server has blacklisted you from sending a message there.";
        return say(app, dm, error(text)).await;
    }
    let text = body(&m.content, app.me);
    if filtered(app, &cfg, &text) {
        let text = "Your message was not sent: it holds words or links that server does not allow.";
        return say(app, dm, error(text)).await;
    }
    let ch = match open_ticket(app, gid, m.author.id).await? {
        Some(ch) => ch,
        // D27: no new tickets while a raid or lock is on, and a few an hour per member.
        None if crate::lockdown::raid_or_lock(app, gid).await? => {
            let text = "That server is in lockdown and does not take new tickets right now. Please try again later.";
            return say(app, dm, error(text)).await;
        }
        None if opened_last_hour(app, gid, Some(m.author.id)).await? >= TICKETS_PER_HOUR => {
            let text = "You opened too many tickets in that server in the last hour. Please try again later.";
            return say(app, dm, error(text)).await;
        }
        None if opened_last_hour(app, gid, None).await? >= SERVER_TICKETS_PER_HOUR => {
            let text = "That server gets too many new tickets right now. Please try again later.";
            return say(app, dm, error(text)).await;
        }
        // Thomas (D27): a quarantined member reaches staff only when nobody told them why.
        None if quarantined_with_reason(app, gid, m.author.id).await? => {
            let text = "You are quarantined in that server and were given the reason, so you cannot open a ticket there.";
            return say(app, dm, error(text)).await;
        }
        None if t.paused => {
            let reason = if t.pause_reason.is_empty() {
                "No reason was provided."
            } else {
                &t.pause_reason
            };
            let e = s.footer(stamp(emb(
                &format!("{TICKET_BLOCK} Ticket Creation Disabled"),
                reason,
                RED,
            )));
            return say(app, dm, e.build()).await;
        }
        None => match create(app, &s, m, &member.roles, t, category).await? {
            Some(ch) => ch,
            None => return Ok(()),
        },
    };
    let (files, linked) = download(app, &m.attachments).await;
    let (sent, got) = from_member(&s, &m.author, &text);
    let dm_msg = app
        .http
        .create_message(dm)
        .embeds(&[sent.build()])
        .attachments(&files)
        .await?
        .model()
        .await?;
    // The member's own message stays in the DM, so its links keep working.
    let got = attachment_fields(got, dm_msg.attachments.iter().chain(linked));
    if let Err(e) = app
        .http
        .create_message(ch)
        .embeds(&[got.build()])
        .attachments(&files)
        .await
    {
        tracing::warn!(?e, %ch, "relaying a ticket message failed");
        best_effort(
            app.http.delete_message(dm, dm_msg.id).await,
            "undo ticket dm",
        );
        let text = "The bot is missing permissions. Please contact an admin on the server.";
        return say(app, dm, error(text)).await;
    }
    remember(app, m.author.id, gid).await
}

/// New tickets one member, and the whole server, may open per hour (D27).
const TICKETS_PER_HOUR: i64 = 3;
const SERVER_TICKETS_PER_HOUR: i64 = 20;

/// Tickets opened in the last hour in the server, by `user` or by anyone.
async fn opened_last_hour(
    app: &App,
    gid: Id<GuildMarker>,
    user: Option<Id<UserMarker>>,
) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM tickets WHERE guild_id = $1 AND ($2::bigint IS NULL OR user_id = $2)
         AND opened_at > now() - interval '1 hour'",
    )
    .bind(gid.get() as i64)
    .bind(user.map(|u| u.get() as i64))
    .fetch_one(&app.pool)
    .await?)
}

/// Quarantined, and the newest quarantine case since this quarantine began is staff's with a
/// reason (an older, lifted case does not count). Automatic quarantines
/// (the bot's own cases) and staff quarantines without a reason told the member nothing.
async fn quarantined_with_reason(
    app: &App,
    gid: Id<GuildMarker>,
    user: Id<UserMarker>,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce((SELECT c.mod_id <> $3 AND coalesce(c.reason, '') <> ''
                          FROM quarantines q JOIN cases c
                            ON c.guild_id = q.guild_id AND c.user_id = q.user_id
                           AND c.kind = 'quarantine' AND c.created_at >= q.created_at
                          WHERE q.guild_id = $1 AND q.user_id = $2
                          ORDER BY c.case_no DESC LIMIT 1), false)",
    )
    .bind(gid.get() as i64)
    .bind(user.get() as i64)
    .bind(app.me.get() as i64)
    .fetch_one(&app.pool)
    .await?)
}

/// A ticket DM goes through the server's word, scam and invite filters (D27). The local word
/// list is used even when words go to Discord's AutoMod, which never sees a DM.
/// shortcut: no shortener lookup and no attachment scan; add them if scams move there.
fn filtered(app: &App, cfg: &crate::app::GuildCfg, text: &str) -> bool {
    use tommybot_automod::filters;
    let am = &cfg.automod;
    if !am.enabled {
        return false;
    }
    let f = &am.filters;
    let hosts = filters::hosts(text);
    let words = f.words.filter.enabled && {
        let forms = filters::word_forms(text);
        filters::word_hits(&forms, &crate::automod::blacklist_words(f)) + f.words.regex.hits(&forms)
            > 0
    };
    words
        || f.scam_links.enabled && filters::is_nitro_steam_scam(text)
        || f.bad_sites.enabled && app.sites.phishing(&hosts)
        || f.invites.enabled && crate::automod::is_invite(text, &hosts)
}

/// Opens the ticket channel, logs it and greets the member. `None` when the member was told why not.
async fn create(
    app: &App,
    s: &Server,
    m: &Message,
    roles: &[Id<RoleMarker>],
    t: &TicketsCfg,
    category: Id<ChannelMarker>,
) -> Result<Option<Id<ChannelMarker>>> {
    let (gid, user, dm) = (s.id, &m.author, m.channel_id);
    let g = gid.get() as i64;
    let number = claim_number(&app.pool, gid, user.id).await?;
    let Some(number) = number else {
        let text = "Your ticket is being opened, please send your message again in a moment.";
        say(app, dm, error(text)).await?;
        return Ok(None);
    };
    let ows = overwrites(gid, app.me.get(), &staff_roles(app, gid, t).await?);
    let make = |name: String| {
        let ows = &ows;
        async move {
            let r = app
                .http
                .create_guild_channel(gid, &name)
                .kind(ChannelType::GuildText)
                .parent_id(category)
                .permission_overwrites(ows)
                .await?;
            anyhow::Ok(r.model().await?.id)
        }
    };
    let made = match make(channel_name(&user.name, user.id, rand::random())).await {
        // Discord refuses names with words it does not allow (50035); the id always passes.
        Err(e) if http_err::code(&e) == Some(50035) => make(user.id.to_string()).await,
        r => r,
    };
    let ch = match made {
        Ok(ch) => ch,
        Err(e) => {
            sqlx::query("DELETE FROM tickets WHERE guild_id = $1 AND number = $2")
                .bind(g)
                .bind(number)
                .execute(&app.pool)
                .await?;
            let text = match http_err::code(&e) {
                Some(30013) => "The server has reached the maximum number of active tickets. Please try again later.".into(),
                _ => format!("An error occurred. Please contact an admin on the server with the following information: {}.", http_err::why(&e)),
            };
            say(app, dm, error(&text)).await?;
            return Ok(None);
        }
    };
    sqlx::query("UPDATE tickets SET channel_id = $3 WHERE guild_id = $1 AND number = $2")
        .bind(g)
        .bind(number)
        .bind(ch.get() as i64)
        .execute(&app.pool)
        .await?;
    if let Some(log) = t.log_channel {
        let e = Log::new("New Ticket", GREEN)
            .user("User", user.id.get())
            .footer(&format!("{} | {}", user.name, user.id));
        if let Err(e) = crate::events::send_private(app, log, &e.time().build(), None).await {
            tracing::warn!(?e, "ticket log failed");
        }
    }
    let pfx = &app.cfg(gid.get()).await?.misc.prefix;
    let quarantined = crate::antinuke::quarantined(app, gid, user.id).await?;
    let intro = intro(pfx, t.command_only, user, roles, quarantined);
    let (pings, mentions) = pings(gid, t);
    let sent = app
        .http
        .create_message(ch)
        .content(&pings)
        .embeds(&[intro])
        .allowed_mentions(Some(&mentions))
        .await;
    match sent {
        // Pinned, so staff always find who the member is and how to reply.
        Ok(r) => {
            let id = r.model().await?.id;
            best_effort(app.http.create_pin(ch, id).await, "pin ticket intro");
        }
        Err(e) => {
            tracing::warn!(?e, %ch, "ticket intro failed");
            let text = "The bot is missing permissions. Please contact an admin on the server.";
            say(app, dm, error(text)).await?;
            return Ok(None);
        }
    }
    if !t.greeting.is_empty() {
        say(app, dm, notice(s, false, &t.greeting, user)).await?;
    }
    Ok(Some(ch))
}

/// The ping line for a new ticket and the mentions it may use.
fn pings(gid: Id<GuildMarker>, t: &TicketsCfg) -> (String, AllowedMentions) {
    let mut words = vec![];
    let mut am = AllowedMentions::default();
    for r in &t.ping_roles {
        if r.get() == gid.get() {
            words.push("@everyone".to_string());
            am.parse = vec![MentionType::Everyone];
        } else {
            words.push(format!("<@&{r}>"));
            am.roles.push(*r);
        }
    }
    if t.ping_here {
        words.push("@here".into());
        am.parse = vec![MentionType::Everyone];
    }
    (words.join(" "), am)
}

/// Buttons and the server menu in DMs.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix("tk:") else {
        return Ok(false);
    };
    let (Some(user), Some(prompt)) = (i.author_id(), i.message.as_ref()) else {
        return Ok(true);
    };
    let client = app.http.interaction(app.me.cast());
    let update = |e: Embed, rows: Vec<Component>| InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            embeds: Some(vec![e]),
            components: Some(rows),
            ..Default::default()
        }),
    };
    let made_ms = ((prompt.id.get() >> 22) + 1_420_070_400_000) as i64;
    if now_secs() * 1000 - made_ms > MENU_SECS * 1000 {
        let r = update(
            error("This request has expired. Send your message again."),
            vec![],
        );
        client.create_response(i.id, &i.token, &r).await?;
        return Ok(true);
    }
    let parts: Vec<&str> = rest.split(':').collect();
    // The member's own DM, fetched again so nothing is kept between the prompt and the answer.
    let original = |id: &str| {
        let id = id.parse().ok().and_then(Id::new_checked);
        async move {
            let Some(id) = id else {
                return anyhow::Ok(None);
            };
            match app.http.message(prompt.channel_id, id).await {
                Ok(r) => Ok(Some(r.model().await?)),
                Err(e) if http_err::gone(&e) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
    };
    let chosen = match parts.as_slice() {
        ["no"] => {
            let r = update(error("Request cancelled successfully."), vec![]);
            client.create_response(i.id, &i.token, &r).await?;
            return Ok(true);
        }
        ["other", msg] => {
            let defer = InteractionResponse {
                kind: InteractionResponseType::DeferredUpdateMessage,
                data: None,
            };
            client.create_response(i.id, &i.token, &defer).await?;
            let Some(msg) = msg.parse().ok().and_then(Id::new_checked) else {
                return Ok(true);
            };
            let (e, rows) = pick_view(app, user, msg).await?;
            client
                .update_response(&i.token)
                .embeds(Some(&[e]))
                .components(Some(&rows))
                .await?;
            return Ok(true);
        }
        ["ok", g, msg] => g.parse().ok().and_then(Id::new_checked).map(|g| (g, *msg)),
        ["pick", msg] => c
            .values
            .first()
            .and_then(|v| v.parse().ok())
            .and_then(Id::new_checked)
            .map(|g| (g, *msg)),
        _ => None,
    };
    let Some((gid, msg)) = chosen else {
        return Ok(true);
    };
    let defer = InteractionResponse {
        kind: InteractionResponseType::DeferredUpdateMessage,
        data: None,
    };
    client.create_response(i.id, &i.token, &defer).await?;
    best_effort(
        app.http.delete_message(prompt.channel_id, prompt.id).await,
        "ticket prompt",
    );
    match original(msg).await? {
        Some(m) => send_mail(app, &m, gid).await,
        None => {
            say(
                app,
                prompt.channel_id,
                error("Your message was deleted, so nothing was sent."),
            )
            .await
        }
    }?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_commands_and_message_bodies() {
        let me = Id::new(7);
        assert_eq!(
            dm_command("t!New hi there", me),
            Some(("new".into(), "hi there"))
        );
        assert_eq!(
            dm_command("<@7> confirmation", me),
            Some(("confirmation".into(), ""))
        );
        assert_eq!(dm_command("hello", me), None);
        assert_eq!(body("t!send 123 hi there", me), "hi there");
        assert_eq!(body("t!switch hi", me), "hi");
        assert_eq!(body("plain text", me), "plain text");
    }

    #[test]
    fn pings_cover_everyone_here_and_roles() {
        let t = TicketsCfg {
            ping_roles: vec![Id::new(9), Id::new(4)],
            ping_here: true,
            ..Default::default()
        };
        let (line, am) = pings(Id::new(9), &t);
        assert_eq!(line, "@everyone <@&4> @here");
        assert_eq!(am.roles, vec![Id::new(4)]);
        assert_eq!(am.parse, vec![MentionType::Everyone]);
    }

    /// D27: a ticket DM is filtered like a server message, and no new ticket opens during a raid
    /// or lockdown or past the hourly limit. Every refusal comes before a channel is made.
    #[tokio::test]
    async fn ticket_dms_are_filtered_and_new_tickets_limited() {
        let Some(t) = crate::test_support::T::new(173).await else {
            return;
        };
        const CAT: u64 = 600;
        let cfg = json!({
            "tickets": {"enabled": true, "category": CAT.to_string()},
            "automod": {"enabled": true, "filters": {
                "words": {"filter": {"enabled": true}, "strict": ["badword"]},
                "invites": {"enabled": true}}},
        });
        t.server_config(0, t.owner(), cfg).await;
        let gid = t.g(0);
        let info = crate::hierarchy::GuildInfo {
            owner: Id::new(t.owner()),
            roles: Default::default(),
            role_perms: Default::default(),
            managed: Default::default(),
            parents: Default::default(),
            channels: [Id::new(CAT)].into(),
            nsfw: Default::default(),
            bot_top: 10,
            fetched: std::time::Instant::now(),
        };
        t.app
            .guilds
            .lock()
            .unwrap()
            .insert(gid, std::sync::Arc::new(info));
        let u = t.base * 10 + 1;
        t.set(|m| {
            m.members.insert((gid.get(), u), vec![]);
        });
        let dm = |text: &str| -> Message {
            serde_json::from_value(json!({"id": "5", "channel_id": "77", "content": text,
                "author": {"id": u.to_string(), "username": "m", "discriminator": "0", "avatar": null},
                "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
                "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [],
                "embeds": [], "components": [], "pinned": false, "type": 0}))
            .unwrap()
        };
        let said = |what: &str| t.calls(what);
        send_mail(&t.app, &dm("you are a badword"), gid)
            .await
            .unwrap();
        send_mail(&t.app, &dm("join discord.gg/abc"), gid)
            .await
            .unwrap();
        assert_eq!(said("words or links that server does not allow"), 2);
        let raid = tommybot_automod::join::RaidDetector::restored(0, i64::MAX, String::new());
        t.app.raids.lock().unwrap().insert(gid.get(), raid);
        send_mail(&t.app, &dm("help"), gid).await.unwrap();
        assert_eq!(said("does not take new tickets right now"), 1);
        t.app.raids.lock().unwrap().clear();
        for n in 1..=3 {
            sqlx::query("INSERT INTO tickets (guild_id, number, user_id, closed_at) VALUES ($1, $2, $3, now())")
                .bind(gid.get() as i64)
                .bind(n)
                .bind(u as i64)
                .execute(&t.app.pool)
                .await
                .unwrap();
        }
        send_mail(&t.app, &dm("help"), gid).await.unwrap();
        assert_eq!(said("too many tickets in that server"), 1);
        assert_eq!(
            said(&format!("POST /guilds/{gid}/channels")),
            0,
            "no ticket opened"
        );
        // A quarantined member still reaches staff; the intro tells staff they are quarantined.
        sqlx::query("DELETE FROM tickets WHERE guild_id = $1")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO quarantines (guild_id, user_id, saved_roles) VALUES ($1, $2, '{}')",
        )
        .bind(gid.get() as i64)
        .bind(u as i64)
        .execute(&t.app.pool)
        .await
        .unwrap();
        // Quarantined by staff with a reason: told why, so no ticket.
        let case = |m: u64, reason: Option<&str>, n: i32| {
            let t = &t;
            let reason = reason.map(String::from);
            async move {
                sqlx::query("INSERT INTO cases (guild_id, case_no, kind, user_id, mod_id, reason) VALUES ($1, $2, 'quarantine', $3, $4, $5)")
                    .bind(gid.get() as i64)
                    .bind(n)
                    .bind(u as i64)
                    .bind(m as i64)
                    .bind(reason)
                    .execute(&t.app.pool)
                    .await
                    .unwrap();
            }
        };
        case(crate::test_support::ME, Some("anti-nuke"), 1).await;
        case(9, Some("spamming"), 2).await;
        send_mail(&t.app, &dm("let me out"), gid).await.unwrap();
        assert_eq!(said("were given the reason"), 1);
        // That staff case is from an earlier, lifted quarantine: the current one is the bot's,
        // which told them nothing.
        sqlx::query("UPDATE cases SET created_at = now() - interval '1 day' WHERE guild_id = $1 AND case_no = 2")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        send_mail(&t.app, &dm("why am I quarantined?"), gid)
            .await
            .unwrap();
        assert_eq!(
            said(&format!("POST /guilds/{gid}/channels")),
            1,
            "ticket opened"
        );
        assert_eq!(said("Quarantined"), 1);
        // The server cap: 20 new tickets an hour from anyone.
        sqlx::query("INSERT INTO tickets (guild_id, number, user_id, closed_at) SELECT $1, n, n, now() FROM generate_series(100, 119) n")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE tickets SET closed_at = now() WHERE guild_id = $1")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM quarantines WHERE guild_id = $1")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tickets WHERE guild_id = $1 AND user_id = $2")
            .bind(gid.get() as i64)
            .bind(u as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        send_mail(&t.app, &dm("help"), gid).await.unwrap();
        assert_eq!(said("too many new tickets right now"), 1);
        sqlx::query("DELETE FROM ticket_users WHERE user_id = $1")
            .bind(u as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tickets WHERE guild_id = $1")
            .bind(gid.get() as i64)
            .execute(&t.app.pool)
            .await
            .unwrap();
        t.cleanup().await;
    }
}
