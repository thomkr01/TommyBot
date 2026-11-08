//! Public Commands page: every bot command and message variable, as the bot published them at startup.
use crate::common::{api, Head};
use leptos::{prelude::*, task::spawn_local};
use serde_json::Value;

fn s(v: &Value) -> String { v.as_str().unwrap_or("").to_string() }

/// Whether command `c` matches the lowercase filter `q` by name, alias, description or usage.
fn hit(c: &Value, q: &str) -> bool {
    let all = format!("{} {} {} {}", s(&c["name"]), c["aliases"], s(&c["what"]), s(&c["usage"])).to_lowercase();
    q.split_whitespace().all(|w| all.contains(w))
}

#[component]
pub fn Commands() -> impl IntoView {
    let data = RwSignal::new(Value::Null);
    let (q, set_q) = signal(String::new());
    spawn_local(async move { if let Ok(v) = api("GET", "/api/commands", None).await { data.set(v) } });
    let list = move || {
        let d = data.get();
        let pfx = s(&d["prefix"]);
        let q = q.get().to_lowercase();
        let cmds: Vec<Value> = d["commands"].as_array().into_iter().flatten().filter(|c| hit(c, &q)).cloned().collect();
        let mut cats: Vec<String> = vec![];
        for c in &cmds { let k = s(&c["category"]); if !cats.contains(&k) { cats.push(k) } }
        if d["commands"].is_null() { return view! { <p class="muted">"The bot has not published its commands yet: start it once."</p> }.into_any() }
        if cmds.is_empty() { return view! { <p class="muted">"No command matches."</p> }.into_any() }
        cats.into_iter().map(|cat| {
            let rows = cmds.iter().filter(|c| s(&c["category"]) == cat).map(|c| {
                let p = pfx.clone();
                let line = |label: &'static str, v: String| (!v.is_empty()).then(|| view! { <p><b>{label}</b>" "<code>{v}</code></p> });
                let aliases = c["aliases"].as_array().map(|a| a.iter().map(s).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                let slash = c["slash"].as_array().map(|a| a.iter().map(s).collect::<Vec<_>>().join("  ·  ")).unwrap_or_default();
                view! {
                    <details class="card">
                        <summary><code>{format!("{p}{}", s(&c["name"]))}</code><span>{s(&c["what"])}</span><span class="chip">{s(&c["who"])}</span></summary>
                        {line("Usage", format!("{p}{}", s(&c["usage"])))}
                        <ul>{c["args"].as_array().into_iter().flatten().map(|a| view! { <li><code>{s(&a[0])}</code>" "{s(&a[1])}</li> }).collect_view()}</ul>
                        {line("Example", format!("{p}{}", s(&c["example"])))}
                        {(!aliases.is_empty()).then(|| view! { <p><b>"Aliases"</b>" "{aliases}</p> })}
                        {line("Slash", slash)}
                    </details>
                }
            }).collect_view();
            view! { <h3>{cat}</h3>{rows} }
        }).collect_view().into_any()
    };
    let apps = move || data.get()["apps"].as_array().cloned().unwrap_or_default().into_iter().map(|a| view! { <li><code>{s(&a["name"])}</code>" "{s(&a["what"])}</li> }).collect_view();
    let vars = move || data.get()["variables"].as_array().cloned().unwrap_or_default().into_iter().map(|v| view! { <li><code>{s(&v[0])}</code>" "{s(&v[1])}</li> }).collect_view();
    view! {
        <Head guild=String::new() page="TommyBot".into() big="Commands".into() />
        <div class="stack cmds">
            <input placeholder="Filter: [name, alias or word]" aria-label="filter commands" prop:value=q on:input=move |e| set_q.set(event_target_value(&e)) />
            <p class="muted">{move || format!("Default prefix {}. Add -h after any command for its help in Discord.", s(&data.get()["prefix"]))}</p>
            {list}
            <h3>"Slash-only commands and apps"</h3>
            <div class="card"><ul>{apps}</ul></div>
            <h3>"Message variables"</h3>
            <div class="card"><p class="muted">{move || format!("{}:", s(&data.get()["variables_in"]))}</p><ul>{vars}</ul></div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn filter_matches_every_word() {
        let c = serde_json::json!({"name": "ban", "aliases": ["b"], "what": "Ban, softban with ?s", "usage": "ban MEMBERS"});
        assert!(super::hit(&c, "ban soft"));
        assert!(!super::hit(&c, "kick"));
    }
}
