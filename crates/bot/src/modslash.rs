//! Slash definitions of the commands in `modcmds`.
use twilight_model::application::{
    command::{Command, CommandType},
    interaction::{
        application_command::{CommandDataOption, CommandOptionValue},
        InteractionContextType,
    },
};
use twilight_util::builder::command::{
    AttachmentBuilder, BooleanBuilder, CommandBuilder, IntegerBuilder, RoleBuilder, StringBuilder,
    SubCommandBuilder, UserBuilder,
};

use crate::{
    modcmds::{ModCmd, MAX_SLOWMODE, SMALL},
    purge,
    slash::{int, role, string, user as user_opt},
};

pub fn definitions() -> Vec<Command> {
    let user = |d| UserBuilder::new("user", d).required(true);
    let reason = || StringBuilder::new("reason", "Reason for the case");
    let case = |req| {
        IntegerBuilder::new("case", "Case number")
            .min_value(1)
            .required(req)
    };
    let note_id = |req| {
        IntegerBuilder::new("note_id", "Note number")
            .min_value(1)
            .required(req)
    };
    let chat = |n, d| {
        CommandBuilder::new(n, d, CommandType::ChatInput).contexts([InteractionContextType::Guild])
    };
    vec![
        chat("hackban", "Ban someone by id, even if not in the server")
            .option(StringBuilder::new("user_id", "User id").required(true))
            .option(reason())
            .build(),
        chat("unwarn", "Deactivate a warn case")
            .option(case(true))
            .option(reason())
            .build(),
        chat("quarantine", "Strip a member's roles or give them back")
            .option(
                SubCommandBuilder::new("add", "Strip a member's roles")
                    .option(user("Member"))
                    .option(reason())
                    .option(BooleanBuilder::new("dm", "Tell the member by DM"))
                    .option(AttachmentBuilder::new(
                        "proof",
                        "Screenshot or file as evidence",
                    )),
            )
            .option(
                SubCommandBuilder::new("remove", "Restore a quarantined member's roles")
                    .option(user("Member"))
                    .option(reason())
                    .option(BooleanBuilder::new("dm", "Tell the member by DM"))
                    .option(AttachmentBuilder::new(
                        "proof",
                        "Screenshot or file as evidence",
                    )),
            )
            .build(),
        chat("verify", "Manually verify a member")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("sanitize", "Clean up a member's hoisted or unreadable name")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("purge", "Bulk delete recent messages here")
            .option(
                IntegerBuilder::new("number", "How many (1-1000)")
                    .min_value(1)
                    .max_value(1000)
                    .required(true),
            )
            .option(
                StringBuilder::new("type", "Only messages of this type").choices(
                    [
                        "embeds",
                        "suspicious",
                        "bots",
                        "mentions",
                        "reactions",
                        "emojis",
                        "text",
                        "noavatar",
                        "norole",
                        "attachments",
                        "links",
                        "after",
                        "before",
                        "equals",
                        "has",
                        "starts",
                        "webhook",
                    ]
                    .map(|k| (k, k)),
                ),
            )
            .option(StringBuilder::new(
                "input",
                "Text or id for has/equals/starts/after/before/webhook",
            ))
            .option(UserBuilder::new("user", "Only this user's messages"))
            .option(RoleBuilder::new(
                "role",
                "Only messages of members with this role",
            ))
            .option(reason())
            .build(),
        chat("slowmode", "Set this channel's slowmode")
            .option(
                IntegerBuilder::new("seconds", "0 turns it off")
                    .min_value(0)
                    .max_value(21_600)
                    .required(true),
            )
            .option(reason())
            .build(),
        chat("notes", "Staff notes about members")
            .option(
                SubCommandBuilder::new("add", "Add a note")
                    .option(user("Member"))
                    .option(
                        StringBuilder::new("msg", "The note (5 to 500 characters)")
                            .min_length(5)
                            .max_length(500)
                            .required(true),
                    ),
            )
            .option(
                SubCommandBuilder::new("view", "Show the latest notes")
                    .option(note_id(false))
                    .option(UserBuilder::new("user", "Notes about this member"))
                    .option(UserBuilder::new("mod", "Notes written by this moderator")),
            )
            .option(
                SubCommandBuilder::new("modify", "Change a note")
                    .option(note_id(true))
                    .option(
                        StringBuilder::new("msg", "The new text (5 to 500 characters)")
                            .min_length(5)
                            .max_length(500)
                            .required(true),
                    ),
            )
            .option(
                SubCommandBuilder::new("remove", "Remove one note")
                    .option(note_id(true))
                    .option(UserBuilder::new(
                        "user",
                        "Only if the note is about this member",
                    )),
            )
            .option(
                SubCommandBuilder::new("sweep", "Remove notes in bulk, all without a filter")
                    .option(UserBuilder::new("user", "Notes about this member"))
                    .option(UserBuilder::new("mod", "Notes written by this moderator")),
            )
            .build(),
        chat("cases", "View or edit cases")
            .option(
                SubCommandBuilder::new("view", "Show a case, or the latest cases of a user")
                    .option(case(false))
                    .option(UserBuilder::new("user", "Member"))
                    .option(UserBuilder::new("mod", "Moderator who made the cases"))
                    .option(
                        StringBuilder::new("type", "Case type").choices(
                            [
                                "ban",
                                "unban",
                                "kick",
                                "timeout",
                                "warn",
                                "quarantine",
                                "unquarantine",
                            ]
                            .map(|k| (k, k)),
                        ),
                    ),
            )
            .option(
                SubCommandBuilder::new("modify", "Change a case's reason")
                    .option(case(true))
                    .option(StringBuilder::new("reason", "New reason").required(true)),
            )
            .build(),
    ]
}

