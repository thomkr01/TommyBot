//! The messages a ticket sends, built in one place so the dashboard previews show the same ones.
use super::*;

/// The DM asking the member to confirm the server before their first message goes out.
pub(crate) fn confirm(s: &Server, msg: Id<MessageMarker>) -> (Embed, Component) {
    let e = emb(
        &format!("{TICKET} Confirmation"),
        &format!(
            "You're sending this message to **{}** (ID: {}). Press **Send** to confirm.\nWant to send to another server? Press **Other server**.\nTo cancel this request, press **Cancel**.",
            s.name, s.id
        ),
        BLUE,
    );
    let e = footer(
        e,
        &format!(
            "Tip: You can disable confirmation messages with the {DM_PREFIX}confirmation command."
        ),
        None,
    );
    let row = action_row(vec![
        button(
            "Send",
            format!("tk:ok:{}:{msg}", s.id),
            ButtonStyle::Success,
        ),
        button(
            "Other server",
            format!("tk:other:{msg}"),
            ButtonStyle::Secondary,
        ),
        button("Cancel", "tk:no".into(), ButtonStyle::Danger),
    ]);
    (e.build(), row)
}

/// The member's message: (copy in their DMs, what staff get in the ticket).
pub(crate) fn from_member(s: &Server, u: &User, text: &str) -> (EmbedBuilder, EmbedBuilder) {
    let title = format!("{TICKET_REPLY} Message Sent");
    let sent = s.footer(stamp(emb(&title, text, GREEN)));
    let title = format!("{TICKET_REPLY} Message Received");
    (sent, user_footer(stamp(emb(&title, text, GREEN)), u))
}

/// A staff reply: (what the member gets in DMs, the copy in the ticket).
pub(crate) fn from_staff(
    s: &Server,
    staff: &User,
    member: &User,
    text: &str,
    anon: bool,
) -> (EmbedBuilder, EmbedBuilder) {
    let mut to_member = s.footer(stamp(emb(
        &format!("{TICKET_REPLY} Message Received"),
        text,
        ORANGE,
    )));
    if !anon {
        to_member = author(to_member, &staff.name, avatar(staff));
    }
    let shown = if anon {
        format!("{} (Anonymous)", staff.name)
    } else {
        staff.name.clone()
    };
    let copy = author(
        stamp(emb(&format!("{TICKET_REPLY} Message Sent"), text, ORANGE)),
        &shown,
        avatar(staff),
    );
    (to_member, user_footer(copy, member))
}

/// The first message in a new ticket, telling staff how to reply.
pub(crate) fn intro(pfx: &str, command_only: bool, u: &User, roles: &[Id<RoleMarker>]) -> Embed {
    let how = if command_only {
        format!("Type `{pfx}reply <message>` in this channel to reply. All other messages are ignored, and can be used for staff discussion. Use the command `{pfx}close [reason]` to close this ticket. (Command only mode enabled)")
    } else {
        format!("Type a message in this channel to reply. Messages starting with the server prefix `{pfx}` are ignored, and can be used for staff discussion. Use the command `{pfx}close [reason]` to close this ticket.")
    };
    let role_list: String = roles.iter().map(|r| format!("<@&{r}> ")).collect();
    let role_list = match role_list.trim_end() {
        "" => "*None*".to_string(),
        l if l.len() <= 1024 => l.to_string(),
        _ => format!("*{} roles*", roles.len()),
    };
    let e = stamp(emb(&format!("{TICKET_NEW} New Ticket"), &how, BLUE))
        .field(EmbedFieldBuilder::new("User", format!("<@{}> ({})", u.id, u.id)).inline())
        .field(EmbedFieldBuilder::new("Roles", role_list).inline());
    user_footer(e, u).build()
}

/// The greeting or closing message the server set, sent to the member.
pub(crate) fn notice(s: &Server, closing: bool, text: &str, u: &User) -> Embed {
    let title = if closing {
        format!("{TICKET_CLOSE} Closing Message")
    } else {
        format!("{TICKET} Greeting Message")
    };
    s.footer(stamp(emb(&title, &tags(text, u), ORANGE))).build()
}

/// The DM telling the member their ticket was closed.
pub(crate) fn closed(s: &Server, reason: &str, staff: Option<&User>) -> Embed {
    let mut e = s.footer(stamp(emb(
        &format!("{TICKET_CLOSE} Ticket Closed"),
        reason,
        RED,
    )));
    if let Some(staff) = staff {
        e = author(e, &staff.name, avatar(staff));
    }
    e.build()
}
