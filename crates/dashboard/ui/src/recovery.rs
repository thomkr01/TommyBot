use crate::common::*;
use crate::form::Section;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) fn when(secs: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
    d.to_locale_string("en-GB", &wasm_bindgen::JsValue::UNDEFINED).as_string().unwrap_or_default()
}

/// Runs a queued bot action, then reloads after a few seconds, once the bot has picked the job up.
fn queued(fut: impl std::future::Future<Output = Result<Value, String>> + 'static, msg: RwSignal<String>, need: RwSignal<bool>, reload: impl Fn() + 'static) {
    run_act(fut, msg, need, move |_| {
        msg.set("Queued: the bot runs it within a few seconds.".into());
        set_timeout(reload, Duration::from_secs(6));
    });
}

#[component]
pub fn Backups(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let rows = RwSignal::new(Vec::<Value>::new());
    let preview = RwSignal::new(None::<(i64, Vec<String>)>);
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let load = {
        let g = guild.clone();
        move || { let g = g.clone(); spawn_local(async move {
            if let Ok(v) = api("GET", &format!("/api/guilds/{g}/backups"), None).await { rows.set(v.as_array().cloned().unwrap_or_default()) }
        }) }
    };
    load();
    let (gc, lc) = (guild.clone(), load.clone());
    let create = move |_| { let g = gc.clone(); queued(async move { api("POST", &format!("/api/guilds/{g}/backups"), None).await }, msg, need, lc.clone()) };
    let (gp, gl, gd) = (guild.clone(), guild.clone(), guild.clone());
    let show = Callback::new(move |id: i64| { let g = gp.clone(); msg.set(String::new()); run_act(async move { api("GET", &format!("/api/guilds/{g}/backups/{id}/preview"), None).await }, msg, need, move |v| {
        let lines = v["lines"].as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect()).unwrap_or_default();
        preview.set(Some((id, lines)));
    }) });
    let apply = Callback::new(move |id: i64| { let g = gl.clone(); preview.set(None); queued(async move { api("POST", &format!("/api/guilds/{g}/backups/{id}/load"), None).await }, msg, need, || ()) });
    let remove = Callback::new(move |id: i64| { let (g, l) = (gd.clone(), load.clone()); run_act(async move { api("POST", &format!("/api/guilds/{g}/backups/{id}/delete"), None).await }, msg, need, move |_| l()) });
    view! {
        <Section guild=guild.clone() name="backups".to_string() enrolled />
        <div class="card">
            <div class="row"><h2>"Backups"</h2><span class="sp"></span><button on:click=create>"Create backup"</button></div>
            <p class="muted">"TommyBot also saves one every 3 hours and keeps the last 10. Loading a backup syncs the server to it: it recreates what is missing, fixes what changed and removes what was added since."</p>
            <For each=move || rows.get() key=|r| r["id"].as_i64() let:r>
                {
                    let id = r["id"].as_i64().unwrap_or(0);
                    view! {
                        <div class="row">
                            <b>{format!("#{id}")}</b>
                            <span class="muted">{format!("{} · {} roles · {} channels", when(r["at"].as_i64().unwrap_or(0)), r["roles"], r["channels"])}</span>
                            <span class="sp"></span>
                            <button class="ghost" on:click=move |_| show.run(id)>"Load"</button>
                            <button class="ghost" on:click=move |_| remove.run(id)>"Delete"</button>
                        </div>
                    }
                }
            </For>
            <Show when=move || rows.get().is_empty()><p class="muted">"No backups yet."</p></Show>
        </div>
        {move || preview.get().map(|(id, lines)| view! {
            <div class="card">
                <h2>{format!("Load backup #{id}")}</h2>
                {if lines.is_empty() { view! { <p class="info">"The server already matches this backup."</p> }.into_any() } else {
                    view! { <p class="muted">{format!("This would change {} things:", lines.len())}</p><pre>{lines.join("\n")}</pre> }.into_any() }}
                <div class="row">
                    <button class="ghost" on:click=move |_| preview.set(None)>"Cancel"</button>
                    <Show when={let n = lines.len(); move || n > 0}><button on:click=move |_| apply.run(id)>"Apply"</button></Show>
                </div>
            </div>
        })}
        <p class="info">{move || msg.get()}</p>
        <StepUp need enrolled />
    }
}

/// Live state of panic mode with a manual start and end; `part` picks the settings the page shows.
#[component]
pub fn Panic(guild: String, part: &'static str, enrolled: ReadSignal<bool>) -> impl IntoView {
    let status = RwSignal::new(json!({}));
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let load = {
        let g = guild.clone();
        move || { let g = g.clone(); spawn_local(async move {
            if let Ok(v) = api("GET", &format!("/api/guilds/{g}/panic"), None).await { status.set(v) }
        }) }
    };
    load();
    let active = move || status.get()["active"].as_bool().unwrap_or(false);
    let g = guild.clone();
    let toggle = move |_| {
        let (g, what) = (g.clone(), if active() { "end" } else { "start" });
        queued(async move { api("POST", &format!("/api/guilds/{g}/panic/{what}"), None).await }, msg, need, load.clone());
    };
    view! {
        <div class="card">
            <h2>"Status"</h2>
            <p>{move || match status.get()["ends_at"].as_i64() {
                Some(t) if active() => format!("Panic mode is on until {}.", when(t)),
                _ => "Panic mode is off.".to_string(),
            }}</p>
            <div class="row"><button on:click=toggle>{move || if active() { "End panic mode now" } else { "Start panic mode now" }}</button></div>
            <p class="info">{move || msg.get()}</p>
        </div>
        <Section guild=guild.clone() name="panic".to_string() part=part enrolled />
        <StepUp need enrolled />
    }
}
