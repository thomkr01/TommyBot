use super::staff::{close, relay};
use super::*;

const STAFF_CMDS: &[&str] = &[
    "reply",
    "r",
    "areply",
    "ar",
    "close",
    "c",
    "aclose",
    "ac",
    "closeall",
    "acloseall",
    "blacklist",
    "block",
    "whitelist",
    "unblock",
    "blacklistclear",
    "viewblacklist",
    "snippet",
    "s",
    "asnippet",
    "as",
    "snippetadd",
    "snippetremove",
    "snippetclear",
    "viewsnippet",
    "viewsnippets",
    "snippetlist",
    "vs",
];
const ADMIN_CMDS: &[&str] = &[
    "ticketsetup",
    "category",
    "accessrole",
    "modrole",
    "supportrole",
    "pingrole",
    "mentionrole",
    "logging",
    "logs",
    "commandonly",
    "commandrequired",
    "greetingmessage",
    "welcomemessage",
    "greetmessage",
    "closingmessage",
    "goodbyemessage",
    "closemessage",
    "loggingplus",
    "advancedlogging",
    "advancedlogs",
    "anonymous",
    "toggle",
    "enable",
    "disable",
    "viewconfig",
];
/// Short names other commands already use; they mean the ticket command only inside a ticket.
const TICKET_ONLY: &[&str] = &["r", "c", "s", "whitelist"];

/// Staff talk in a ticket is relayed to the member; ticket commands run anywhere. True when handled.
pub async fn on_message(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    rest: Option<&str>,
) -> Result<bool> {
    let cfg = app.cfg(gid.get()).await?;
    let word = rest.map(|r| {
        let r = r.trim_start();
        let (w, a) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
        (w.to_lowercase(), a.trim())
    });
    let Some((word, args)) = word else {
        // Plain message: relayed when in a ticket, unless it starts with a bot mention.
        let me = [format!("<@{}>", app.me), format!("<@!{}>", app.me)];
        if cfg.tickets.command_only
            || me.iter().any(|p| m.content.starts_with(p.as_str()))
            || (m.content.trim().is_empty() && m.attachments.is_empty())
        {
            return Ok(false);
        }
        let Some(user) = ticket_here(app, &cfg.tickets, gid, m.channel_id).await? else {
            return Ok(false);
        };
        relay(app, m, gid, user, &m.content, cfg.tickets.anonymous, false).await?;
        return Ok(true);
    };
    let w = word.as_str();
    let admin = ADMIN_CMDS.contains(&w);
    if !admin && !STAFF_CMDS.contains(&w) {
        return Ok(false);
    }
    let ticket = ticket_here(app, &cfg.tickets, gid, m.channel_id).await?;
    if TICKET_ONLY.contains(&w) && ticket.is_none() {
        return Ok(false);
    }
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, m.author.id).await?;
    let ch = m.channel_id;
    let roles = m.member.as_ref().map_or(&[][..], |mem| &mem.roles);
    let allowed = if admin {
        level.immune()
    } else {
        level.has("tickets") || roles.iter().any(|r| cfg.tickets.access_roles.contains(r))
    };
    if !allowed {
        say(app, ch, error("You do not have access to this command.")).await?;
        return Ok(true);
    }
    let pfx = cfg.misc.prefix.clone();
    let t = &cfg.tickets;
    if w != "ticketsetup" && w != "category" && t.category.is_none() {
        let text = format!("Your server has not been set up yet. Use `{pfx}ticketsetup` first.");
        say(app, ch, error(&text)).await?;
        return Ok(true);
    }
    let needs_ticket = matches!(
        w,
        "reply"
            | "r"
            | "areply"
            | "ar"
            | "close"
            | "c"
            | "aclose"
            | "ac"
            | "snippet"
            | "s"
            | "asnippet"
            | "as"
    );
    if needs_ticket && ticket.is_none() {
        say(app, ch, error("This channel is not a ticket channel.")).await?;
        return Ok(true);
    }
    run(app, m, gid, &cfg, ticket, w, args).await?;
    Ok(true)
}

