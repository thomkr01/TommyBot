use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

mod common;
mod form;
mod pages;
use common::*;
use form::Section;
use pages::{Rescue, Revert, Score, Wizard};

/// Sidebar groups, mirroring Wick; only features TommyBot has. Untyped sections stay reachable under "Raw JSON".
const NAV: &[(&str, &[(&str, &str)])] = &[
    ("Auto Mod", &[("automod", "General"), ("filters", "Filters"), ("whitelist", "Whitelist")]),
    ("Anti Nuke", &[("antinuke", "General"), ("panic", "Panic Mode")]),
    ("Server Joins", &[("joingate", "Join Gate"), ("joinraid", "Join Raid")]),
    ("Tools", &[("revert", "Revert Center"), ("history", "History"), ("rescue", "Rescue Key")]),
    ("Raw JSON", &[("backups", "Backups"), ("discord_automod", "Discord Automod"), ("condition_automod", "Condition Automod"), ("verification", "Verification"), ("moderation", "Moderation"), ("warn_points", "Warn Points"), ("punish", "Punish"), ("appeals", "Appeals"), ("utility", "Utility")]),
];
const TOP: &[(&str, &str)] = &[("wizard", "Wizard"), ("overview", "Overview"), ("misc", "Miscellaneous"), ("permits", "Permits"), ("logging", "Logging"), ("lockdown", "Lockdown")];

fn title(p: &str) -> String {
    let hit = NAV.iter().flat_map(|(g, i)| i.iter().map(move |(k, t)| (*k, *t, *g))).chain(TOP.iter().map(|(k, t)| (*k, *t, ""))).find(|(k, ..)| *k == p);
    match hit {
        Some((_, t, g)) if matches!(g, "Auto Mod" | "Anti Nuke") => format!("{g} {t}"),
        Some((_, t, _)) => t.to_string(),
        None => p.replace('_', " "),
    }
}

fn hash() -> Vec<String> {
    let h = web_sys::window().unwrap().location().hash().unwrap_or_default();
    h.trim_start_matches('#').split('/').filter(|s| !s.is_empty()).map(String::from).collect()
}

#[component]
fn Permits(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (cfg, set_cfg) = signal(json!({}));
    let version = RwSignal::new(0i64);
    let msg = RwSignal::new(String::new());
    let need = RwSignal::new(false);
    let g = guild.clone();
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/config"), None).await {
            version.set(v["version"].as_i64().unwrap_or(0));
            set_cfg.set(v["config"]["permits"].clone());
        }
    });
    let save = move |_| {
        let (g, p, v) = (guild.clone(), cfg.get_untracked(), version.get_untracked());
        run_save(async move { api("PUT", &format!("/api/guilds/{g}/permits"), Some(json!({"expected_version": v, "value": p}))).await }, version, msg, need);
    };
    let edit = move |key: &'static str, i: Option<usize>, field: &'static str, val: String| {
        set_cfg.update(|c| match i {
            None => c[key] = from_csv(&val),
            Some(i) => c[key][i][field] = match field { "rank" => json!(val.parse::<i64>().unwrap_or(0)), "name" => json!(val), _ => from_csv(&val) },
        })
    };
    view! {
        <div class="card">
            <div class="row"><label>"Owners (EO)"</label><input prop:value=move || csv(&cfg.get()["eo"]) on:change=move |e| edit("eo", None, "", event_target_value(&e)) /></div>
            <div class="row"><label>"Trusted admins"</label><input prop:value=move || csv(&cfg.get()["ta"]) on:change=move |e| edit("ta", None, "", event_target_value(&e)) /></div>
            <p class="muted">"Comma-separated Discord user IDs. Only owners can change these two lists."</p>
        </div>
        <For each=move || 0..cfg.get()["custom"].as_array().map_or(0, Vec::len) key=|i| *i let:i>
            <div class="card">
                <div class="row"><label>"Name"</label><input prop:value=move || cfg.get()["custom"][i]["name"].as_str().unwrap_or("").to_string() on:change=move |e| edit("custom", Some(i), "name", event_target_value(&e)) /></div>
                <div class="row"><label>"Rank"</label><input prop:value=move || cfg.get()["custom"][i]["rank"].to_string() on:change=move |e| edit("custom", Some(i), "rank", event_target_value(&e)) /></div>
                <div class="row"><label>"Role IDs"</label><input prop:value=move || csv(&cfg.get()["custom"][i]["roles"]) on:change=move |e| edit("custom", Some(i), "roles", event_target_value(&e)) /></div>
                <div class="row"><label>"Permissions"</label><input prop:value=move || csv(&cfg.get()["custom"][i]["perms"]) on:change=move |e| edit("custom", Some(i), "perms", event_target_value(&e)) /></div>
                <div class="row"><label>"Whitelist"</label><input prop:value=move || csv(&cfg.get()["custom"][i]["whitelist"]) on:change=move |e| edit("custom", Some(i), "whitelist", event_target_value(&e)) /></div>
                <button class="ghost" on:click=move |_| set_cfg.update(|c| { c["custom"].as_array_mut().map(|a| a.remove(i)); })>"Remove"</button>
            </div>
        </For>
        <div class="row">
            <button class="ghost" on:click=move |_| set_cfg.update(|c| { if !c["custom"].is_array() { c["custom"] = json!([]); } c["custom"].as_array_mut().unwrap().push(json!({"name": "New permit", "rank": 0, "perms": [], "roles": [], "whitelist": [], "limits": {}})); })>"Add permit"</button>
            <button on:click=save>"Save"</button>
            <span>{move || msg.get()}</span>
        </div>
        <StepUp need enrolled />
    }
}

