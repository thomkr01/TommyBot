//! Slash commands and user context menus. No default_member_permissions: staff need no Discord
//! permissions (spec 2.6), the bot checks permits instead.
use anyhow::Result;
use tommybot_automod::lockdown::{kind_from, LockKind};
use tommybot_core::{duration, permits};
use twilight_model::{
    application::{
        command::{Command, CommandType},
        interaction::{
            application_command::{CommandDataOption, CommandOptionValue},
            Interaction, InteractionContextType, InteractionData,
        },
    },
    channel::message::{AllowedMentions, Component, MessageFlags},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{
        marker::{GenericMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::command::{
    BooleanBuilder, CommandBuilder, IntegerBuilder, StringBuilder, SubCommandBuilder, UserBuilder,
};

use crate::{
    app::App,
    commands::{self, Action, ModKind, Pending},
    modcmds::{self, ModCmd},
    prefix::Flags,
    reports,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Mod {
        kind: ModKind,
        target: Id<UserMarker>,
        reason: Option<String>,
        duration_s: Option<i64>,
        points: i32,
        flags: Flags,
    },
    Lock {
        kind: Option<LockKind>,
        duration_s: Option<i64>,
    },
    Unlock,
    Revert {
        target: Id<UserMarker>,
        since_s: i64,
    },
    Backup(BackupCmd),
    Ext(ModCmd, Option<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum BackupCmd {
    Create,
    List,
    Load { id: i64, confirm: bool },
    Delete { id: i64 },
}

pub(crate) fn user(o: &[CommandDataOption], name: &str) -> Option<Id<UserMarker>> {
    o.iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            CommandOptionValue::User(u) => Some(u),
            _ => None,
        })
}

pub(crate) fn string<'a>(o: &'a [CommandDataOption], name: &str) -> Option<&'a str> {
    o.iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandOptionValue::String(s) => Some(s.as_str()),
            _ => None,
        })
}

pub(crate) fn int(o: &[CommandDataOption], name: &str) -> Option<i64> {
    o.iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            CommandOptionValue::Integer(n) => Some(n),
            _ => None,
        })
}

fn flag(o: &[CommandDataOption], name: &str) -> bool {
    o.iter()
        .any(|o| o.name == name && o.value == CommandOptionValue::Boolean(true))
}

fn backup(o: &[CommandDataOption]) -> Result<Cmd, String> {
    let Some(CommandDataOption {
        name,
        value: CommandOptionValue::SubCommand(sub),
    }) = o.first()
    else {
        return Err("Pick a subcommand".into());
    };
    let id = || int(sub, "id").ok_or("missing id");
    Ok(Cmd::Backup(match name.as_str() {
        "create" => BackupCmd::Create,
        "list" => BackupCmd::List,
        "load" => BackupCmd::Load {
            id: id()?,
            confirm: flag(sub, "confirm"),
        },
        "delete" => BackupCmd::Delete { id: id()? },
        _ => return Err("Unknown subcommand".into()),
    }))
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
        "ban" | "Ban" | "softban" => ModKind::Ban,
        "kick" | "Kick" => ModKind::Kick,
        "timeout" | "Timeout" => ModKind::Timeout,
        "warn" => ModKind::Warn,
        "lock" => {
            let kind = match string(o, "type") {
                Some(n) => Some(kind_from(n, None).ok_or(commands::LOCK_TYPES)?),
                None => None,
            };
            return Ok(Cmd::Lock {
                kind,
                duration_s: dur(o, "duration")?,
            });
        }
        "unlock" => return Ok(Cmd::Unlock),
        "backup" => return backup(o),
        "Quarantine" | "Sanitize" => {
            let user = target_id.ok_or("missing user")?.get();
            let cmd = match name {
                "Quarantine" => ModCmd::Quarantine { user },
                _ => ModCmd::Sanitize { user },
            };
            return Ok(Cmd::Ext(cmd, None));
        }
        "unban" | "hackban" | "untimeout" | "unwarn" | "quarantine" | "unquarantine" | "purge"
        | "slowmode" | "cases" | "verify" | "sanitize" | "note" => {
            return modcmds::from_slash(name, o).map(|(c, r)| Cmd::Ext(c, r));
        }
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
        flags: Flags {
            soft: name == "softban",
            ..Default::default()
        },
    })
}

