use crate::common::*;
use crate::form::{Field, Section};
use leptos::{prelude::*, task::spawn_local};
use wasm_bindgen::JsCast;
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn later(ms: i32, f: impl Fn() + 'static) {
    let cb = wasm_bindgen::closure::Closure::<dyn Fn()>::new(f);
    if let Some(w) = web_sys::window() {
        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms);
    }
    cb.forget();
}

type Covers = fn(&Value) -> bool;
const LOCKS: [(&str, &str, &str, Covers); 4] = [
    ("Server Channels", "Members can no longer send messages in any channel.", "server", |l| l["channels"] == true),
    ("Server Roles", "Every role below the bot loses its dangerous permissions.", "role", |l| l["roles"] == true),
    ("Kick New Members", "New members are kicked on join and invites are paused.", "kick", |l| l["join"] == "Kick"),
    ("Ban New Members", "New members are banned on join.", "ban", |l| l["join"] == "Ban"),
];

#[component]
pub fn LockdownManage(guild: String) -> impl IntoView {
    let status = RwSignal::new(json!({}));
    let (msg, dc) = (RwSignal::new(String::new()), RwSignal::new(json!({})));
    let (secs, single) = (RwSignal::new(0i64), RwSignal::new(false));
    let channel = RwSignal::new(String::new());
    let g = StoredValue::new(guild);
    let load = move || spawn_local(async move {
        match api("GET", &format!("/api/guilds/{}/lockdown", g.get_value()), None).await {
            Ok(v) => status.set(v),
            Err(e) => msg.set(e),
        }
    });
    load();
    spawn_local(async move {
        if let Ok(d) = api("GET", &format!("/api/guilds/{}/discord", g.get_value()), None).await { dc.set(d); }
    });
    let act = move |path: &'static str, body: Value| spawn_local(async move {
        match api("POST", &format!("/api/guilds/{}/lockdown/{path}", g.get_value()), Some(body)).await {
            Ok(_) => { msg.set("Sent. It takes effect within a few seconds.".into()); later(6000, load); }
            Err(e) => msg.set(e),
        }
    });
    let lock = move |kind: &'static str, channel: Option<String>| act("lock", json!({"kind": kind, "channel": channel, "duration_s": (secs.get_untracked() > 0).then(|| secs.get_untracked())}));
    let locked = move || Some(status.get()["locked"].clone()).filter(|l| !l.is_null());
    view! {
        <div class="card row">
            <label>"Lock length"</label>
            <select on:change=move |e| secs.set(event_target_value(&e).parse().unwrap_or(0))>
                <option value="0">"Until I unlock"</option><option value="900">"15 minutes"</option><option value="3600">"1 hour"</option>
                <option value="21600">"6 hours"</option><option value="86400">"24 hours"</option>
            </select>
            <button class="ghost" on:click=move |_| load()>"Refresh"</button>
        </div>
        {LOCKS.iter().map(|(title, what, kind, covers)| {
            let is_on = move || locked().is_some_and(|l| covers(&l));
            view! { <div class="card row lockrow">
                <div><h3>{*title}</h3><span class="muted">{*what}</span></div><span class="sp"></span>
                <Show when=move || *kind == "server" && !is_on()><button class="ghost" on:click=move |_| single.update(|x| *x = !*x)>"Individual channels"</button></Show>
                <Show when=is_on fallback=move || view! { <span class="chip ok">"Unlocked"</span><button class="danger" disabled=move || locked().is_some() on:click=move |_| lock(kind, None)>"Lock"</button> }>
                    <span class="chip">{move || status.get()["unlock_at"].as_str().map_or("Locked".to_string(), |t| format!("Locked until {}", t.replace('T', " ").split('.').next().unwrap_or("")))}</span>
                    <button class="go" on:click=move |_| act("unlock", json!({}))>"Unlock"</button>
                </Show>
            </div> }
        }).collect_view()}
        <Show when=move || single.get()>
            <div class="card row">
                <label>"Lock a single channel"</label>
                <select on:change=move |e| channel.set(event_target_value(&e))>
                    <option value="">"Choose a channel"</option>
                    {move || dc.get()["channels"].as_array().cloned().unwrap_or_default().into_iter().map(|c| view! { <option value=s(&c["id"])>{format!("#{}", s(&c["name"]))}</option> }).collect_view()}
                </select>
                <button class="danger" disabled=move || channel.get().is_empty() || locked().is_some() on:click=move |_| lock("channel", Some(channel.get_untracked()))>"Lock"</button>
            </div>
        </Show>
        <p class="info">{move || msg.get()}</p>
    }
}

