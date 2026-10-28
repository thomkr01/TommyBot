//! Wick's Interactive Whitelist: `w!whitelist TARGET` posts a multi-select of whitelist types with
//! Save / Cancel. The ids carry executor, target and the picked types, so nothing is stored until Save.
use anyhow::Result;
use serde_json::json;
use tommybot_core::permits::Level;
use tommybot_logging::embed::{self, GREEN, NO, ORANGE, YES};
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::{
        message::{
            component::{
                ActionRow, Button, ButtonStyle, Component, SelectMenu, SelectMenuOption,
                SelectMenuType,
            },
            AllowedMentions, Embed, MessageFlags,
        },
        ChannelType,
    },
    gateway::payload::incoming::MessageCreate,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::GuildMarker, Id},
};

use crate::{
    app::{App, GuildCfg},
    settings::{apply, may_edit, save, Edit},
    targets,
};

/// (value, label, description). Quarantine only applies to members.
const TYPES: [(&str, &str, &str); 5] = [
    (
        "spam",
        "Spam Whitelist",
        "Spam messages anywhere except for mention spam.",
    ),
    (
        "ping",
        "Ping Whitelist",
        "Spam mention members and roles anywhere.",
    ),
    (
        "ads",
        "Advertising Whitelist",
        "Post invites to other Discord servers.",
    ),
    (
        "quarantine",
        "Quarantine Whitelist",
        "Edit roles of other quarantined members.",
    ),
    (
        "everyone",
        "@everyone Ping Whitelist",
        "Mention @everyone or spam mention it.",
    ),
];

/// Target kinds: u member, r role, c channel, k category, w webhook.
fn field(kind: char) -> &'static str {
    match kind {
        'u' => "users",
        'r' => "roles",
        'c' => "channels",
        'k' => "categories",
        _ => "webhooks",
    }
}

/// Config path of type `t` for target kind `kind`; None when the type does not apply.
fn path(t: usize, kind: char) -> Option<String> {
    let f = field(kind);
    Some(match t {
        0 => format!("whitelist.{f}"),
        1 => format!("whitelist.mentions.{f}"),
        2 => format!("whitelist.invites.{f}"),
        3 if kind == 'u' => "antinuke.quarantine_whitelist".into(),
        3 => return None,
        _ => format!("whitelist.everyone.{f}"),
    })
}

fn current(cfg: &GuildCfg, kind: char, id: u64) -> Vec<bool> {
    let w = &cfg.whitelist;
    let has = |l: &tommybot_core::sections::WhitelistList| match kind {
        'u' => l.users.iter().any(|x| x.get() == id),
        'r' => l.roles.iter().any(|x| x.get() == id),
        'c' => l.channels.iter().any(|x| x.get() == id),
        'k' => l.categories.iter().any(|x| x.get() == id),
        _ => l.webhooks.iter().any(|x| x.get() == id),
    };
    vec![
        has(&w.spam),
        has(&w.mentions),
        has(&w.invites),
        cfg.antinuke
            .quarantine_whitelist
            .iter()
            .any(|x| x.get() == id),
        has(&w.everyone),
    ]
}

fn mention(kind: char, id: u64) -> String {
    match kind {
        'u' => embed::user(id),
        'r' => embed::role(id),
        'c' | 'k' => embed::channel(id),
        _ => format!("Webhook `{id}`"),
    }
}

fn mask(picked: &[bool]) -> String {
    picked.iter().map(|b| if *b { '1' } else { '0' }).collect()
}

