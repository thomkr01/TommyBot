//! Message context menus: Report Message (anyone) and the two purges (staff). A report is logged with
//! action buttons; the staff who press them need the Manage Reports permit plus the permit of the action.
use anyhow::Result;
use tommybot_logging::emoji::{Emoji, CLOSE, LINK, PAGE_FIRST, PAGE_LAST, PAGE_NEXT, PAGE_PREV};
use tommybot_logging::emoji::{AGE, MEMBER, NO, YES};
use tommybot_logging::{
    embed::{BLUE, GREEN, HANDLED, ORANGE},
    Log, LogKind,
};
use twilight_model::{
    application::interaction::{application_command::CommandData, Interaction, InteractionData},
    channel::{
        message::{
            component::{ActionRow, Button, ButtonStyle, Component, TextInput, TextInputStyle},
            embed::EmbedField,
            AllowedMentions, MessageFlags,
        },
        Message,
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{AutoModerationRuleMarker, ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::embed::EmbedBuilder;

use crate::{
    app::App,
    commands::{self, ModKind},
    modcmds::{self, Anchor, ModCmd},
};

/// One member cannot flood the report channel.
const REPORTS_PER_HOUR: i64 = 10;
const SNIPPET: usize = 300;
const ACTION_LOG: &str = "Action Log";

pub async fn message_menu(
    app: &App,
    data: &CommandData,
    gid: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
) -> Result<String> {
    let msg = data
        .target_id
        .zip(data.resolved.as_ref())
        .and_then(|(t, r)| r.messages.get(&t.cast()));
    let Some(msg) = msg else {
        return Ok("Message not found".into());
    };
    let from = match data.name.as_str() {
        "Purge 100 From Here" => Anchor::From(msg.id.get()),
        "Purge 100 Until Here" => Anchor::Until(msg.id.get()),
        _ => return Ok("Unknown command".into()),
    };
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    let cmd = ModCmd::Purge {
        count: 100,
        user: None,
        from: Some(from),
        filters: vec![],
    };
    modcmds::run(
        app,
        gid,
        owner,
        (actor_id, &actor),
        msg.channel_id,
        cmd,
        None,
        None,
    )
    .await
}

fn button(label: &str, id: i64, action: &str, style: ButtonStyle) -> Component {
    Component::Button(Button {
        id: None,
        custom_id: Some(format!("report:{id}:{action}")),
        disabled: false,
        emoji: None,
        label: Some(label.into()),
        style,
        url: None,
        sku_id: None,
    })
}

fn rows(id: i64, jump: String, deletable: bool) -> Vec<Component> {
    let row = |components| {
        Component::ActionRow(ActionRow {
            id: None,
            components,
        })
    };
    let link = Component::Button(LINK.on(Button {
        id: None,
        custom_id: None,
        disabled: false,
        emoji: None,
        label: Some("View Message ↗".into()),
        style: ButtonStyle::Link,
        url: Some(jump),
        sku_id: None,
    }));
    vec![
        row(std::iter::once(link)
            .chain(deletable.then(|| button("Delete Message", id, "delete", ButtonStyle::Success)))
            .chain([button("Dismiss Report", id, "dismiss", ButtonStyle::Danger)])
            .collect()),
        row(vec![
            button("User Info", id, "cases", ButtonStyle::Secondary),
            button("Warn", id, "warn", ButtonStyle::Secondary),
            button("Timeout", id, "timeout", ButtonStyle::Secondary),
            button("Kick", id, "kick", ButtonStyle::Secondary),
            button("Ban", id, "ban", ButtonStyle::Secondary),
        ]),
    ]
}

async fn report(
    app: &App,
    gid: Id<GuildMarker>,
    reporter: Id<UserMarker>,
    msg: &Message,
    reporter_name: &str,
    reason: &str,
) -> Result<Option<String>> {
    let g = gid.get() as i64;
    let Some(log) = app
        .cfg(gid.get())
        .await?
        .logging
        .target(LogKind::Report, None)
    else {
        return Ok(Some("Reports are not set up on this server".into()));
    };
    if msg.author.bot {
        return Ok(Some(
            "Messages from bots and webhooks cannot be reported".into(),
        ));
    }
    if msg.author.id == reporter {
        return Ok(Some("You cannot report your own message".into()));
    }
    let banned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM report_blacklist WHERE guild_id=$1 AND user_id=$2)",
    )
    .bind(g)
    .bind(reporter.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    if banned {
        return Ok(Some(
            "You can no longer report messages in this server".into(),
        ));
    }
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reports WHERE guild_id=$1 AND reporter_id=$2 AND created_at > now() - interval '1 hour'",
    )
    .bind(g)
    .bind(reporter.get() as i64)
    .fetch_one(&app.pool)
    .await?;
    if recent >= REPORTS_PER_HOUR {
        return Ok(Some(
            "You reported a lot of messages this hour, try again later".into(),
        ));
    }
    if app.whitelisted(gid, msg.author.id, "reports").await? {
        return Ok(Some("This member cannot be reported".into()));
    }
    let content: String = msg.content.chars().take(SNIPPET).collect();
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO reports (guild_id, reporter_id, target_id, channel_id, message_id, content, reason)
         VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(g)
    .bind(reporter.get() as i64)
    .bind(msg.author.id.get() as i64)
    .bind(msg.channel_id.get() as i64)
    .bind(msg.id.get() as i64)
    .bind(&content)
    .bind(reason)
    .fetch_optional(&app.pool)
    .await?;
    let Some(id) = id else {
        return Ok(Some("You already reported this message".into()));
    };
    let jump = format!(
        "https://discord.com/channels/{gid}/{}/{}",
        msg.channel_id, msg.id
    );
    let created = ((msg.author.id.get() >> 22) + 1_420_070_400_000) / 1000;
    let embed = Log::new("", ORANGE)
        .author(
            &msg.author,
            &format!("A message by {} has been reported!", msg.author.name),
        )
        .thumbnail(&msg.author)
        .text(&format!(
            "<@{}>'s [message]({jump}) has been reported by <@{reporter}>",
            msg.author.id
        ))
        .text(&format!(
            "{MEMBER} **Member:** {} [{}]",
            msg.author.name, msg.author.id
        ))
        .text(&format!("{AGE} **Creation:** <t:{created}:F>"))
        .time()
        .field(
            "Reported Message:",
            &if content.is_empty() {
                "(no text)".into()
            } else {
                format!("||{}||", content.replace("||", "|\u{200b}|"))
            },
        )
        .field("Reason:", reason)
        .footer(&format!(
            "Reported by {reporter_name} | TARGET: {}",
            msg.author.id
        ))
        .build();
    let sent = app
        .http
        .create_message(log)
        .embeds(&[embed])
        .components(&rows(id, jump, true))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await;
    if let Err(e) = sent {
        sqlx::query("DELETE FROM reports WHERE id=$1")
            .bind(id)
            .execute(&app.pool)
            .await?;
        return Err(e.into());
    }
    Ok(None)
}

/// Files a report for a message Discord's AutoMod already blocked, with the same action buttons
/// (minus Delete, the message is gone). `false` when this server has no report channel.
pub async fn file_native(
    app: &App,
    gid: Id<GuildMarker>,
    channel: Option<Id<ChannelMarker>>,
    target: Id<UserMarker>,
    content: &str,
    rule: Id<AutoModerationRuleMarker>,
) -> Result<bool> {
    let Some(log) = app
        .cfg(gid.get())
        .await?
        .logging
        .target(LogKind::Report, None)
    else {
        return Ok(false);
    };
    let content: String = content.chars().take(SNIPPET).collect();
    // The blocked message has no id; the clock keeps the (guild, reporter, message) key unique.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO reports (guild_id, reporter_id, target_id, channel_id, message_id, content, reason)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(gid.get() as i64)
    .bind(app.me.get() as i64)
    .bind(target.get() as i64)
    .bind(channel.map_or(0, |c| c.get() as i64))
    .bind(crate::app::now_secs() * 1000 + rand::random::<u8>() as i64)
    .bind(&content)
    .bind(format!("Discord AutoMod rule {rule}"))
    .fetch_one(&app.pool)
    .await?;
    let jump = format!(
        "https://discord.com/channels/{gid}/{}",
        channel.map_or(String::new(), |c| c.to_string())
    );
    let embed = Log::new("Discord AutoMod Report", ORANGE)
        .text(&format!(
            "<@{target}>'s message was blocked by Discord AutoMod rule `{rule}`"
        ))
        .time()
        .field(
            "Blocked Message:",
            &if content.is_empty() {
                "(no text)".into()
            } else {
                format!("||{}||", content.replace("||", "|\u{200b}|"))
            },
        )
        .footer(&format!("Reported by TommyBot | TARGET: {target}"))
        .build();
    let sent = app
        .http
        .create_message(log)
        .embeds(&[embed])
        .components(&rows(id, jump, false))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await;
    if let Err(e) = sent {
        sqlx::query("DELETE FROM reports WHERE id=$1")
            .bind(id)
            .execute(&app.pool)
            .await?;
        return Err(e.into());
    }
    Ok(true)
}

const MODAL: &str = "reportm:";

/// Report Message asks for a reason first ("Report Message" modal).
#[allow(deprecated)]
async fn open_modal(app: &App, i: &Interaction, data: &CommandData) -> Result<()> {
    let msg = data
        .target_id
        .zip(data.resolved.as_ref())
        .and_then(|(t, r)| r.messages.get(&t.cast()));
    let Some(msg) = msg else { return Ok(()) };
    let input = Component::TextInput(TextInput {
        id: None,
        custom_id: "reason".into(),
        label: Some("Reason".into()),
        max_length: Some(1000),
        min_length: Some(1),
        placeholder: None,
        required: Some(true),
        style: TextInputStyle::Paragraph,
        value: None,
    });
    let resp = InteractionResponse {
        kind: InteractionResponseType::Modal,
        data: Some(InteractionResponseData {
            custom_id: Some(format!("{MODAL}{}:{}", msg.channel_id, msg.id)),
            title: Some("Report Message".into()),
            components: Some(vec![Component::ActionRow(ActionRow {
                id: None,
                components: vec![input],
            })]),
            ..Default::default()
        }),
    };
    app.http
        .interaction(Id::new(app.me.get()))
        .create_response(i.id, &i.token, &resp)
        .await?;
    Ok(())
}

async fn submit_report(app: &App, i: &Interaction, ids: &str, reason: &str) -> Result<()> {
    let client = app.http.interaction(Id::new(app.me.get()));
    let defer = InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    };
    client.create_response(i.id, &i.token, &defer).await?;
    let ids = ids
        .split_once(':')
        .and_then(|(c, m)| Some((c.parse::<u64>().ok()?, m.parse::<u64>().ok()?)));
    let (Some(gid), Some(reporter), Some((c, m))) = (i.guild_id, i.author(), ids) else {
        return Ok(());
    };
    let refusal = match app.http.message(Id::new(c), Id::new(m)).await {
        Ok(r) => {
            let msg = r.model().await?;
            report(app, gid, reporter.id, &msg, &reporter.name, reason).await?
        }
        Err(_) => Some("Message not found".into()),
    };
    let up = client.update_response(&i.token);
    match refusal {
        Some(text) => up.content(Some(&text)).await?,
        None => {
            let done = [EmbedBuilder::new()
                .color(GREEN)
                .description(format!(
                    "{YES} This message has been flagged and sent to the staff team for a review!"
                ))
                .build()];
            up.embeds(Some(&done)).await?
        }
    };
    Ok(())
}