/// Ids typed as mentions or numbers; `Err` when a word is neither.
fn ids(args: &str) -> Result<Vec<u64>, ()> {
    args.split_whitespace()
        .map(|t| {
            let d = t
                .trim_start_matches(['<', '@', '!', '&', '#'])
                .trim_end_matches('>');
            d.parse::<u64>().ok().filter(|n| *n > 0).ok_or(())
        })
        .collect()
}

async fn save(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    done: &str,
    f: impl FnOnce(&mut Value),
) -> Result<()> {
    let r = settings::save(app, gid, m.author.id.get(), &["tickets"], |doc| {
        if !doc["tickets"].is_object() {
            doc["tickets"] = json!({});
        }
        f(&mut doc["tickets"]);
        Ok(())
    })
    .await?;
    let reply = match r {
        Ok(_) => info(done),
        Err(e) => error(&e),
    };
    say(app, m.channel_id, reply).await
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    x.as_str()
                        .map(String::from)
                        .or_else(|| x.as_u64().map(|n| n.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn blocked_page(t: &TicketsCfg, page: usize) -> (Embed, usize) {
    let pages = t.blocked.len().div_ceil(25).max(1);
    let page = page.min(pages - 1);
    let lines: Vec<String> = t
        .blocked
        .iter()
        .skip(page * 25)
        .take(25)
        .map(|u| format!("<@{u}> ({u})"))
        .collect();
    (
        emb(
            &format!("{TICKET_BLOCK} Blacklist"),
            &lines.join("\n"),
            BLUE,
        )
        .build(),
        pages,
    )
}

pub fn snippets_page(t: &TicketsCfg, page: usize) -> (Embed, usize) {
    let pages = t.snippets.len().div_ceil(10).max(1);
    let page = page.min(pages - 1);
    let mut e = emb("Snippets", "", BLUE);
    for s in t.snippets.iter().skip(page * 10).take(10) {
        e = e.field(EmbedFieldBuilder::new(
            clip(&s.name, 256),
            clip(&s.content, 100),
        ));
    }
    (e.build(), pages)
}

async fn paged(app: &App, m: &MessageCreate, kind: &str, (e, pages): (Embed, usize)) -> Result<()> {
    let rows: Vec<Component> = (pages > 1)
        .then(|| settings::page_row(m.author.id.get(), kind, 0, pages))
        .into_iter()
        .collect();
    app.http
        .create_message(m.channel_id)
        .embeds(&[e])
        .components(&rows)
        .await?;
    Ok(())
}

async fn run(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    cfg: &crate::app::GuildCfg,
    ticket: Option<Id<UserMarker>>,
    w: &str,
    args: &str,
) -> Result<()> {
    let ch = m.channel_id;
    let t = &cfg.tickets;
    let pfx = &cfg.misc.prefix;
    let ok = |text: String| say(app, ch, info(&text));
    let no = |text: String| say(app, ch, error(&text));
    match w {
        "reply" | "r" | "areply" | "ar" => {
            if args.is_empty() && m.attachments.is_empty() {
                return no(format!("Usage: `{pfx}{w} <message>`")).await;
            }
            let Some(user) = ticket else { return Ok(()) };
            relay(app, m, gid, user, args, w.starts_with('a'), false).await
        }
        "snippet" | "s" | "asnippet" | "as" => {
            let Some(user) = ticket else { return Ok(()) };
            let name = args.to_lowercase();
            match t.snippets.iter().find(|s| s.name == name) {
                Some(s) => relay(app, m, gid, user, &s.content, w.starts_with('a'), true).await,
                None => no("The snippet was not found.".into()).await,
            }
        }
        "close" | "c" | "aclose" | "ac" => {
            let Some(user) = ticket else { return Ok(()) };
            close(app, gid, ch, user, &m.author, args, w.starts_with('a')).await
        }
        "closeall" | "acloseall" => {
            let anon = w == "acloseall";
            let open: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT channel_id, user_id FROM tickets WHERE guild_id = $1 AND closed_at IS NULL AND channel_id IS NOT NULL",
            )
            .bind(gid.get() as i64)
            .fetch_all(&app.pool)
            .await?;
            for (c, u) in open {
                if let Err(e) = close(
                    app,
                    gid,
                    Id::new(c as u64),
                    Id::new(u as u64),
                    &m.author,
                    args,
                    anon,
                )
                .await
                {
                    tracing::warn!(?e, channel = c, "closing a ticket failed");
                }
            }
            let text = if anon {
                "All tickets are successfully closed anonymously."
            } else {
                "All tickets are successfully closed."
            };
            best_effort(
                app.http.create_message(ch).embeds(&[info(text)]).await,
                "closeall reply",
            );
            Ok(())
        }
        "blacklist" | "block" | "whitelist" | "unblock" => {
            let add = matches!(w, "blacklist" | "block");
            let Ok(mut users) = ids(args) else {
                return no("The user(s) are not found. Please try again.".into()).await;
            };
            if users.is_empty() {
                users.extend(ticket.map(|u| u.get()));
            }
            if users.is_empty() {
                return no("You must provide users or run this command in a ticket.".into()).await;
            }
            let done = if add {
                "The user(s) are blacklisted successfully."
            } else {
                "The user(s) are whitelisted successfully."
            };
            save(app, m, gid, done, |tk| {
                let mut list = strings(&tk["blocked"]);
                for u in users.iter().map(u64::to_string) {
                    list.retain(|x| *x != u);
                    if add {
                        list.push(u);
                    }
                }
                tk["blocked"] = json!(list);
            })
            .await
        }
        "blacklistclear" => {
            save(
                app,
                m,
                gid,
                "The blacklist is cleared successfully.",
                |tk| tk["blocked"] = json!([]),
            )
            .await
        }
        "viewblacklist" if t.blocked.is_empty() => ok("No one is blacklisted.".into()).await,
        "viewblacklist" => paged(app, m, "tkblock", blocked_page(t, 0)).await,
        "snippetadd" => {
            let (name, content) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
            let (name, content) = (name.to_lowercase(), content.trim());
            if name.is_empty() || content.is_empty() {
                return no(format!("Usage: `{pfx}snippetadd <name> <content>`")).await;
            }
            if name.chars().count() > 100 {
                return no("The snippet name cannot exceed 100 characters.".into()).await;
            }
            if content.chars().count() > 1000 {
                return no("The snippet content cannot exceed 1000 characters.".into()).await;
            }
            if t.snippets.iter().any(|s| s.name == name) {
                return no("A snippet with that name already exists.".into()).await;
            }
            let author = m.author.id.to_string();
            let entry = json!({"name": name, "content": content, "author": author});
            save(app, m, gid, "The snippet was added successfully.", |tk| {
                let mut list = tk["snippets"].as_array().cloned().unwrap_or_default();
                list.push(entry);
                tk["snippets"] = json!(list);
            })
            .await
        }
        "snippetremove" => {
            let name = args.to_lowercase();
            if !t.snippets.iter().any(|s| s.name == name) {
                return no("A snippet with that name was not found.".into()).await;
            }
            save(app, m, gid, "The snippet was removed successfully.", |tk| {
                let mut list = tk["snippets"].as_array().cloned().unwrap_or_default();
                list.retain(|s| s["name"].as_str() != Some(name.as_str()));
                tk["snippets"] = json!(list);
            })
            .await
        }
        "snippetclear" => {
            save(
                app,
                m,
                gid,
                "All snippets were removed successfully.",
                |tk| tk["snippets"] = json!([]),
            )
            .await
        }
        "viewsnippet" | "viewsnippets" | "snippetlist" | "vs" if !args.is_empty() => {
            let name = args.to_lowercase();
            let Some(s) = t.snippets.iter().find(|s| s.name == name) else {
                return no("A snippet with that name was not found.".into()).await;
            };
            let by = s
                .author
                .map_or("*Unknown*".to_string(), |a| format!("<@{a}>"));
            let e = emb("Snippet", "", BLUE)
                .field(EmbedFieldBuilder::new("Name", clip(&s.name, 1024)))
                .field(EmbedFieldBuilder::new("Content", clip(&s.content, 1024)))
                .field(EmbedFieldBuilder::new("Author", by));
            say(app, ch, e.build()).await
        }
        "viewsnippet" | "viewsnippets" | "snippetlist" | "vs" if t.snippets.is_empty() => {
            ok("No snippet has been added yet.".into()).await
        }
        "viewsnippet" | "viewsnippets" | "snippetlist" | "vs" => {
            paged(app, m, "tksnip", snippets_page(t, 0)).await
        }
        "ticketsetup" | "category" => setup(app, m, gid, t, w, args).await,
        "accessrole" | "modrole" | "supportrole" | "pingrole" | "mentionrole" => {
            let ping = matches!(w, "pingrole" | "mentionrole");
            let mut here = false;
            let mut list = vec![];
            for tok in args.split_whitespace() {
                match tok.trim_start_matches('@').to_lowercase().as_str() {
                    "everyone" if ping => list.push(gid.to_string()),
                    "here" if ping => here = true,
                    _ => match ids(tok) {
                        Ok(v) => list.extend(v.iter().map(u64::to_string)),
                        Err(()) => {
                            return no("The role(s) are not found. Please try again.".into()).await
                        }
                    },
                }
            }
            if list.len() + usize::from(here) > 10 {
                return no("There can at most be 10 roles. Try using the command again but specify fewer roles.".into()).await;
            }
            save(app, m, gid, "The role(s) are updated successfully.", |tk| {
                if ping {
                    tk["ping_roles"] = json!(list);
                    tk["ping_here"] = json!(here);
                } else {
                    tk["access_roles"] = json!(list);
                }
            })
            .await
        }
        "logging" | "logs" => {
            let given = match ids(args) {
                Ok(v) => v.first().copied(),
                Err(()) => return no("The channel is not found. Please try again.".into()).await,
            };
            if t.log_channel.is_some() && given.is_none() {
                let done = "Ticket logging is disabled. You may delete the channel.";
                return save(app, m, gid, done, |tk| tk["log_channel"] = Value::Null).await;
            }
            let Some(cat) = t.category else { return Ok(()) };
            let log = match given {
                Some(c) => c,
                None => {
                    let ows = overwrites(gid, app.me.get(), &staff_roles(app, gid, t).await?);
                    app.http
                        .create_guild_channel(gid, "ticket-logs")
                        .kind(ChannelType::GuildText)
                        .parent_id(cat)
                        .permission_overwrites(&ows)
                        .await?
                        .model()
                        .await?
                        .id
                        .get()
                }
            };
            let done = "Ticket logging is enabled.";
            save(app, m, gid, done, |tk| {
                tk["log_channel"] = json!(log.to_string())
            })
            .await
        }
        "commandonly" | "commandrequired" | "anonymous" | "loggingplus" | "advancedlogging"
        | "advancedlogs" => {
            let (key, now, text) = match w {
                "commandonly" | "commandrequired" => {
                    ("command_only", !t.command_only, "Command only mode")
                }
                "anonymous" => ("anonymous", !t.anonymous, "Anonymous messaging"),
                _ => ("full_log", !t.full_log, "Advanced logging"),
            };
            let done = format!("{text} is {}.", if now { "enabled" } else { "disabled" });
            save(app, m, gid, &done, |tk| tk[key] = json!(now)).await
        }
        "greetingmessage" | "welcomemessage" | "greetmessage" | "closingmessage"
        | "goodbyemessage" | "closemessage" => {
            let key = if matches!(w, "greetingmessage" | "welcomemessage" | "greetmessage") {
                "greeting"
            } else {
                "closing"
            };
            let done = format!("The {key} message is set successfully.");
            save(app, m, gid, &done, |tk| tk[key] = json!(args)).await
        }
        "toggle" | "enable" | "disable" => {
            let pause = !t.paused;
            let done = format!(
                "Ticket creation is {}.",
                if pause { "disabled" } else { "enabled" }
            );
            save(app, m, gid, &done, |tk| {
                tk["paused"] = json!(pause);
                tk["pause_reason"] = json!(if pause { args } else { "" });
            })
            .await
        }
        "viewconfig" => say(app, ch, view_config(t, pfx, gid)).await,
        _ => Ok(()),
    }
}

fn view_config(t: &TicketsCfg, pfx: &str, gid: Id<GuildMarker>) -> Embed {
    let or_unset = |s: String| {
        if s.is_empty() {
            "*Not set*".to_string()
        } else {
            s
        }
    };
    let on = |b: bool| if b { "Enabled" } else { "Disabled" }.to_string();
    let access: Vec<String> = t.access_roles.iter().map(|r| format!("<@&{r}>")).collect();
    let mut ping: Vec<String> = t
        .ping_roles
        .iter()
        .map(|r| {
            if r.get() == gid.get() {
                "@everyone".into()
            } else {
                format!("<@&{r}>")
            }
        })
        .collect();
    if t.ping_here {
        ping.push("@here".into());
    }
    let reason = if t.pause_reason.is_empty() {
        "No reason was provided."
    } else {
        &t.pause_reason
    };
    let creation = if t.paused {
        format!("Disabled ({})", clip(reason, 989))
    } else {
        "Enabled".into()
    };
    let f = |n: &str, v: String| EmbedFieldBuilder::new(n, v).inline();
    emb("Server Configurations", "", BLUE)
        .field(f("Prefix", pfx.to_string()))
        .field(f(
            "Category",
            t.category.map_or("*Not set*".into(), |c| format!("<#{c}>")),
        ))
        .field(f("Access Roles", or_unset(access.join(" "))))
        .field(f("Ping Roles", or_unset(ping.join(" "))))
        .field(f(
            "Logging",
            t.log_channel
                .map_or("*Not set*".into(), |c| format!("<#{c}>")),
        ))
        .field(f("Advanced Logging", on(t.full_log)))
        .field(f("Anonymous Messaging", on(t.anonymous)))
        .field(f("Command Only", on(t.command_only)))
        .field(f("Ticket Creation", creation))
        .field(EmbedFieldBuilder::new(
            "Greeting Message",
            or_unset(clip(&t.greeting, 1000)),
        ))
        .field(EmbedFieldBuilder::new(
            "Closing Message",
            or_unset(clip(&t.closing, 1000)),
        ))
        .build()
}

/// `ticketsetup` makes the category and the log channel; `category [name]` only a new category.
async fn setup(
    app: &App,
    m: &MessageCreate,
    gid: Id<GuildMarker>,
    t: &TicketsCfg,
    w: &str,
    args: &str,
) -> Result<()> {
    let ch = m.channel_id;
    let pfx = app.cfg(gid.get()).await?.misc.prefix.clone();
    let channels = &app.guild_info(gid).await?.channels;
    let exists = t.category.is_some_and(|c| channels.contains(&c));
    let name = if w == "category" && !args.is_empty() {
        args
    } else {
        "Tickets"
    };
    if name.chars().count() > 100 {
        return say(
            app,
            ch,
            error("The category name cannot be longer than 100 characters"),
        )
        .await;
    }
    if exists {
        let text = if w == "category" {
            "A ticket category already exists. Please delete that category and try again."
        } else {
            "The bot has already been set up."
        };
        return say(app, ch, error(text)).await;
    }
    let ows = overwrites(gid, app.me.get(), &staff_roles(app, gid, t).await?);
    let category = app
        .http
        .create_guild_channel(gid, name)
        .kind(ChannelType::GuildCategory)
        .permission_overwrites(&ows)
        .await?
        .model()
        .await?
        .id;
    let log = if w == "ticketsetup" {
        let c = app
            .http
            .create_guild_channel(gid, "ticket-logs")
            .kind(ChannelType::GuildText)
            .parent_id(category)
            .permission_overwrites(&ows)
            .await?
            .model()
            .await?
            .id;
        Some(c.to_string())
    } else {
        None
    };
    let done = if w == "category" {
        "Successfully created the category.".to_string()
    } else {
        format!("Everything has been set up! Next up, you can give your staff access to ticket commands using `{pfx}accessrole [roles]` or a permit with Manage Tickets (the server owner, Extra Owners and Trusted Admins always have full access). You can also test things out by direct messaging me. Check out more information and configurations with `{pfx}help`.")
    };
    save(app, m, gid, &done, |tk| {
        tk["enabled"] = json!(true);
        tk["category"] = json!(category.to_string());
        if let Some(l) = log {
            tk["log_channel"] = json!(l);
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_take_mentions_and_numbers_only() {
        assert_eq!(ids("<@1> <@!2> <@&3> 4"), Ok(vec![1, 2, 3, 4]));
        assert_eq!(ids("bob"), Err(()));
        assert_eq!(ids(""), Ok(vec![]));
    }
}
