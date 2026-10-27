//! Header settings search: finds any schema field or page, and the open page shows only its hits.
use crate::common::api;
use leptos::{html, prelude::*, task::spawn_local};
use serde_json::Value;

/// Search text and the field index, shared with the sidebar and `Section`.
#[derive(Clone, Copy)]
pub struct Search {
    pub q: RwSignal<String>,
    /// (page route, field); pages themselves are fields with only a title.
    pub index: RwSignal<Vec<(String, Value)>>,
}

/// Page route that shows `section`'s fields of dashboard `page` (see `App`'s router).
fn route(section: &str, page: Option<&str>) -> String {
    match (section, page) {
        ("automod", Some(p)) => format!("filters/{p}"),
        ("antinuke", Some(_)) => "antinuke_filters".into(),
        ("panic", Some("heat")) => "automod_panic".into(),
        ("panic", _) => "antinuke_panic".into(),
        ("logging", Some(p)) => format!("logging/{}", p.replacen('_', "/", 1)),
        ("whitelist", Some(p)) => format!("whitelist/{p}"),
        ("lockdown", _) => "lockdown_settings".into(),
        (s, _) => s.into(),
    }
}

/// Levenshtein distance of at most one.
fn one_typo(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if l.len() - s.len() > 1 { return false; }
    let i = s.iter().zip(&l).take_while(|(x, y)| x == y).count();
    // Past the first difference the rest must match after one substitution, insertion or deletion.
    s[i..] == l[i + 1..] || (s.len() == l.len() && s.get(i + 1..) == l.get(i + 1..))
}

/// How well `text` matches the lowercase query: exact > prefix > word start > substring > typo. 0: no match.
pub fn score(text: &str, q: &str) -> u32 {
    let t = text.to_lowercase();
    let words = || t.split(|c: char| !c.is_alphanumeric());
    if q.is_empty() { 0 }
    else if t == q { 100 }
    else if t.starts_with(q) { 80 }
    else if words().any(|w| w.starts_with(q)) { 60 }
    else if t.contains(q) { 40 }
    else if q.len() >= 4 && words().any(|w| one_typo(w, q)) { 20 }
    else { 0 }
}

/// A field's rank: its label counts most, then its card, then its help; several words must all hit.
pub fn field_score(f: &Value, q: &str) -> u32 {
    let q = q.trim().to_lowercase();
    let s = |k: &str| f[k].as_str().unwrap_or("");
    let title = if s("title").is_empty() { s("label") } else { s("title") };
    let best = score(title, &q).max(score(s("group"), &q) / 2).max(score(s("help"), &q) / 4);
    if best > 0 || !q.contains(' ') { return best; }
    let all = format!("{title} {} {}", s("group"), s("help"));
    if q.split_whitespace().all(|w| score(&all, w) > 0) { 30 } else { 0 }
}

/// Route -> number of hits, for the sidebar.
pub fn counts(s: Search) -> std::collections::HashMap<String, usize> {
    let q = s.q.get();
    let mut c = std::collections::HashMap::new();
    if q.trim().is_empty() { return c; }
    for (r, f) in s.index.get().iter().filter(|(_, f)| f["page"] != "nav") {
        if field_score(f, &q) > 0 { *c.entry(r.clone()).or_insert(0) += 1; }
    }
    c
}

/// The header box: `/` or Ctrl+K focuses it, Escape clears it.
#[component]
pub fn SearchBox(guild: Signal<Option<String>>, pages: Vec<(&'static str, &'static str)>) -> impl IntoView {
    let s = expect_context::<Search>();
    let open = RwSignal::new(false);
    let input = NodeRef::<html::Input>::new();
    window_event_listener(leptos::ev::keydown, move |e| {
        let typing = e.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(t).ok()).is_some_and(|el| matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT"));
        if (e.key() == "/" && !typing) || (e.key() == "k" && (e.ctrl_key() || e.meta_key())) {
            e.prevent_default();
            if let Some(i) = input.get() { let _ = i.focus(); }
        }
    });
    let load = move || {
        let Some(g) = guild.get_untracked() else { return };
        if !s.index.get_untracked().is_empty() { return; }
        let pages = pages.clone();
        spawn_local(async move {
            let mut idx: Vec<(String, Value)> = pages.iter().map(|(r, t)| (r.to_string(), serde_json::json!({"title": t, "group": "Page", "page": "nav"}))).collect();
            if let Ok(v) = api("GET", &format!("/api/guilds/{g}/schema"), None).await {
                for sec in v.as_array().into_iter().flatten() {
                    let name = sec["name"].as_str().unwrap_or("");
                    for f in sec["fields"].as_array().into_iter().flatten() {
                        let mut f = f.clone();
                        f["section_title"] = sec["title"].clone();
                        idx.push((route(name, f["page"].as_str()), f));
                    }
                }
            }
            s.index.set(idx);
        });
    };
    let hits = move || {
        let q = s.q.get();
        let mut v: Vec<(u32, String, Value)> = s.index.get().into_iter().filter_map(|(r, f)| Some((field_score(&f, &q), r, f)).filter(|h| h.0 > 0)).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.truncate(8);
        v
    };
    view! {
        <div class="search" on:mousedown=|e| if e.target() != e.current_target() { e.prevent_default() }>
            <input type="search" node_ref=input placeholder="Search settings ( / )" aria-label="search settings" prop:value=move || s.q.get()
                on:focus=move |_| { load(); open.set(true) } on:blur=move |_| open.set(false)
                on:input=move |e| { s.q.set(event_target_value(&e)); open.set(true) }
                on:keydown=move |e| if e.key() == "Escape" { s.q.set(String::new()); if let Some(i) = input.get() { let _ = i.blur(); } } />
            <Show when=move || open.get() && !s.q.get().trim().is_empty()>
                <div class="results">
                    {move || { let g = guild.get().unwrap_or_default(); let h = hits();
                        if h.is_empty() { return view! { <p class="muted">"No settings found"</p> }.into_any(); }
                        h.into_iter().map(|(_, r, f)| {
                            let t = f["title"].as_str().or(f["label"].as_str()).unwrap_or("").to_string();
                            let place = [f["section_title"].as_str(), f["group"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                            view! { <a href=format!("#/{g}/{r}") on:click=move |_| open.set(false)>{t}<small class="muted">{place}</small></a> }
                        }).collect_view().into_any() }}
                </div>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ranks_label_over_help_and_forgives_one_typo() {
        assert_eq!(score("Ban", "ban"), 100);
        assert!(score("Days purged on bans", "days") > score("Days purged on bans", "purged"));
        assert_eq!(score("Logging", "loggign"), 0); // two edits
        assert_eq!(score("Quarantine role", "quarantien"), 0);
        assert_eq!(score("Quarantine role", "quarntine"), 20);
        let f = json!({"title": "Catch channel", "group": "General", "help": "hacked accounts"});
        assert!(field_score(&f, "catch") > field_score(&f, "hacked"));
        assert!(field_score(&f, "hacked catch") > 0);
        assert_eq!(field_score(&f, "nothing"), 0);
    }

    #[test]
    fn routes_match_the_router() {
        assert_eq!(route("logging", Some("wick_essential")), "logging/wick/essential");
        assert_eq!(route("automod", Some("spam_text")), "filters/spam_text");
        assert_eq!(route("automod", None), "automod");
        assert_eq!(route("panic", None), "antinuke_panic");
    }
}
