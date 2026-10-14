use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn confirm(text: &str) -> bool {
    web_sys::window().is_some_and(|w| w.confirm_with_message(text).unwrap_or(false))
}

/// Filter chips writing into `sig`; every change reloads through `load`.
fn chips(sig: RwSignal<String>, opts: &'static [(&'static str, &'static str)], load: impl Fn() + Copy + 'static) -> impl IntoView {
    view! { <div class="chips">{opts.iter().map(|(k, l)| view! { <button class="ghost" class:on=move || sig.get() == *k on:click=move |_| { sig.set((*k).into()); load() }>{*l}</button> }).collect_view()}</div> }
}

/// Loads `/api/guilds/{g}/{path}?{query}` into `rows`, errors into `msg`.
fn fetch(path: String, rows: WriteSignal<Vec<Value>>, msg: RwSignal<String>) {
    spawn_local(async move {
        match api("GET", &path, None).await {
            Ok(v) => { msg.set(String::new()); rows.set(v.as_array().cloned().unwrap_or_default()) }
            Err(e) => msg.set(e),
        }
    });
}

fn enc(v: &str) -> String {
    js_sys::encode_uri_component(v).into()
}

#[component]
pub fn CaseList(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (q, kind, state, msg) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let g = StoredValue::new(guild);
    let load = move || fetch(format!("/api/guilds/{}/cases?q={}&kind={}&state={}", g.get_value(), enc(&q.get_untracked()), kind.get_untracked(), state.get_untracked()), set_rows, msg);
    load();
    let post = move |no: i64, path: &str, body: Value| {
        let url = format!("/api/guilds/{}/cases/{no}{path}", g.get_value());
        spawn_local(async move {
            match api("POST", &url, Some(body)).await {
                Ok(_) => load(),
                Err(e) => msg.set(e),
            }
        });
    };
    view! {
        <div class="card row">
            {chips(state, &[("", "All"), ("open", "Open"), ("closed", "Closed")], load)}
            <select on:change=move |e| { kind.set(event_target_value(&e)); load() }>
                <option value="">"Any type"</option>
                {["ban", "kick", "timeout", "warn", "quarantine", "sanitize", "purge", "slowmode", "verify"].map(|k| view! { <option value=k>{k}</option> }).to_vec()}
            </select>
            <input placeholder="User or mod ID" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| load() />
            <button class="ghost" on:click=move |_| load()>"Search"</button>
            <span class="err">{move || msg.get()}</span>
        </div>
        <Show when=move || rows.get().is_empty()><div class="banner">"No cases found.."</div></Show>
        <For each=move || rows.get() key=|r| (r["case_no"].as_i64(), r["active"].as_bool(), r["reason"].as_str().map(String::from), r["proof"].as_str().map(String::from)) let:r>
            {
                let no = r["case_no"].as_i64().unwrap_or(0);
                let active = r["active"].as_bool().unwrap_or(true);
                let (reason, proof) = (RwSignal::new(s(&r["reason"])), RwSignal::new(s(&r["proof"])));
                let save = move |active: bool| post(no, "", json!({"reason": reason.get_untracked(), "proof": proof.get_untracked(), "active": active}));
                view! { <div class="card">
                    <div class="row"><b>{format!("#{no} {}", s(&r["kind"]))}</b><span class="muted">{if active { "open" } else { "closed" }}</span>
                        <span>"user "<code>{s(&r["user_id"])}</code></span><span>"mod "<code>{s(&r["mod_id"])}</code></span>
                        <span class="muted">{format!("{} points, {}", r["points"], s(&r["at"]))}</span></div>
                    <div class="row"><input placeholder="Reason" prop:value=reason on:input=move |e| reason.set(event_target_value(&e)) /><input placeholder="Proof" prop:value=proof on:input=move |e| proof.set(event_target_value(&e)) /></div>
                    <div class="row">
                        <button on:click=move |_| save(active)>"Save"</button>
                        <button class="ghost" on:click=move |_| save(!active)>{if active { "Close" } else { "Reopen" }}</button>
                        <button class="ghost" on:click=move |_| if confirm("Delete this case for good?") { post(no, "/delete", json!({})) }>"Delete"</button>
                    </div>
                </div> }
            }
        </For>
    }
}

