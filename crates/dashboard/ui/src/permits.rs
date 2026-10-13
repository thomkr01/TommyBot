use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

/// Adds or removes `key` in `c.custom[i][field]` (perms / whitelist); keys the catalog doesn't list stay untouched.
fn toggle(c: &mut Value, i: usize, field: &str, key: &str, on: bool) {
    let list = &mut c["custom"][i][field];
    if !list.is_array() { *list = json!([]); }
    let a = list.as_array_mut().unwrap();
    a.retain(|x| x != key);
    if on { a.push(json!(key)); }
}

fn has(c: &Value, i: usize, field: &str, key: &str) -> bool {
    c["custom"][i][field].as_array().is_some_and(|a| a.iter().any(|x| x == key))
}

#[component]
pub fn Permits(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (cfg, set_cfg) = signal(json!({}));
    let catalog = RwSignal::new(json!([]));
    let version = RwSignal::new(0i64);
    let msg = RwSignal::new(String::new());
    let need = RwSignal::new(false);
    let open = RwSignal::new(None::<usize>); // permit being edited
    let tab = RwSignal::new(0u8);
    let confirm = RwSignal::new(false);
    let g = guild.clone();
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/config"), None).await {
            version.set(v["version"].as_i64().unwrap_or(0));
            set_cfg.set(v["config"]["permits"].clone());
        }
        if let Ok(v) = api("GET", "/api/permit-catalog", None).await { catalog.set(v); }
    });
    let save = move |_| {
        let (g, p, v) = (guild.clone(), cfg.get_untracked(), version.get_untracked());
        run_save(async move { api("PUT", &format!("/api/guilds/{g}/permits"), Some(json!({"expected_version": v, "value": p}))).await }, version, msg, need);
    };
    let set_ids = move |key: &'static str, val: String| set_cfg.update(|c| c[key] = from_csv(&val));
    let set_field = move |i: usize, field: &'static str, val: String| set_cfg.update(|c| {
        c["custom"][i][field] = match field { "rank" => json!(val.parse::<i64>().unwrap_or(0)), "name" => json!(val), _ => from_csv(&val) }
    });
    let editor = move |i: usize| view! {
        <div class="card">
            <div class="row">
                <button class=move || if tab.get() == 0 { "" } else { "ghost" } on:click=move |_| tab.set(0)>"General"</button>
                <button class=move || if tab.get() == 1 { "" } else { "ghost" } on:click=move |_| tab.set(1)>"Permissions"</button>
            </div>
            <Show when=move || tab.get() == 0>
                <div class="row"><label>"Name"</label><input prop:value=move || cfg.get()["custom"][i]["name"].as_str().unwrap_or("").to_string() on:change=move |e| set_field(i, "name", event_target_value(&e)) /></div>
                <div class="row"><label>"Rank"</label><input prop:value=move || cfg.get()["custom"][i]["rank"].to_string() on:change=move |e| set_field(i, "rank", event_target_value(&e)) /></div>
                <div class="row"><label>"Role IDs"</label><input prop:value=move || csv(&cfg.get()["custom"][i]["roles"]) on:change=move |e| set_field(i, "roles", event_target_value(&e)) /></div>
                <p class="muted">"Members with any of these roles get this permit. A lower rank number outranks a higher one."</p>
            </Show>
            <Show when=move || tab.get() == 1>
                <For each=move || catalog.get().as_array().cloned().unwrap_or_default() key=|s| s["id"].as_str().unwrap_or("").to_string() let:s>
                    <h4>{s["title"].as_str().unwrap_or("").to_string()}</h4>
                    {s["items"].as_array().cloned().unwrap_or_default().into_iter().map(|it| {
                        let key = it["key"].as_str().unwrap_or("").to_string();
                        let field = if it["flag"].as_bool().unwrap_or(false) { "whitelist" } else { "perms" };
                        let locked = it["locked"].as_bool().unwrap_or(false);
                        let k = key.clone();
                        view! {
                            <div class="row">
                                <input type="checkbox" class="sw" disabled=locked
                                    prop:checked=move || has(&cfg.get(), i, field, &k)
                                    on:change=move |e| { let on = event_target_checked(&e); let k = key.clone(); set_cfg.update(|c| toggle(c, i, field, &k, on)) } />
                                <label>{it["label"].as_str().unwrap_or("").to_string()}</label>
                                <span class="muted">{it["help"].as_str().unwrap_or("").to_string()}</span>
                            </div>
                        }
                    }).collect_view()}
                </For>
            </Show>
            <div class="row">
                <Show when=move || confirm.get() fallback=move || view! { <button class="ghost" on:click=move |_| confirm.set(true)>"Delete"</button> }>
                    <button on:click=move |_| { set_cfg.update(|c| { c["custom"].as_array_mut().map(|a| a.remove(i)); }); open.set(None); confirm.set(false); }>"Confirm delete"</button>
                    <button class="ghost" on:click=move |_| confirm.set(false)>"Abort"</button>
                </Show>
                <button on:click=move |_| { open.set(None); confirm.set(false); }>"Apply"</button>
            </div>
        </div>
    };
    view! {
        <div class="card">
            <div class="row"><label>"Extra owners"</label><input prop:value=move || csv(&cfg.get()["eo"]) on:change=move |e| set_ids("eo", event_target_value(&e)) /></div>
            <div class="row"><label>"Trusted admins"</label><input prop:value=move || csv(&cfg.get()["ta"]) on:change=move |e| set_ids("ta", event_target_value(&e)) /></div>
            <p class="muted">"Discord user IDs, comma-separated. Only owners can change these two lists."</p>
        </div>
        {move || (0..cfg.get()["custom"].as_array().map_or(0, Vec::len)).map(|i| view! {
            <div class="card row">
                <b>{cfg.get_untracked()["custom"][i]["name"].as_str().unwrap_or("").to_string()}</b>
                <span class="muted">{format!("rank {}", cfg.get_untracked()["custom"][i]["rank"])}</span>
                <button class="ghost" on:click=move |_| { tab.set(0); confirm.set(false); open.set(Some(i)); }>"Edit"</button>
            </div>
            {move || (open.get() == Some(i)).then(|| editor(i))}
        }).collect_view()}
        <div class="row">
            <button class="ghost" on:click=move |_| set_cfg.update(|c| { if !c["custom"].is_array() { c["custom"] = json!([]); } c["custom"].as_array_mut().unwrap().push(json!({"name": "New permit", "rank": 0, "perms": [], "roles": [], "whitelist": [], "limits": {}})); })>"+ Add permit"</button>
            <button on:click=save>"Save"</button>
            <span>{move || msg.get()}</span>
        </div>
        <StepUp need enrolled />
    }
}