/// The select and Save / Cancel rows. `allowed` hides types the target or executor cannot use.
fn rows(exec: u64, kind: char, id: u64, picked: &[bool], allowed: &[bool]) -> Vec<Component> {
    let options: Vec<_> = TYPES
        .iter()
        .enumerate()
        .filter(|(i, _)| allowed[*i])
        .map(|(i, (v, label, desc))| SelectMenuOption {
            default: picked[i],
            description: Some((*desc).into()),
            emoji: None,
            label: (*label).into(),
            value: (*v).into(),
        })
        .collect();
    let n = options.len() as u8;
    let select = Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id: format!("wl:sel:{exec}:{kind}:{id}"),
        default_values: None,
        disabled: false,
        kind: SelectMenuType::Text,
        max_values: Some(n),
        min_values: Some(0),
        options: Some(options),
        placeholder: Some("Whitelist types".into()),
        required: None,
    });
    let button = |label: &str, cid: String, style| {
        Component::Button(Button {
            id: None,
            custom_id: Some(cid),
            disabled: false,
            emoji: None,
            label: Some(label.into()),
            style,
            url: None,
            sku_id: None,
        })
    };
    vec![
        Component::ActionRow(ActionRow {
            id: None,
            components: vec![select],
        }),
        Component::ActionRow(ActionRow {
            id: None,
            components: vec![
                button(
                    "Save",
                    format!("wl:save:{exec}:{kind}:{id}:{}", mask(picked)),
                    ButtonStyle::Success,
                ),
                button("Cancel", format!("wl:no:{exec}"), ButtonStyle::Danger),
            ],
        }),
    ]
}

fn allowed(kind: char, level: &Level) -> Vec<bool> {
    (0..TYPES.len())
        .map(|t| path(t, kind).is_some() && (t != 3 || may_edit(level, "antinuke")))
        .collect()
}

fn prompt(kind: char, id: u64) -> Embed {
    embed::Log::new("🎟️ Interactive Whitelist", ORANGE)
        .text(&format!("Target: {}", mention(kind, id)))
        .build()
}

/// Resolves the typed target to (kind, id).
async fn target(app: &App, gid: Id<GuildMarker>, arg: &str) -> Result<Result<(char, u64), String>> {
    let digits = arg.trim_matches(|c: char| !c.is_ascii_digit());
    let id = digits
        .parse::<u64>()
        .ok()
        .filter(|_| arg.starts_with('<') || arg == digits);
    let Some(id) = id else {
        return Ok(targets::resolve(app, gid, arg)
            .await?
            .map(|u| ('u', u.get())));
    };
    if arg.starts_with("<@&") {
        return Ok(Ok(('r', id)));
    }
    if arg.starts_with("<@") {
        return Ok(Ok(('u', id)));
    }
    if app
        .http
        .roles(gid)
        .await?
        .models()
        .await?
        .iter()
        .any(|r| r.id.get() == id)
    {
        return Ok(Ok(('r', id)));
    }
    let channels = app.http.guild_channels(gid).await?.models().await?;
    if let Some(c) = channels.iter().find(|c| c.id.get() == id) {
        return Ok(Ok((
            if c.kind == ChannelType::GuildCategory {
                'k'
            } else {
                'c'
            },
            id,
        )));
    }
    if app.http.webhook(Id::new(id)).await.is_ok() {
        return Ok(Ok(('w', id)));
    }
    Ok(Ok(('u', id)))
}

pub async fn run(app: &App, msg: &MessageCreate, args: &str) -> Result<()> {
    let Some(gid) = msg.guild_id else {
        return Ok(());
    };
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, msg.author.id).await?;
    if level == Level::None {
        return Ok(());
    }
    let say = |t: String| async move {
        app.http
            .create_message(msg.channel_id)
            .content(&t)
            .allowed_mentions(Some(&AllowedMentions::default()))
            .await
            .map(drop)
    };
    if !may_edit(&level, "whitelist") {
        return Ok(say("You may not change the whitelist".into()).await?);
    }
    let arg = args.trim();
    if arg.is_empty() {
        return Ok(
            say("Usage: `whitelist @member|@role|#channel|category-id|webhook-id`".into()).await?,
        );
    }
    let (kind, id) = match target(app, gid, arg).await? {
        Ok(t) => t,
        Err(e) => return Ok(say(e).await?),
    };
    let picked = current(&*app.cfg(gid.get()).await?, kind, id);
    app.http
        .create_message(msg.channel_id)
        .embeds(&[prompt(kind, id)])
        .components(&rows(
            msg.author.id.get(),
            kind,
            id,
            &picked,
            &allowed(kind, &level),
        ))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