pub fn definitions() -> Vec<Command> {
    let user = || UserBuilder::new("user", "Target member").required(true);
    let reason = || StringBuilder::new("reason", "Reason for the case");
    let guild_only = |b: CommandBuilder| b.contexts([InteractionContextType::Guild]).build();
    let chat = |n: &str, d: &str| CommandBuilder::new(n, d, CommandType::ChatInput);
    let menu = |n: &str| CommandBuilder::new(n, "", CommandType::User);
    let mut defs = modcmds::definitions();
    defs.extend([
        guild_only(
            chat("ban", "Ban a member")
                .option(user())
                .option(reason())
                .option(StringBuilder::new("duration", "Length, e.g. 7d")),
        ),
        guild_only(
            chat(
                "softban",
                "Ban and unban a member to delete their recent messages",
            )
            .option(user())
            .option(reason()),
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
        guild_only(
            chat("lock", "Lock the server")
                .option(StringBuilder::new(
                    "type",
                    "server, blind, role, kick, ban or all",
                ))
                .option(StringBuilder::new("duration", "Unlock after, e.g. 30m")),
        ),
        guild_only(chat("unlock", "Unlock the server")),
        guild_only(
            chat("revert", "Undo a member's recent logged actions")
                .option(UserBuilder::new("actor", "Whose actions to undo").required(true))
                .option(StringBuilder::new("since", "How far back, e.g. 2h").required(true)),
        ),
        guild_only(
            chat("backup", "Server backups")
                .option(SubCommandBuilder::new("create", "Take a backup now"))
                .option(SubCommandBuilder::new("list", "List saved backups"))
                .option(
                    SubCommandBuilder::new("load", "Restore the server to a backup")
                        .option(IntegerBuilder::new("id", "Backup id").required(true))
                        .option(BooleanBuilder::new(
                            "confirm",
                            "Apply it (otherwise preview)",
                        )),
                )
                .option(
                    SubCommandBuilder::new("delete", "Delete a backup")
                        .option(IntegerBuilder::new("id", "Backup id").required(true)),
                ),
        ),
        guild_only(menu("Ban")),
        guild_only(menu("Kick")),
        guild_only(menu("Timeout")),
        guild_only(menu("Quarantine")),
        guild_only(menu("Sanitize")),
        guild_only(CommandBuilder::new("Report", "", CommandType::Message)),
        guild_only(CommandBuilder::new(
            "Purge from here",
            "",
            CommandType::Message,
        )),
    ]);
    defs
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
    let mut confirm = None;
    let text = match run(app, i, data, &mut confirm).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(?e, command = %data.name, "slash command failed");
            "Something went wrong, see logs".into()
        }
    };
    client
        .update_response(&i.token)
        .content(Some(&text))
        .components(confirm.as_ref().map(std::slice::from_ref))
        .allowed_mentions(Some(&AllowedMentions::default()))
        .await?;
    Ok(())
}