/// The pressed button's effect: the private answer and the history line it adds to the report.
struct Outcome {
    text: String,
    /// `(line, closes the report)`
    line: Option<(String, bool)>,
}

fn refused(text: impl Into<String>) -> Outcome {
    Outcome {
        text: text.into(),
        line: None,
    }
}

async fn act(
    app: &App,
    gid: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
    id: i64,
    action: &str,
) -> Result<Outcome> {
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    if !actor.has("reports") {
        return Ok(refused("You need the Manage Reports permit"));
    }
    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT target_id, channel_id, message_id FROM reports WHERE id=$1 AND guild_id=$2",
    )
    .bind(id)
    .bind(gid.get() as i64)
    .fetch_optional(&app.pool)
    .await?;
    let Some((target, channel, message)) = row else {
        return Ok(refused("Report not found"));
    };
    let target = Id::new(target as u64);
    let who = (actor_id, &actor);
    let reason = format!("Report #{id}");
    let (kind, name) = match action {
        "dismiss" => {
            return Ok(Outcome {
                text: "Report dismissed".into(),
                line: Some((
                    format!(
                        "- **Report was dismissed by** <@{actor_id}> <t:{}:R>",
                        crate::app::now_secs()
                    ),
                    true,
                )),
            })
        }
        "delete" => {
            let gone = app
                .http
                .delete_message(Id::new(channel as u64), Id::new(message as u64))
                .await;
            return Ok(match gone {
                Ok(_) => Outcome {
                    text: "Message deleted".into(),
                    line: Some((
                        format!(
                            "- Message reported has been deleted by <@{actor_id}> <t:{}:R>",
                            crate::app::now_secs()
                        ),
                        true,
                    )),
                },
                Err(_) => refused("Could not delete the message, it may be gone"),
            });
        }
        "cases" => {
            let cmd = ModCmd::CasesView {
                case: None,
                user: Some(target.get()),
                by: None,
                kind: None,
            };
            let text = modcmds::run(
                app,
                gid,
                owner,
                who,
                Id::new(channel as u64),
                cmd,
                None,
                None,
            )
            .await?;
            return Ok(Outcome { text, line: None });
        }
        "warn" => (ModKind::Warn, "Warn"),
        "timeout" => (ModKind::Timeout, "Timeout"),
        "kick" => (ModKind::Kick, "Kick"),
        "ban" => (ModKind::Ban, "Ban"),
        _ => return Ok(refused("Unknown action")),
    };
    let attempted = |text: String| Outcome {
        line: Some((
            format!(
                "- <@{actor_id}> attempted to **{name}** <t:{}:R>",
                crate::app::now_secs()
            ),
            false,
        )),
        text,
    };
    if kind == ModKind::Warn && app.http.ban(gid, target).await.is_ok() {
        return Ok(attempted("This user is already banned".into()));
    }
    let points = app.cfg(gid.get()).await?.misc.warn_base_points;
    let text = commands::apply(
        app,
        gid,
        owner,
        who,
        kind,
        target,
        Some(&reason),
        None,
        points,
    )
    .await?;
    // A refusal never opens a case, so "Case #" tells the two apart.
    if !text.starts_with("Case #") {
        return Ok(attempted(text));
    }
    Ok(Outcome {
        text,
        line: Some((
            format!(
                "- <@{actor_id}> issued a **{name}** <t:{}:R>",
                crate::app::now_secs()
            ),
            true,
        )),
    })
}