#[component]
pub fn NoteList(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (q, text, msg) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let g = StoredValue::new(guild);
    let load = move || fetch(format!("/api/guilds/{}/notes?q={}", g.get_value(), enc(&q.get_untracked())), set_rows, msg);
    load();
    let post = move |path: String, body: Value| spawn_local(async move {
        match api("POST", &format!("/api/guilds/{}/notes{path}", g.get_value()), Some(body)).await {
            Ok(_) => load(),
            Err(e) => msg.set(e),
        }
    });
    view! {
        <div class="card row">
            <input placeholder="User ID" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| load() />
            <button class="ghost" on:click=move |_| load()>"Search"</button>
            <input placeholder="New note" maxlength="500" prop:value=text on:input=move |e| text.set(event_target_value(&e)) />
            <button on:click=move |_| { post(String::new(), json!({"user_id": q.get_untracked(), "text": text.get_untracked()})); text.set(String::new()) }>"Add note"</button>
            <button class="ghost" on:click=move |_| if confirm("Remove every note of this user?") { post("/sweep".into(), json!({"user_id": q.get_untracked()})) }>"Sweep user"</button>
            <span class="err">{move || msg.get()}</span>
        </div>
        <Show when=move || rows.get().is_empty()><div class="banner">"No notes found.."</div></Show>
        <For each=move || rows.get() key=|r| (r["id"].as_i64(), r["text"].as_str().map(String::from)) let:r>
            {
                let id = r["id"].as_i64().unwrap_or(0);
                let body = RwSignal::new(s(&r["text"]));
                view! { <div class="card">
                    <div class="row"><b>{format!("#{id}")}</b><code>{s(&r["user_id"])}</code><span class="muted">{format!("by {}, {}", s(&r["author_id"]), s(&r["at"]))}</span></div>
                    <div class="row"><input maxlength="500" prop:value=body on:input=move |e| body.set(event_target_value(&e)) />
                        <button on:click=move |_| post(format!("/{id}"), json!({"text": body.get_untracked()}))>"Save"</button>
                        <button class="ghost" on:click=move |_| post(format!("/{id}/delete"), json!({}))>"Delete"</button></div>
                </div> }
            }
        </For>
    }
}

#[component]
pub fn ReportList(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (state, msg) = (RwSignal::new("open".to_string()), RwSignal::new(String::new()));
    let g = StoredValue::new(guild);
    let load = move || fetch(format!("/api/guilds/{}/reports?state={}", g.get_value(), state.get_untracked()), set_rows, msg);
    load();
    let set_open = move |id: i64, open: bool| spawn_local(async move {
        match api("POST", &format!("/api/guilds/{}/reports/{id}/status", g.get_value()), Some(json!({"open": open}))).await {
            Ok(_) => load(),
            Err(e) => msg.set(e),
        }
    });
    view! {
        <div class="card row">
            {chips(state, &[("open", "Open"), ("dismissed", "Dismissed"), ("actioned", "Actioned"), ("", "All")], load)}
            <span class="muted">"Warn, timeout, kick and ban are done with the buttons on the report in Discord."</span>
            <span class="err">{move || msg.get()}</span>
        </div>
        <Show when=move || rows.get().is_empty()><div class="banner">"No reports.."</div></Show>
        <For each=move || rows.get() key=|r| (r["id"].as_i64(), r["status"].as_str().map(String::from)) let:r>
            {
                let id = r["id"].as_i64().unwrap_or(0);
                let open = r["status"] == "open";
                let g_id = g.get_value();
                let jump = format!("https://discord.com/channels/{g_id}/{}/{}", s(&r["channel_id"]), s(&r["message_id"]));
                let handled = match r["handled_by"].as_str() {
                    Some(by) => format!("{} by {by}", r["action"].as_str().unwrap_or(r["status"].as_str().unwrap_or(""))),
                    None => s(&r["status"]),
                };
                view! { <div class="card">
                    <div class="row"><b>{format!("Report #{id}")}</b><span class="muted">{handled}</span>
                        <span>"reporter "<code>{s(&r["reporter_id"])}</code></span><span>"target "<code>{s(&r["target_id"])}</code></span>
                        <a href=jump target="_blank" rel="noopener">"Jump to message"</a></div>
                    <p>{s(&r["content"])}</p>
                    <div class="row"><button class="ghost" on:click=move |_| set_open(id, !open)>{if open { "Dismiss" } else { "Reopen" }}</button></div>
                </div> }
            }
        </For>
    }
}
