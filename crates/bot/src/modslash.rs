//! Slash definitions of the commands in `modcmds`.
use twilight_model::application::{
    command::{Command, CommandType},
    interaction::InteractionContextType,
};
use twilight_util::builder::command::{
    CommandBuilder, IntegerBuilder, RoleBuilder, StringBuilder, SubCommandBuilder, UserBuilder,
};

pub fn definitions() -> Vec<Command> {
    let user = |d| UserBuilder::new("user", d).required(true);
    let reason = || StringBuilder::new("reason", "Reason for the case");
    let case = |req| {
        IntegerBuilder::new("case", "Case number")
            .min_value(1)
            .required(req)
    };
    let chat = |n, d| {
        CommandBuilder::new(n, d, CommandType::ChatInput).contexts([InteractionContextType::Guild])
    };
    vec![
        chat("unban", "Unban a user")
            .option(user("User to unban"))
            .option(reason())
            .build(),
        chat("hackban", "Ban someone by id, even if not in the server")
            .option(StringBuilder::new("user_id", "User id").required(true))
            .option(reason())
            .build(),
        chat("untimeout", "Remove a member's timeout")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("unwarn", "Deactivate a warn case")
            .option(case(true))
            .option(reason())
            .build(),
        chat("quarantine", "Strip a member's roles")
            .option(user("Member"))
            .option(reason())
            .build(),
        chat("unquarantine", "Restore a quarantined member's roles")
            .option(user("Member"))
            .option(reason())
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
                SubCommandBuilder::new("view", "Show a member's latest notes")
                    .option(user("Member")),
            )
            .option(
                SubCommandBuilder::new("remove", "Remove one note").option(
                    IntegerBuilder::new("id", "Note number")
                        .min_value(1)
                        .required(true),
                ),
            )
            .option(
                SubCommandBuilder::new("sweep", "Remove all notes of a member")
                    .option(user("Member")),
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