/// `confirm` receives the Confirm / Cancel row when the reply is a question instead of a result.
async fn run(
    app: &App,
    i: &Interaction,
    data: &twilight_model::application::interaction::application_command::CommandData,
    confirm: &mut Option<Component>,
) -> Result<String> {
    let (Some(gid), Some(actor_id)) = (i.guild_id, i.author_id()) else {
        return Ok("Server only".into());
    };
    if data.kind == CommandType::Message {
        return reports::message_menu(app, data, gid, actor_id).await;
    }
    let cmd = match parse(&data.name, &data.options, data.target_id) {
        Ok(c) => c,
        Err(e) => return Ok(e),
    };
    let owner = app.guild_info(gid).await?.owner;
    let actor = app.level(gid, owner, actor_id).await?;
    if !app.cfg(gid.get()).await?.misc.use_slash
        && !matches!(actor, permits::Level::Owner | permits::Level::ExtraOwner)
    {
        return Ok("Slash commands are disabled on this server".into());
    }
    let cfg_confirm = app.cfg(gid.get()).await?.misc.confirm;
    // Asks for a Confirm click when the server wants one; the buttons are handled in `commands`.
    let mut ask = async |action| -> Result<Option<String>> {
        let job = Pending::new(gid, actor_id, action);
        if !(cfg_confirm && job.needs_confirm()) {
            return Ok(None);
        }
        let (text, row) = commands::stash_confirm(app, i.id.get(), job).await?;
        *confirm = Some(row);
        Ok(Some(text))
    };
    match cmd {
        Cmd::Mod {
            kind,
            target,
            reason,
            duration_s,
            points,
            flags,
        } => {
            let action = Action::Mod {
                kind,
                targets: vec![target],
                reason: reason.clone(),
                duration_s,
                flags,
            };
            if let Some(q) = ask(action).await? {
                return Ok(q);
            }
            commands::apply_dm(
                app,
                gid,
                owner,
                (actor_id, &actor),
                kind,
                target,
                reason.as_deref(),
                duration_s,
                points,
                flags,
            )
            .await
        }
        Cmd::Lock { kind, duration_s } => {
            commands::lockdown_cmd(app, gid, &actor, false, kind, duration_s).await
        }
        Cmd::Unlock => commands::lockdown_cmd(app, gid, &actor, true, None, None).await,
        Cmd::Revert { target, since_s } => {
            commands::revert_cmd(app, gid, (actor_id, &actor), target, since_s).await
        }
        Cmd::Backup(b) => commands::backup_cmd(app, gid, (actor_id, &actor), b).await,
        Cmd::Ext(c, reason) => {
            let ch = i
                .channel
                .as_ref()
                .map(|c| c.id)
                .ok_or(anyhow::anyhow!("no channel"))?;
            let action = Action::Ext {
                cmd: c.clone(),
                channel: ch,
                reason: reason.clone(),
            };
            if let Some(q) = ask(action).await? {
                return Ok(q);
            }
            modcmds::run(
                app,
                gid,
                owner,
                (actor_id, &actor),
                ch,
                c,
                reason.as_deref(),
            )
            .await
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
                points: 1,
                flags: Flags::default()
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
                points: 1,
                flags: Flags::default()
            })
        );
        assert_eq!(
            parse("Quarantine", &[], Some(Id::new(9))),
            Ok(Cmd::Ext(ModCmd::Quarantine { user: 9 }, None))
        );
        assert!(parse("Sanitize", &[], None).is_err());
        assert!(matches!(
            parse("softban", &o[..1], None),
            Ok(Cmd::Mod {
                kind: ModKind::Ban,
                flags: Flags { soft: true, .. },
                ..
            })
        ));
        assert!(ModCmd::Purge {
            count: 5,
            user: None,
            from: None
        }
        .destructive());
        assert!(!ModCmd::Unban { user: 1 }.destructive());
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

    #[test]
    fn parses_backup_subcommands() {
        let sub = |name: &str, args: Vec<CommandDataOption>| {
            [opt(name, CommandOptionValue::SubCommand(args))]
        };
        assert_eq!(
            parse("backup", &sub("create", vec![]), None),
            Ok(Cmd::Backup(BackupCmd::Create))
        );
        assert_eq!(
            parse(
                "backup",
                &sub(
                    "load",
                    vec![
                        opt("id", CommandOptionValue::Integer(4)),
                        opt("confirm", CommandOptionValue::Boolean(true))
                    ]
                ),
                None
            ),
            Ok(Cmd::Backup(BackupCmd::Load {
                id: 4,
                confirm: true
            }))
        );
        assert_eq!(
            parse(
                "backup",
                &sub("load", vec![opt("id", CommandOptionValue::Integer(4))]),
                None
            ),
            Ok(Cmd::Backup(BackupCmd::Load {
                id: 4,
                confirm: false
            }))
        );
        assert!(parse("backup", &sub("delete", vec![]), None).is_err()); // id required
    }
}
