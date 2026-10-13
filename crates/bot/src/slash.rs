//! Slash commands and user context menus. No default_member_permissions: staff need no Discord
//! permissions (spec 2.6), the bot checks permits instead.
use anyhow::Result;
use tommybot_core::duration;
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            application_command::{CommandDataOption, CommandOptionValue},
            Interaction, InteractionContextType, InteractionData,
        },
    },
    channel::message::{AllowedMentions, MessageFlags},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{GenericMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::command::{CommandBuilder, IntegerBuilder, StringBuilder, UserBuilder};

use crate::{
    app::App,
    commands::{self, ModKind},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Mod {
        kind: ModKind,
        target: Id<UserMarker>,
        reason: Option<String>,
        duration_s: Option<i64>,
        points: i32,
    },
    Lock,
    Unlock,
    Revert {
        target: Id<UserMarker>,
        since_s: i64,
    },
}

fn user(o: &[CommandDataOption], name: &str) -> Option<Id<UserMarker>> {
    o.iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            CommandOptionValue::User(u) => Some(u),
            _ => None,
        })
}

fn string<'a>(o: &'a [CommandDataOption], name: &str) -> Option<&'a str> {
    o.iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandOptionValue::String(s) => Some(s.as_str()),
            _ => None,
        })
}

fn int(o: &[CommandDataOption], name: &str) -> Option<i64> {
    o.iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            CommandOptionValue::Integer(n) => Some(n),
            _ => None,
        })
}

/// Optional duration option: absent is fine, unparseable is an error.
fn dur(o: &[CommandDataOption], name: &str) -> Result<Option<i64>, String> {
    string(o, name)
        .map(|s| {
            duration::parse(s)
                .ok_or_else(|| format!("`{s}` is not a duration, use e.g. 10m, 2h, 5d"))
        })
        .transpose()
}

/// `target_id` is the clicked user of a context-menu command.
pub fn parse(
    name: &str,
    o: &[CommandDataOption],
    target_id: Option<Id<GenericMarker>>,
) -> Result<Cmd, String> {
    let kind = match name {
        "ban" | "Ban" => ModKind::Ban,
        "kick" | "Kick" => ModKind::Kick,
        "timeout" | "Timeout" => ModKind::Timeout,
        "warn" => ModKind::Warn,
        "lock" => return Ok(Cmd::Lock),
        "unlock" => return Ok(Cmd::Unlock),
        "revert" => {
            return Ok(Cmd::Revert {
                target: user(o, "actor").ok_or("missing actor")?,
                since_s: dur(o, "since")?.ok_or("missing since")?,
            })
        }
        _ => return Err("Unknown command".into()),
    };
    let menu = name.starts_with(|c: char| c.is_uppercase());
    let target = if menu {
        target_id.map(Id::cast)
    } else {
        user(o, "user")
    }
    .ok_or("missing user")?;
    let duration_s = match (kind, dur(o, "duration")?) {
        (ModKind::Timeout, None) if !menu => return Err("missing duration".into()),
        (ModKind::Timeout, None) => Some(600),
        (_, d) => d,
    };
    Ok(Cmd::Mod {
        kind,
        target,
        reason: string(o, "reason").map(String::from),
        duration_s,
        points: int(o, "points").unwrap_or(1) as i32,
    })
}

pub fn definitions() -> Vec<Command> {
    let user = || UserBuilder::new("user", "Target member").required(true);
    let reason = || StringBuilder::new("reason", "Reason for the case");
    let guild_only = |b: CommandBuilder| b.contexts([InteractionContextType::Guild]).build();
    let chat = |n: &str, d: &str| CommandBuilder::new(n, d, CommandType::ChatInput);
    let menu = |n: &str| CommandBuilder::new(n, "", CommandType::User);
    vec![
        guild_only(
            chat("ban", "Ban a member")
                .option(user())
                .option(reason())
                .option(StringBuilder::new("duration", "Length, e.g. 7d")),
        ),
        guild_only(
            chat("kick", "Kick a member")
                .option(user())
                .option(reason()),
        ),
        guild_only(
            chat("timeout", "Time a member out")
                .option(user())
                .option(StringBuilder::new("duration", "Length, e.g. 10m").required(true))
                .option(reason()),
        ),
        guild_only(
            chat("warn", "Warn a member")
                .option(user())
                .option(reason())
                .option(IntegerBuilder::new("points", "Warn points (default 1)").min_value(1)),
        ),
        guild_only(chat("lock", "Lock the server")),
        guild_only(chat("unlock", "Unlock the server")),
        guild_only(
            chat("revert", "Undo a member's recent logged actions")
                .option(UserBuilder::new("actor", "Whose actions to undo").required(true))
                .option(StringBuilder::new("since", "How far back, e.g. 2h").required(true)),
        ),
        guild_only(menu("Ban")),
        guild_only(menu("Kick")),
        guild_only(menu("Timeout")),
    ]
}

