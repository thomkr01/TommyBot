//! What a message carries besides its content: forwarded attachments, and the embed and component
//! text of webhook and app posts.
use twilight_model::channel::{
    message::{component::Component, Embed},
    Attachment, Message,
};

/// The message's own attachments and those of what it forwards.
pub(crate) fn attachments(m: &Message) -> impl Iterator<Item = &Attachment> {
    m.attachments.iter().chain(
        m.message_snapshots
            .iter()
            .flat_map(|s| &s.message.attachments),
    )
}

/// Embed and component text of a webhook or app post. Discord's previews of links in the content
/// are left out: the content already holds those links, and the page's text is not the poster's.
/// A poster's own embeds are always `rich`, so one cannot pass as a preview by repeating its URL.
pub(super) fn app_text(m: &Message) -> String {
    if m.webhook_id.is_none() && m.application_id.is_none() {
        return String::new();
    }
    let preview =
        |e: &&Embed| e.kind != "rich" && e.url.as_deref().is_some_and(|u| m.content.contains(u));
    let mut out = vec![];
    for e in m.embeds.iter().filter(|e| !preview(e)) {
        out.extend(
            [&e.title, &e.description, &e.url]
                .into_iter()
                .flatten()
                .map(String::as_str),
        );
        for f in &e.fields {
            out.extend([f.name.as_str(), f.value.as_str()]);
        }
        out.extend(e.footer.as_ref().map(|f| f.text.as_str()));
        if let Some(a) = &e.author {
            out.extend(std::iter::once(a.name.as_str()).chain(a.url.as_deref()));
        }
    }
    component_text(&m.components, &mut out);
    out.join("\n")
}

/// Link-button URLs and text blocks, also inside rows, sections and containers.
fn component_text<'a>(cs: &'a [Component], out: &mut Vec<&'a str>) {
    for c in cs {
        match c {
            Component::ActionRow(r) => component_text(&r.components, out),
            Component::Container(c) => component_text(&c.components, out),
            Component::Section(s) => {
                component_text(&s.components, out);
                component_text(std::slice::from_ref(&s.accessory), out);
            }
            Component::Button(b) => out.extend(b.url.as_deref()),
            Component::TextDisplay(t) => out.push(&t.content),
            _ => {}
        }
    }
}