#[component]
fn History(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (diff, set_diff) = signal(String::new());
    let version = RwSignal::new(0i64);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let load = {
        let g = guild.clone();
        move || { let g = g.clone(); spawn_local(async move {
            if let Ok(v) = api("GET", &format!("/api/guilds/{g}/history"), None).await {
                version.set(v[0]["version"].as_i64().unwrap_or(0));
                set_rows.set(v.as_array().cloned().unwrap_or_default());
            }
        }) }
    };
    load();
    let (g1, g2) = (guild.clone(), guild);
    view! {
        <For each=move || rows.get() key=|r| r["version"].as_i64() let:r>
            {
                let v = r["version"].as_i64().unwrap_or(0);
                let (g1, g2, load) = (g1.clone(), g2.clone(), load.clone());
                view! {
                    <div class="card row">
                        <b>{format!("v{v}")}</b>
                        <span class="muted">{format!("by {} at {}", r["changed_by"].as_str().unwrap_or("?"), r["at"].as_str().unwrap_or(""))}</span>
                        <button class="ghost" on:click=move |_| { let g = g1.clone(); spawn_local(async move {
                            let cur = version.get_untracked();
                            match api("GET", &format!("/api/guilds/{g}/history/{v}/{cur}"), None).await {
                                Ok(d) => set_diff.set(format!("Changes from v{v} to current (v{cur}):\n{}", serde_json::to_string_pretty(&d).unwrap_or_default())),
                                Err(e) => set_diff.set(e),
                            }}) }>"Diff vs current"</button>
                        <button on:click=move |_| { let (g, load) = (g2.clone(), load.clone()); run_save(async move {
                            let r = api("POST", &format!("/api/guilds/{g}/history/{v}/rollback"), Some(json!({"expected_version": version.get_untracked()}))).await;
                            load(); r }, version, msg, need) }>"Roll back"</button>
                    </div>
                }
            }
        </For>
        <p>{move || msg.get()}</p>
        <StepUp need enrolled />
        <pre>{move || diff.get()}</pre>
    }
}

