//! Discord look-alike messages for the wizard previews and the Embed Builder. The bot's own
//! messages come from `assets/previews/messages.json`, which a bot test builds with the same code
//! that sends them, so the previews match Discord.
use crate::md::{esc, inline, md};
use leptos::prelude::*;
use serde_json::Value;
use std::sync::OnceLock;

fn samples() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| serde_json::from_str(include_str!("../../../../assets/previews/messages.json")).unwrap_or_default())
}

/// One of the bot's sample messages.
pub fn sample(key: &str) -> &'static Value {
    &samples()[key]
}

/// The name behind a sample id: `kind` is users, channels or roles.
pub fn name(kind: &str, id: &str) -> Option<String> {
    samples()["names"][kind][id].as_str().map(String::from)
}

/// A `<t:unix:style>` timestamp as Discord shows it; 0 is now.
#[cfg(target_arch = "wasm32")]
pub fn time(secs: i64, style: &str) -> String {
    let d = if secs == 0 { js_sys::Date::new_0() } else { js_sys::Date::new(&(secs as f64 * 1000.0).into()) };
    let o = js_sys::Object::new();
    let set = |k: &str, v: &str| { let _ = js_sys::Reflect::set(&o, &k.into(), &v.into()); };
    match style {
        "t" | "T" => {}
        "d" => { set("year", "numeric"); set("month", "2-digit"); set("day", "2-digit"); }
        "F" => { set("weekday", "long"); set("year", "numeric"); set("month", "long"); set("day", "numeric"); }
        _ => { set("year", "numeric"); set("month", "long"); set("day", "numeric"); }
    }
    if !matches!(style, "d" | "D") { set("hour", "numeric"); set("minute", "2-digit"); }
    if style == "T" { set("second", "2-digit"); }
    String::from(d.to_locale_string("en-US", &o)).replace(" at ", " ")
}

/// Native unit tests have no browser clock.
#[cfg(not(target_arch = "wasm32"))]
pub fn time(secs: i64, style: &str) -> String {
    format!("{secs}:{style}")
}

/// "Today at 9:11 PM", the time beside a message's author.
fn now() -> String {
    format!("Today at {}", time(0, "t"))
}

/// A member's message: grey avatar, name, text.
pub fn chat(author: &str, body: AnyView) -> AnyView {
    let first = author.chars().next().unwrap_or('?').to_uppercase().to_string();
    view! { <div class="dmsg"><i class="user">{first}</i><div><b>{author.to_string()}</b>" "<small class="muted">{now()}</small><div class="blk">{body}</div></div></div> }.into_any()
}

fn button(b: &Value) -> AnyView {
    let style = match b["style"].as_u64() { Some(1) => "primary", Some(3) => "success", Some(4) => "danger", _ => "secondary" };
    let link = b["style"] == 5;
    view! { <span class=format!("dbtn blk {style}")>{b["label"].as_str().unwrap_or("").to_string()}{link.then_some(" ↗")}</span> }.into_any()
}

fn embed(e: &Value) -> AnyView {
    let s = |p: &str| p.split('.').fold(e, |v, k| &v[k]).as_str().unwrap_or("").to_string();
    // Discord's bar for an embed without a colour.
    let color = e["color"].as_u64().map_or("#1e1f22".into(), |c| format!("#{c:06x}"));
    let image = s("image.url").replace("attachment://", "/");
    let footer = [s("footer.text"), if s("timestamp").is_empty() { String::new() } else { now() }].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" • ");
    view! {
        <div class="embed" style=format!("border-left-color:{color}")>
            {(!s("thumbnail.url").is_empty()).then(|| view! { <img class="thumb" src=s("thumbnail.url") alt="" /> })}
            {(!s("author.name").is_empty()).then(|| view! { <small class="author blk">
                {(!s("author.icon_url").is_empty()).then(|| view! { <img class="ico" src=s("author.icon_url") alt="" /> })}
                <b>{s("author.name")}</b></small> })}
            {(!s("title").is_empty()).then(|| view! { <b class="blk" class:link=!s("url").is_empty() inner_html=inline(&esc(&s("title"))) /> })}
            {(!s("description").is_empty()).then(|| view! { <div class="md" inner_html=md(&s("description")) /> })}
            {e["fields"].as_array().filter(|f| !f.is_empty()).map(|fs| view! { <div class="fields">{fs.iter().map(|f| view! {
                <div class="blk" class:wide=f["inline"] != true><b inner_html=inline(&esc(f["name"].as_str().unwrap_or(""))) /><div class="md" inner_html=md(f["value"].as_str().unwrap_or("")) /></div> }).collect_view()}</div> })}
            {(!image.is_empty()).then(|| view! { <img src=image alt="" /> })}
            {(!footer.is_empty()).then(|| view! { <small class="muted author blk">
                {(!s("footer.icon_url").is_empty()).then(|| view! { <img class="ico" src=s("footer.icon_url") alt="" /> })}{footer}</small> })}
        </div>
    }.into_any()
}

/// A message the bot sends: content, embeds, button rows; flag 64 marks it "only you can see this".
pub fn bot(m: &Value) -> AnyView {
    let content = m["content"].as_str().unwrap_or("");
    let rows = m["components"].as_array().cloned().unwrap_or_default();
    view! {
        <div class="dmsg">
            <img class="avatar" src="/logo.svg" alt="" />
            <div>
                <b>"TommyBot "</b><span class="chip">"APP"</span>" "<small class="muted">{now()}</small>
                {(!content.is_empty()).then(|| view! { <div class="md" inner_html=md(content) /> })}
                {m["embeds"].as_array().into_iter().flatten().map(embed).collect_view()}
                {rows.iter().map(|r| view! { <div class="drow">{r["components"].as_array().into_iter().flatten().map(button).collect_view()}</div> }).collect_view()}
                {(m["flags"] == 64).then(|| view! { <small class="muted eph">"Only you can see this • "<span style="color:#00a8fc">"Dismiss message"</span></small> })}
            </div>
        </div>
    }.into_any()
}

/// A sample by key, as the bot sends it.
pub fn show(key: &str) -> AnyView {
    bot(sample(key))
}

/// Marks the preview's lines whose text is new since the last call, so a settings change shows
/// where it landed; `seen` holds the last texts and starts empty, so the first render stays still.
pub fn flash_changes(root: &web_sys::Element, seen: &mut Option<Vec<String>>) {
    let Ok(list) = root.query_selector_all(".blk, .md > div, .md > pre, p, .chip") else { return };
    let els: Vec<web_sys::Element> = (0..list.length()).filter_map(|i| wasm_bindgen::JsCast::dyn_into(list.item(i)?).ok()).collect();
    let texts: Vec<String> = els.iter().map(|e| e.text_content().unwrap_or_default()).collect();
    if let Some(old) = seen.as_ref() {
        for (e, t) in els.iter().zip(&texts) {
            if !old.contains(t) { e.set_class_name(&format!("{} chg", e.class_name())); }
        }
    }
    *seen = Some(texts);
}