const PER_PAGE: i64 = 8;

/// The page to show (clamped) and the page count for `total` cases.
fn paging(page: i64, total: i64) -> (i64, i64) {
    let pages = ((total + PER_PAGE - 1) / PER_PAGE).max(1);
    (page.clamp(0, pages - 1), pages)
}

/// "User Info": an overview embed and the member's cases, paged with the page buttons. `arg` is empty for
/// the first press (needs the Manage Reports permit) or `page:slot` / `x` from the pager.
async fn user_info(
    app: &App,
    i: &Interaction,
    gid: Id<GuildMarker>,
    actor_id: Id<UserMarker>,
    id: i64,
    arg: &str,
) -> Result<()> {
    let client = app.http.interaction(Id::new(app.me.get()));
    if arg == "x" {
        client.delete_response(&i.token).await?;
        return Ok(());
    }
    let first = arg.is_empty();
    if first {
        let owner = app.guild_info(gid).await?.owner;
        if !app.level(gid, owner, actor_id).await?.has("reports") {
            let m = client
                .create_followup(&i.token)
                .content("You need the Manage Reports permit");
            m.flags(MessageFlags::EPHEMERAL).await?;
            return Ok(());
        }
    }
    let page: i64 = arg
        .split(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let target: i64 =
        sqlx::query_scalar("SELECT target_id FROM reports WHERE id=$1 AND guild_id=$2")
            .bind(id)
            .bind(gid.get() as i64)
            .fetch_one(&app.pool)
            .await?;
    let user = app.http.user(Id::new(target as u64)).await?.model().await?;
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cases WHERE guild_id=$1 AND user_id=$2")
            .bind(gid.get() as i64)
            .bind(target)
            .fetch_one(&app.pool)
            .await?;
    let (page, pages) = paging(page, total);
    let rows: Vec<(i32, String, bool, i64)> = sqlx::query_as(
        "SELECT case_no, kind, active, extract(epoch from created_at)::bigint FROM cases
         WHERE guild_id=$1 AND user_id=$2 ORDER BY case_no DESC LIMIT $3 OFFSET $4",
    )
    .bind(gid.get() as i64)
    .bind(target)
    .bind(PER_PAGE)
    .bind(page * PER_PAGE)
    .fetch_all(&app.pool)
    .await?;
    let created = ((user.id.get() >> 22) + 1_420_070_400_000) / 1000;
    let badges = user
        .public_flags
        .filter(|f| !f.is_empty())
        .map_or("None".into(), |f| format!("{f:?}"));
    let overview = Log::new(&format!("{} Overview:", user.name), BLUE)
        .thumbnail(&user)
        .line("Username", &user.name)
        .line("Tag", &format!("{}#{:04}", user.name, user.discriminator))
        .line("ID", &format!("`{}`", user.id))
        .line("Creation", &format!("<t:{created}:F>"))
        .line("Badges", &badges)
        .build();
    let list: Vec<String> = rows
        .iter()
        .map(|(n, k, active, at)| {
            format!(
                "{} {n} [+{}] <t:{at}:R>",
                if *active { YES } else { NO },
                k.to_uppercase()
            )
        })
        .collect();
    let cases = Log::new(&format!("{} Mod Cases ({total}):", user.name), BLUE)
        .text(&if list.is_empty() {
            "No cases".into()
        } else {
            list.join("\n")
        })
        .footer(&format!("Page {}/{pages}", page + 1))
        .build();
    let embeds = [overview, cases];
    let pager = pager(id, page, pages);
    if first {
        client
            .create_followup(&i.token)
            .embeds(&embeds)
            .components(&[pager])
            .flags(MessageFlags::EPHEMERAL)
            .await?;
    } else {
        client
            .update_response(&i.token)
            .embeds(Some(&embeds))
            .components(Some(&[pager]))
            .await?;
    }
    Ok(())
}

fn pager(id: i64, page: i64, pages: i64) -> Component {
    let last = pages - 1;
    let b = |icon: Emoji, to: i64, slot: &str, style, off| match button(
        icon.fallback,
        id,
        &format!("cases:{to}:{slot}"),
        style,
    ) {
        Component::Button(b) => Component::Button(icon.on(Button { disabled: off, ..b })),
        c => c,
    };
    let grey = ButtonStyle::Secondary;
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            b(PAGE_FIRST, 0, "f", grey, page == 0),
            b(PAGE_PREV, (page - 1).max(0), "p", grey, page == 0),
            b(PAGE_NEXT, (page + 1).min(last), "n", grey, page >= last),
            b(PAGE_LAST, last, "l", grey, page >= last),
            b(CLOSE, 0, "x", ButtonStyle::Danger, false),
        ],
    })
}