#[component]
fn App() -> impl IntoView {
    let (route, set_route) = signal(hash());
    window_event_listener(leptos::ev::hashchange, move |_| set_route.set(hash()));
    let (me, set_me) = signal(None::<Value>);
    let (enrolled, set_enrolled) = signal(false);
    let (guilds, set_guilds) = signal(Vec::<Value>::new());
    provide_context(guilds);
    let (guilds_note, set_guilds_note) = signal(String::from("Loading servers..."));
    let load_guilds = move || async move {
        match api("GET", "/api/guilds", None).await {
            Ok(g) => {
                let list = g.as_array().cloned().unwrap_or_default();
                set_guilds_note.set(if list.is_empty() { "No servers: the bot must be in the server, and you must be its owner, an extra owner, a trusted admin or hold a permit with dashboard_view.".into() } else { String::new() });
                set_guilds.set(list);
            }
            Err(e) => set_guilds_note.set(format!("Could not load servers: {e}")),
        }
    };
    spawn_local(async move {
        if let Ok(v) = api("GET", "/api/me", None).await {
            CSRF.with(|c| *c.borrow_mut() = v["csrf"].as_str().unwrap_or("").to_string());
            set_enrolled.set(v["totp_enrolled"].as_bool().unwrap_or(false));
            set_me.set(Some(v));
            load_guilds().await;
        }
    });
    view! {
        <Show when=move || me.get().is_some() fallback=|| view! { <main><h1>"TommyBot"</h1><a href="/auth/login"><button>"Log in with Discord"</button></a></main> }>
            <div class="shell">
                <nav>
                    {move || match route.get().first() {
                        None => view! { <a href="#/">"Servers"</a> }.into_any(),
                        Some(g) => {
                            let g = g.clone();
                            let page = route.get().get(1).cloned().unwrap_or_default();
                            let link = |p: &str, t: &str| { let on = page == p; view! { <a class:on=on href=format!("#/{g}/{p}")>{t.to_string()}</a> } };
                            let gi = guilds.get().into_iter().find(|x| x["id"] == g.as_str());
                            let name = gi.as_ref().and_then(|x| x["name"].as_str()).unwrap_or("Server").to_string();
                            let icon = match gi.as_ref().and_then(|x| x["icon"].as_str()) {
                                Some(i) => view! { <img src=format!("https://cdn.discordapp.com/icons/{g}/{i}.png?size=64") /> }.into_any(),
                                None => view! { <i>{name.chars().next().unwrap_or('?').to_string()}</i> }.into_any(),
                            };
                            view! {
                                <a class="srv" href="#/" title="Switch server">{icon}{name}</a>
                                {TOP.iter().map(|(p, t)| link(p, t)).collect_view()}
                                {NAV.iter().map(|(grp, items)| view! { <details open=*grp != "Raw JSON"><summary>{*grp}</summary>{items.iter().map(|(p, t)| link(p, t)).collect_view()}</details> }).collect_view()}
                            }.into_any()
                        }
                    }}
                </nav>
                <main>
                    {move || match route.get().as_slice() {
                        [] => view! {
                            <div class="head"><h1>"Servers"</h1><button class="ghost" on:click=move |_| { set_guilds_note.set("Loading servers...".into()); spawn_local(load_guilds()) }>"Refresh servers"</button></div>
                            <p class="muted">{move || guilds_note.get()}</p>
                            <div class="cards">{guilds.get().iter().map(|g| view! { <div class="card"><a class="gcard" href=format!("#/{}/overview", g["id"].as_str().unwrap_or(""))>{match g["icon"].as_str() {
                                Some(i) => view! { <img src=format!("https://cdn.discordapp.com/icons/{}/{i}.png?size=64", g["id"].as_str().unwrap_or("")) /> }.into_any(),
                                None => view! { <i>{g["name"].as_str().unwrap_or("?").chars().next().unwrap_or('?').to_string()}</i> }.into_any() }}
                                <b>{g["name"].as_str().unwrap_or("").to_string()}</b><span class="muted">{g["level"].as_str().unwrap_or("").to_string()}</span></a></div> }).collect_view()}</div>
                        }.into_any(),
                        [g] => view! { <Head guild=g.clone() page=title("overview") /><Score guild=g.clone() enrolled /> }.into_any(),
                        [g, p, ..] => {
                            let page = match p.as_str() {
                                "permits" => view! { <Permits guild=g.clone() enrolled /> }.into_any(),
                                "history" => view! { <History guild=g.clone() enrolled /> }.into_any(),
                                "overview" => view! { <Score guild=g.clone() enrolled /> }.into_any(),
                                "wizard" => view! { <Wizard guild=g.clone() enrolled /> }.into_any(),
                                "revert" => view! { <Revert guild=g.clone() enrolled /> }.into_any(),
                                "rescue" => view! { <Rescue guild=g.clone() enrolled /> }.into_any(),
                                "filters" => view! { <Section guild=g.clone() name="automod".to_string() part="filters" enrolled /> }.into_any(),
                                "automod" => view! { <Section guild=g.clone() name="automod".to_string() part="general" enrolled /> }.into_any(),
                                _ => view! { <Section guild=g.clone() name=p.clone() enrolled /> }.into_any(),
                            };
                            view! { <Head guild=g.clone() page=title(p) />{page} }.into_any()
                        }
                    }}
                </main>
            </div>
        </Show>
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        if let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) {
            let _ = body.insert_adjacent_text("beforeend", &format!("Dashboard crashed: {info}"));
        }
    }));
    leptos::mount::mount_to_body(App);
}