/// Slash options -> command plus the optional `reason`.
pub fn from_slash(name: &str, o: &[CommandDataOption]) -> Result<(ModCmd, Option<String>), String> {
    let u = || user_opt(o, "user").map(|u| u.get()).ok_or("missing user");
    let case = |o| int(o, "case").map(|n| n as i32).ok_or("missing case");
    let cmd = match name {
        "unban" => ModCmd::Unban { user: u()? },
        "hackban" => ModCmd::Hackban {
            user: string(o, "user_id")
                .and_then(|s| s.trim().parse().ok())
                .filter(|id| *id >= SMALL)
                .ok_or("`user_id` must be a numeric user id")?,
        },
        "untimeout" => ModCmd::Untimeout { user: u()? },
        "unwarn" => ModCmd::Unwarn { case: case(o)? },
        "quarantine" => ModCmd::Quarantine { user: u()? },
        "unquarantine" => ModCmd::Unquarantine { user: u()? },
        "verify" => ModCmd::Verify { user: u()? },
        "sanitize" => ModCmd::Sanitize { user: u()? },
        "purge" => ModCmd::Purge {
            count: int(o, "number")
                .ok_or("missing number")?
                .clamp(1, purge::MAX as i64) as usize,
            user: user_opt(o, "user").map(|u| u.get()),
            from: None,
            filters: string(o, "type")
                .map(|t| purge::Filter::parse(t, string(o, "input")))
                .transpose()?
                .into_iter()
                .chain(role(o, "role").map(purge::Filter::Role))
                .collect(),
        },
        "slowmode" => ModCmd::Slowmode {
            secs: int(o, "seconds")
                .ok_or("missing seconds")?
                .clamp(0, MAX_SLOWMODE as i64) as u16,
            channels: vec![],
        },
        "notes" => {
            let Some(CommandDataOption {
                name,
                value: CommandOptionValue::SubCommand(s),
            }) = o.first()
            else {
                return Err("Pick a subcommand".into());
            };
            let u = || user_opt(s, "user").map(|u| u.get()).ok_or("missing user");
            match name.as_str() {
                "add" => ModCmd::NoteAdd {
                    user: u()?,
                    text: string(s, "msg").ok_or("missing msg")?.into(),
                },
                "view" => ModCmd::NoteList {
                    user: user_opt(s, "user").map(|u| u.get()),
                    id: int(s, "note_id"),
                    by: user_opt(s, "mod").map(|u| u.get()),
                },
                "remove" => ModCmd::NoteRemove {
                    id: int(s, "note_id").ok_or("missing note_id")?,
                    user: user_opt(s, "user").map(|u| u.get()),
                },
                "modify" => ModCmd::NoteModify {
                    id: int(s, "note_id").ok_or("missing note_id")?,
                    text: string(s, "msg").ok_or("missing msg")?.into(),
                },
                "sweep" => ModCmd::NoteSweep {
                    user: user_opt(s, "user").map(|u| u.get()),
                    by: user_opt(s, "mod").map(|u| u.get()),
                },
                _ => return Err("Unknown subcommand".into()),
            }
        }
        "cases" => {
            let Some(CommandDataOption {
                name,
                value: CommandOptionValue::SubCommand(s),
            }) = o.first()
            else {
                return Err("Pick a subcommand".into());
            };
            match name.as_str() {
                "view" => ModCmd::CasesView {
                    case: case(s).ok(),
                    user: user_opt(s, "user").map(|u| u.get()),
                    by: user_opt(s, "mod").map(|u| u.get()),
                    kind: string(s, "type").map(String::from),
                },
                "modify" => ModCmd::CasesEdit {
                    case: case(s)?,
                    reason: string(s, "reason").ok_or("missing reason")?.into(),
                },
                _ => return Err("Unknown subcommand".into()),
            }
        }
        _ => return Err("Unknown command".into()),
    };
    Ok((cmd, string(o, "reason").map(String::from)))
}