/// The select, Save and Cancel components. Returns false for other interactions.
pub async fn handle(app: &App, i: &Interaction) -> Result<bool> {
    let Some(InteractionData::MessageComponent(c)) = &i.data else {
        return Ok(false);
    };
    let Some(rest) = c.custom_id.strip_prefix("wl:") else {
        return Ok(false);
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let client = app.http.interaction(app.me.cast());
    let reply = |kind, data| InteractionResponse {
        kind,
        data: Some(data),
    };
    let exec = parts.get(1).and_then(|s| s.parse::<u64>().ok());
    let (Some(gid), Some(actor)) = (i.guild_id, i.author_id()) else {
        return Ok(true);
    };
    if exec != Some(actor.get()) {
        let data = InteractionResponseData {
            content: Some("Only the staff member who typed the command can use this.".into()),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        };
        client
            .create_response(
                i.id,
                &i.token,
                &reply(InteractionResponseType::ChannelMessageWithSource, data),
            )
            .await?;
        return Ok(true);
    }
    let owner = app.guild_info(gid).await?.owner;
    let level = app.level(gid, owner, actor).await?;
    let update = |data| reply(InteractionResponseType::UpdateMessage, data);
    let target = || -> Option<(char, u64)> {
        Some((parts.get(2)?.chars().next()?, parts.get(3)?.parse().ok()?))
    };
    let data = match (parts[0], target()) {
        ("sel", Some((kind, id))) => {
            let picked: Vec<bool> = TYPES
                .iter()
                .map(|(v, ..)| c.values.iter().any(|x| x == v))
                .collect();
            InteractionResponseData {
                components: Some(rows(actor.get(), kind, id, &picked, &allowed(kind, &level))),
                ..Default::default()
            }
        }
        ("save", Some((kind, id))) => {
            let ok = allowed(kind, &level);
            let picked: Vec<bool> = parts
                .get(4)
                .unwrap_or(&"")
                .chars()
                .map(|b| b == '1')
                .collect();
            let saved = save(app, gid, actor.get(), &["whitelist", "antinuke"], |doc| {
                if !may_edit(&level, "whitelist") {
                    return Err("You may not change the whitelist".into());
                }
                for t in (0..TYPES.len()).filter(|t| ok[*t]) {
                    let v = vec![json!(id.to_string())];
                    let want = picked.get(t).copied().unwrap_or(false);
                    let p = path(t, kind).expect("allowed type has a path");
                    apply(doc, &p, if want { Edit::Add(v) } else { Edit::Remove(v) });
                }
                Ok(())
            })
            .await?;
            match saved {
                Ok(doc) => {
                    let now = current(&GuildCfg::parse(&doc), kind, id);
                    let mut e = embed::Log::new("🎟️ Interactive Whitelist", GREEN);
                    for (t, (_, label, _)) in TYPES.iter().enumerate().filter(|(t, _)| ok[*t]) {
                        e = e.text(&format!("{} {label}", if now[t] { YES } else { NO }));
                    }
                    let e = e
                        .inline("👤 Executor", &embed::user(actor.get()))
                        .inline("🎟️ Target", &mention(kind, id));
                    InteractionResponseData {
                        embeds: Some(vec![e.build()]),
                        components: Some(vec![]),
                        ..Default::default()
                    }
                }
                Err(e) => InteractionResponseData {
                    content: Some(e),
                    components: Some(vec![]),
                    ..Default::default()
                },
            }
        }
        _ => InteractionResponseData {
            content: Some("Cancelled.".into()),
            embeds: Some(vec![]),
            components: Some(vec![]),
            ..Default::default()
        },
    };
    client
        .create_response(i.id, &i.token, &update(data))
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::check_section;

    #[test]
    fn every_type_writes_a_valid_section() {
        for kind in ['u', 'r', 'c', 'k', 'w'] {
            let mut doc = json!({});
            for t in 0..TYPES.len() {
                if let Some(p) = path(t, kind) {
                    apply(&mut doc, &p, Edit::Add(vec![json!("5")]));
                }
            }
            check_section("whitelist", &doc["whitelist"]).unwrap();
            if kind == 'u' {
                check_section("antinuke", &doc["antinuke"]).unwrap();
            }
            let got = current(&GuildCfg::parse(&doc), kind, 5);
            assert_eq!(got, vec![true, true, true, kind == 'u', true], "{kind}");
        }
    }

    #[test]
    fn quarantine_needs_an_owner() {
        assert_eq!(
            allowed('u', &Level::TrustedAdmin),
            vec![true, true, true, false, true]
        );
        assert_eq!(allowed('u', &Level::Owner), vec![true; 5]);
        assert!(!allowed('r', &Level::Owner)[3]);
    }
}