/// Handles the report command's modal and the buttons of a report log message; `false` when the
/// interaction is not one of them.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let c = match &i.data {
        Some(InteractionData::ApplicationCommand(d)) if d.name == "Report Message" => {
            open_modal(app, i, d).await?;
            return Ok(true);
        }
        Some(InteractionData::ModalSubmit(m)) if m.custom_id.starts_with(MODAL) => {
            let reason = m
                .components
                .iter()
                .find_map(crate::verify::typed_text)
                .unwrap_or_default();
            submit_report(app, i, &m.custom_id[MODAL.len()..], reason.trim()).await?;
            return Ok(true);
        }
        Some(InteractionData::MessageComponent(c)) => c,
        _ => return Ok(false),
    };
    let parsed = c
        .custom_id
        .strip_prefix("report:")
        .and_then(|r| r.split_once(':'));
    let Some((id, action)) = parsed.and_then(|(id, a)| Some((id.parse::<i64>().ok()?, a))) else {
        return Ok(false);
    };
    let (Some(gid), Some(actor_id)) = (i.guild_id, i.author_id()) else {
        return Ok(true);
    };
    let client = app.http.interaction(Id::new(app.me.get()));
    let defer = InteractionResponse {
        kind: InteractionResponseType::DeferredUpdateMessage,
        data: None,
    };
    client.create_response(i.id, &i.token, &defer).await?;
    if let Some(arg) = action.strip_prefix("cases") {
        user_info(app, i, gid, actor_id, id, arg.trim_start_matches(':')).await?;
        return Ok(true);
    }
    let Outcome { text, line } = act(app, gid, actor_id, id, action)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(?e, action, "report action failed");
            refused(format!(
                "Something went wrong: {}",
                crate::http_err::why(&e)
            ))
        });
    if let Some((line, closes)) = line {
        if closes {
            let status = if action == "dismiss" {
                "dismissed"
            } else {
                "actioned"
            };
            sqlx::query(
                "UPDATE reports SET status=$2, action=$3, handled_by=$4, handled_at=now() WHERE id=$1",
            )
            .bind(id)
            .bind(status)
            .bind(action)
            .bind(actor_id.get() as i64)
            .execute(&app.pool)
            .await?;
        }
        // It keeps the report's history in an "Action Log" field and turns the log green.
        if let Some(mut embed) = i.message.as_ref().and_then(|m| m.embeds.first().cloned()) {
            match embed.fields.iter_mut().find(|f| f.name == ACTION_LOG) {
                // A field holds 1024 characters; a very long history just stops growing.
                Some(f) if f.value.len() + line.len() < 1024 => {
                    f.value = format!("{}\n{line}", f.value)
                }
                Some(_) => {}
                None => embed.fields.push(EmbedField {
                    inline: false,
                    name: ACTION_LOG.into(),
                    value: line,
                }),
            }
            if closes {
                embed.color = Some(HANDLED);
            }
            let embeds = [embed];
            let u = client.update_response(&i.token).embeds(Some(&embeds));
            if matches!(action, "dismiss" | "delete") {
                u.components(Some(&[])).await?;
            } else {
                u.await?;
            }
        }
    }
    client
        .create_followup(&i.token)
        .content(&text)
        .flags(MessageFlags::EPHEMERAL)
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn paging_clamps_and_counts() {
        assert_eq!(super::paging(5, 0), (0, 1));
        assert_eq!(super::paging(9, 17), (2, 3));
        assert_eq!(super::paging(1, 16), (1, 2));
    }
}