#[component]
pub fn LockdownSettings(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let problems = RwSignal::new(None::<Vec<Value>>);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let g = StoredValue::new(guild.clone());
    let check = move || spawn_local(async move {
        match api("GET", &format!("/api/guilds/{}/lockdown/check", g.get_value()), None).await {
            Ok(v) => problems.set(v.as_array().cloned()),
            Err(e) => msg.set(e),
        }
    });
    check();
    let fix = move || run_act(async move { api("POST", &format!("/api/guilds/{}/lockdown/fix", g.get_value()), None).await }, msg, need, move |_| check());
    view! {
        <div class="card">
            <div class="row"><h2>"Compatibility check"</h2>
                <button on:click=move |_| fix() disabled=move || !problems.get().is_some_and(|p| p.iter().any(|p| !p["fix"].is_null()))>"Apply Fix"</button></div>
            <p class="muted">"Checks that only the main roles can view and send, so a lockdown works in a few calls. Apply Fix corrects the role and channel permissions."</p>
            <Show when=move || problems.get().is_some_and(|p| p.is_empty())><p class="ok">"No lockdown compatibility issues found."</p></Show>
            {move || problems.get().unwrap_or_default().into_iter().map(|p| view! { <div class="row"><span class="warn">{s(&p["text"]).replace("<@&", "role ").replace("<#", "channel ").replace('>', "")}</span></div> }).collect_view()}
            <button class="ghost" on:click=move |_| check()>"Check again"</button>
            <p class="err">{move || msg.get()}</p>
        </div>
        <Section guild=guild.clone() name="lockdown".to_string() enrolled />
        <StepUp need enrolled />
    }
}

fn blank_rule() -> Value {
    json!({"name": "New rule", "enabled": true, "trigger": "keyword", "block_message": true, "keywords": [], "allow": [], "presets": [1, 2, 3], "mention_limit": 10,
        "custom_message": "", "alert_channel": "", "timeout_s": 0, "exempt_roles": [], "exempt_channels": [], "bot_actions": []})
}

#[component]
fn RuleEditor(guild: String, rule: RwSignal<Option<Value>>, dc: RwSignal<Value>, done: impl Fn() + Clone + 'static) -> impl IntoView {
    let val = RwSignal::new(rule.get_untracked().unwrap_or_default());
    let msg = RwSignal::new(String::new());
    let id = val.get_untracked()["id"].as_str().map(String::from);
    let trigger = move || s(&val.get()["trigger"]);
    let fld = move |path: &str, kind: &str, title: &str| view! { <Field f=json!({"path": path, "kind": kind, "title": title}) val dc /> };
    let save = {
        let done = done.clone();
        move |_| {
            let (g, body, id, done) = (guild.clone(), val.get_untracked(), id.clone(), done.clone());
            spawn_local(async move {
                let r = match id {
                    Some(id) => api("PUT", &format!("/api/guilds/{g}/discord-rules/{id}"), Some(body)).await,
                    None => api("POST", &format!("/api/guilds/{g}/discord-rules"), Some(body)).await,
                };
                match r { Ok(_) => done(), Err(e) => msg.set(e) }
            });
        }
    };
    view! {
        <div class="card">
            <h2>"Rule editor"</h2>
            {fld("name", "text", "name")}
            <div class="row"><label>"Trigger"</label>
                <select on:change=move |e| val.update(|v| v["trigger"] = json!(event_target_value(&e)))>
                    {[("keyword", "Keywords"), ("spam", "Spam"), ("mention", "Mention spam"), ("preset", "Keyword preset")].into_iter().map(|(k, t)| view! { <option value=k selected=move || trigger() == k>{t}</option> }).collect_view()}
                </select></div>
            <Show when=move || trigger() == "keyword">{fld("keywords", "list", "keywords (comma separated)")}</Show>
            <Show when=move || matches!(trigger().as_str(), "keyword" | "preset")>{fld("allow", "list", "allow list")}</Show>
            <Show when=move || trigger() == "mention">{fld("mention_limit", "int", "mentions per message")}</Show>
            <Show when=move || trigger() == "preset"><div class="row"><label>"Presets"</label>
                {[(1, "Profanity"), (2, "Sexual content"), (3, "Slurs")].into_iter().map(|(n, t)| view! { <label><input type="checkbox" prop:checked=move || val.get()["presets"].as_array().is_some_and(|a| a.contains(&json!(n)))
                    on:change=move |e| val.update(|v| { let mut p: Vec<i64> = v["presets"].as_array().map(|a| a.iter().filter_map(Value::as_i64).filter(|x| *x != n).collect()).unwrap_or_default(); if event_target_checked(&e) { p.push(n); } v["presets"] = json!(p); }) /> {t}</label> }).collect_view()}</div></Show>
            <h3>"Actions"</h3>
            {fld("block_message", "bool", "block message")}
            {fld("custom_message", "text", "message shown to the member")}
            {fld("alert_channel", "channel", "send an alert to")}
            <Show when=move || matches!(trigger().as_str(), "keyword" | "mention")>{fld("timeout_s", "int", "timeout (seconds, 0 = none)")}</Show>
            <Show when=move || val.get()["block_message"] == true><div class="row"><label>"Bot actions"</label>
                {["report", "warn", "timeout", "kick", "ban", "lock"].into_iter().map(|a| view! { <label><input type="checkbox" prop:checked=move || val.get()["bot_actions"].as_array().is_some_and(|l| l.contains(&json!(a)))
                    on:change=move |e| val.update(|v| { let mut l: Vec<String> = v["bot_actions"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(String::from)).filter(|x| x != a).collect()).unwrap_or_default(); if event_target_checked(&e) { l.push(a.into()); } v["bot_actions"] = json!(l); }) /> {a}</label> }).collect_view()}</div></Show>
            <h3>"Exempt"</h3>
            {fld("exempt_roles", "roles", "roles")}
            {fld("exempt_channels", "channels", "channels")}
            <div class="row"><button on:click=save>"Save rule"</button><button class="ghost" on:click=move |_| rule.set(None)>"Cancel"</button><span class="err">{move || msg.get()}</span></div>
        </div>
    }
}

