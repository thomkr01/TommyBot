use gloo_net::http::Request;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};
use std::cell::RefCell;

thread_local! { static CSRF: RefCell<String> = const { RefCell::new(String::new()) }; }

async fn api(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let csrf = CSRF.with(|c| c.borrow().clone());
    let b = match method {
        "GET" => Request::get(path),
        "PUT" => Request::put(path),
        _ => Request::post(path),
    }
    .header("x-csrf-token", &csrf);
    let req = match body {
        Some(v) => b.json(&v).map_err(|e| e.to_string())?,
        None => b.build().map_err(|e| e.to_string())?,
    };
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let ok = resp.ok();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if ok { Ok(v) } else { Err(v["error"].as_str().unwrap_or("request failed").to_string()) }
}

const SECTIONS: &[&str] = &[
    "wizard", "whitelist", "antinuke", "panic", "backups", "automod", "filters", "discord_automod",
    "condition_automod", "joingate", "joinraid", "verification", "lockdown", "moderation",
    "warn_points", "punish", "appeals", "logging", "utility",
];

fn hash() -> Vec<String> {
    let h = web_sys::window().unwrap().location().hash().unwrap_or_default();
    h.trim_start_matches('#').split('/').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn csv(v: &Value) -> String {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

fn from_csv(s: &str) -> Value {
    json!(s.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>())
}

/// Asks for a TOTP code (or enrollment) when a save was refused with `step_up_required`.
#[component]
fn StepUp(need: RwSignal<bool>, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (code, set_code) = signal(String::new());
    let (info, set_info) = signal(String::new());
    let enroll = move |_| spawn_local(async move {
        match api("POST", "/api/totp/enroll", None).await {
            Ok(v) => set_info.set(format!("Add this key to your authenticator app, then enter a code: {} ({})", v["secret"], v["uri"])),
            Err(e) => set_info.set(e),
        }
    });
    let verify = move |_| spawn_local(async move {
        match api("POST", "/api/totp/verify", Some(json!({"code": code.get_untracked()}))).await {
            Ok(_) => { need.set(false); set_info.set(String::new()); }
            Err(e) => set_info.set(e),
        }
    });
    view! {
        <Show when=move || need.get()>
            <div class="card">
                <b>"Confirm it's you"</b>
                <p class="muted">"This change needs your authenticator code. After verifying, save again."</p>
                <Show when=move || !enrolled.get()><button class="ghost" on:click=enroll>"Set up authenticator"</button></Show>
                <div class="row">
                    <input placeholder="123456" prop:value=code on:input=move |e| set_code.set(event_target_value(&e)) />
                    <button on:click=verify>"Verify"</button>
                </div>
                <p class="err">{move || info.get()}</p>
            </div>
        </Show>
    }
}

/// Runs a save, routing the `step_up_required` error to the prompt.
fn run_save(fut: impl std::future::Future<Output = Result<Value, String>> + 'static, version: RwSignal<i64>, msg: RwSignal<String>, need: RwSignal<bool>) {
    spawn_local(async move {
        match fut.await {
            Ok(v) => { version.set(v["version"].as_i64().unwrap_or(0)); msg.set(format!("Saved as version {}", version.get_untracked())); }
            Err(e) if e == "step_up_required" => need.set(true),
            Err(e) => msg.set(e),
        }
    });
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
        <h1>"Permits"</h1>
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

/// Every feature section edits its slice of the guild config as JSON until typed forms land.
#[component]
fn Section(guild: String, name: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let version = RwSignal::new(0i64);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let (g, n) = (guild.clone(), name.clone());
    spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{g}/config"), None).await {
            version.set(v["version"].as_i64().unwrap_or(0));
            text.set(serde_json::to_string_pretty(&v["config"][&n]).unwrap_or_default().replace("null", "{}"));
        }
    });
    let save = move |_| {
        let Ok(value) = serde_json::from_str::<Value>(&text.get_untracked()) else { return msg.set("Not valid JSON".into()) };
        let (g, n, v) = (guild.clone(), name.clone(), version.get_untracked());
        run_save(async move { api("PUT", &format!("/api/guilds/{g}/config/{n}"), Some(json!({"expected_version": v, "value": value}))).await }, version, msg, need);
    };
    view! {
        <textarea prop:value=move || text.get() on:input=move |e| text.set(event_target_value(&e))></textarea>
        <div class="row"><button on:click=save>"Save"</button><span>{move || msg.get()}</span></div>
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
        <h1>"Config history"</h1>
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
    spawn_local(async move {
        if let Ok(v) = api("GET", "/api/me", None).await {
            CSRF.with(|c| *c.borrow_mut() = v["csrf"].as_str().unwrap_or("").to_string());
            set_enrolled.set(v["totp_enrolled"].as_bool().unwrap_or(false));
            set_me.set(Some(v));
            if let Ok(g) = api("GET", "/api/guilds", None).await { set_guilds.set(g.as_array().cloned().unwrap_or_default()); }
        }
    });
    view! {
        <Show when=move || me.get().is_some() fallback=|| view! { <main><h1>"TommyBot"</h1><a href="/auth/login"><button>"Log in with Discord"</button></a></main> }>
            <div class="shell">
                <nav>
                    <a href="#/">"Servers"</a>
                    {move || route.get().first().map(|g| {
                        let g = g.clone();
                        let page = route.get().get(1).cloned().unwrap_or_default();
                        let link = |p: &str| { let on = page == p; view! { <a class:on=on href=format!("#/{g}/{p}")>{p.replace('_', " ")}</a> } };
                        view! { {link("permits")} {link("history")} {SECTIONS.iter().map(|s| link(s)).collect_view()} }
                    })}
                </nav>
                <main>
                    {move || match route.get().as_slice() {
                        [] => view! {
                            <h1>"Servers"</h1>
                            {guilds.get().iter().map(|g| view! { <div class="card"><a href=format!("#/{}/permits", g["id"].as_str().unwrap_or(""))>{g["name"].as_str().unwrap_or("").to_string()}</a> <span class="muted">{g["level"].as_str().unwrap_or("").to_string()}</span></div> }).collect_view()}
                        }.into_any(),
                        [g, p, ..] if p == "permits" => view! { <Permits guild=g.clone() enrolled /> }.into_any(),
                        [g, p, ..] if p == "history" => view! { <History guild=g.clone() enrolled /> }.into_any(),
                        [g, p, ..] => view! { <h1>{p.replace('_', " ")}</h1><Section guild=g.clone() name=p.clone() enrolled /> }.into_any(),
                        [g] => view! { <Permits guild=g.clone() enrolled /> }.into_any(),
                    }}
                </main>
            </div>
        </Show>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