pub async fn register(app: &App) -> Result<()> {
    app.http
        .interaction(Id::new(app.me.get()))
        .set_global_commands(&definitions())
        .await?;
    Ok(())
}

pub async fn handle(app: &App, i: &Interaction) -> Result<()> {
    let Some(InteractionData::ApplicationCommand(data)) = &i.data else {
        return Ok(());
    };
    let client = app.http.interaction(Id::new(app.me.get()));
    // Several HTTP calls can exceed the 3s answer window, so acknowledge first.
    client
        .create_response(
            i.id,
            &i.token,
            &InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: Some(InteractionResponseData {
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                }),
            },
        )
        .await?;
    let text = match run(app, i, data).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(?e, command = %data.name, "slash command failed");
            "Something went wrong, see logs".into()
        }
    };
    client
        .update_response(&i.token)
        .content(Some(&text))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

async fn run(
    app: &App,
    i: &Interaction,
    data: &twilight_model::application::interaction::application_command::CommandData,
) -> Result<String> {
    let (Some(gid), Some(actor_id)) = (i.guild_id, i.author_id()) else {
        return Ok("Server only".into());
    };
    let cmd = match parse(&data.name, &data.options, data.target_id) {
        Ok(c) => c,
        Err(e) => return Ok(e),
    };
    let owner = app.http.guild(gid).await?.model().await?.owner_id;
    let actor = app.level(gid, owner, actor_id).await?;
    match cmd {
        Cmd::Mod {
            kind,
            target,
            reason,
            duration_s,
            points,
        } => {
            commands::apply(
                app,
                gid,
                owner,
                (actor_id, &actor),
                kind,
                target,
                reason.as_deref(),
                duration_s,
                points,
            )
            .await
        }
        Cmd::Lock | Cmd::Unlock => {
            commands::lockdown_cmd(app, gid, &actor, cmd == Cmd::Unlock).await
        }
        Cmd::Revert { target, since_s } => {
            commands::revert_cmd(app, gid, (actor_id, &actor), target, since_s).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(name: &str, value: CommandOptionValue) -> CommandDataOption {
        CommandDataOption {
            name: name.into(),
            value,
        }
    }

    #[test]
    fn parses_slash_and_menu() {
        let u = Id::new(7);
        let o = [
            opt("user", CommandOptionValue::User(u)),
            opt("duration", CommandOptionValue::String("2h".into())),
            opt("reason", CommandOptionValue::String("spam".into())),
        ];
        assert_eq!(
            parse("timeout", &o, None),
            Ok(Cmd::Mod {
                kind: ModKind::Timeout,
                target: u,
                reason: Some("spam".into()),
                duration_s: Some(7200),
                points: 1
            })
        );
        assert!(parse("timeout", &o[..1], None).is_err()); // duration required
        assert!(parse(
            "ban",
            &[
                opt("user", CommandOptionValue::User(u)),
                opt("duration", CommandOptionValue::String("x".into()))
            ],
            None
        )
        .is_err());
        // context menu: target comes from target_id, timeout defaults to 10m
        assert_eq!(
            parse("Timeout", &[], Some(Id::new(9))),
            Ok(Cmd::Mod {
                kind: ModKind::Timeout,
                target: Id::new(9),
                reason: None,
                duration_s: Some(600),
                points: 1
            })
        );
        let r = [
            opt("actor", CommandOptionValue::User(u)),
            opt("since", CommandOptionValue::String("2h".into())),
        ];
        assert_eq!(
            parse("revert", &r, None),
            Ok(Cmd::Revert {
                target: u,
                since_s: 7200
            })
        );
        assert_eq!(
            parse(
                "warn",
                &[
                    opt("user", CommandOptionValue::User(u)),
                    opt("points", CommandOptionValue::Integer(3))
                ],
                None
            )
            .map(|c| matches!(c, Cmd::Mod { points: 3, .. })),
            Ok(true)
        );
    }
}