#[component]
pub fn DiscordRules(guild: String) -> impl IntoView {
    let rules = RwSignal::new(Vec::<Value>::new());
    let (msg, dc, editing) = (RwSignal::new(String::new()), RwSignal::new(json!({})), RwSignal::new(None::<Value>));
    let g = StoredValue::new(guild);
    let load = move || spawn_local(async move {
        match api("GET", &format!("/api/guilds/{}/discord-rules", g.get_value()), None).await {
            Ok(v) => { rules.set(v.as_array().cloned().unwrap_or_default()); msg.set(String::new()); }
            Err(e) => msg.set(e),
        }
    });
    load();
    spawn_local(async move {
        if let Ok(d) = api("GET", &format!("/api/guilds/{}/discord", g.get_value()), None).await { dc.set(d); }
    });
    let call = move |method: &'static str, path: String, body: Option<Value>| spawn_local(async move {
        match api(method, &format!("/api/guilds/{}/discord-rules{path}", g.get_value()), body).await {
            Ok(_) => load(),
            Err(e) => msg.set(e),
        }
    });
    view! {
        <div class="card row">
            <button on:click=move |_| load()>"Refresh"</button>
            <button class="go" on:click=move |_| editing.set(Some(blank_rule()))>"+ Create"</button>
            <span class="err">{move || msg.get()}</span>
        </div>
        <Show when=move || rules.get().is_empty()>
            <div class="card empty"><p class="warn">"No Discord AutoMod rules found."</p>
                <button class="ghost dashed" on:click=move |_| editing.set(Some(blank_rule()))>"+ CREATE NEW RULE"</button></div>
        </Show>
        <For each=move || rules.get() key=|r| r.to_string() let:r>
            {
                let (id, on) = (s(&r["id"]), r["enabled"].as_bool().unwrap_or(false));
                let (id3, r2) = (id.clone(), r.clone());
                view! { <div class="card row">
                    <div><h3>{s(&r["name"])}</h3><span class="muted">{format!("trigger: {}", s(&r["trigger"]))}</span></div><span class="sp"></span>
                    <span class="chip" class:ok=on>{if on { "Active" } else { "Off" }}</span>
                    <button class=if on { "ghost" } else { "go" } on:click=move |_| call("POST", format!("/{id}/enabled"), Some(json!({"enabled": !on})))>{if on { "Deactivate" } else { "Activate" }}</button>
                    <button class="ghost" on:click=move |_| editing.set(Some(r2.clone()))>"Edit"</button>
                    <button class="danger" on:click=move |_| call("DELETE", format!("/{id3}"), None)>"Delete"</button>
                </div> }
            }
        </For>
        {move || editing.get().map(|_| {
            let done = move || { editing.set(None); load(); };
            view! { <RuleEditor guild=g.get_value() rule=editing dc=dc done /> }
        })}
    }
}
