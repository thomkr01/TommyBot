use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Staff view: filter, search, accept or deny.
#[component]
pub fn AppealList(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (kind, q, msg) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let g = StoredValue::new(guild);
    let load = move || spawn_local(async move {
        let path = format!("/api/guilds/{}/appeals?kind={}&q={}", g.get_value(), kind.get_untracked(), js_sys::encode_uri_component(&q.get_untracked()));
        match api("GET", &path, None).await {
            Ok(v) => set_rows.set(v.as_array().cloned().unwrap_or_default()),
            Err(e) => msg.set(e),
        }
    });
    load();
    let decide = move |id: i64, accept: bool, blacklist: bool| spawn_local(async move {
        let path = format!("/api/guilds/{}/appeals/{id}/decide", g.get_value());
        match api("POST", &path, Some(json!({"accept": accept, "blacklist": blacklist}))).await {
            Ok(_) => load(),
            Err(e) => msg.set(e),
        }
    });
    view! {
        <div class="card row">
            <div class="chips">
                {[("", "All"), ("ban", "Ban"), ("timeout", "Timeout")].map(|(k, l)| view! { <button class="ghost" class:on=move || kind.get() == k on:click=move |_| { kind.set(k.into()); load() }>{l}</button> }).collect_view()}
            </div>
            <input placeholder="User ID/tag" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| load() />
            <button class="ghost" on:click=move |_| load()>"Search"</button>
            <span class="err">{move || msg.get()}</span>
        </div>
        <Show when=move || rows.get().is_empty()><div class="banner">"No pending appeals.."</div></Show>
        <For each=move || rows.get() key=|r| (r["id"].as_i64(), r["status"].as_str().map(String::from)) let:r>
            {
                let id = r["id"].as_i64().unwrap_or(0);
                let open = r["status"] == "open";
                view! { <div class="card">
                    <div class="row"><b>{s(&r["username"])}</b><code>{s(&r["user_id"])}</code><span class="muted">{format!("{} appeal, {}", s(&r["kind"]), s(&r["status"]))}</span></div>
                    {r["answers"].as_array().cloned().unwrap_or_default().into_iter().map(|a| view! { <p><b>{s(&a["label"])}</b><br/>{s(&a["answer"])}</p> }).collect_view()}
                    <Show when=move || open>
                        <div class="row">
                            <button on:click=move |_| decide(id, true, false)>"Accept"</button>
                            <button class="ghost" on:click=move |_| decide(id, false, false)>"Deny"</button>
                            <button class="ghost" on:click=move |_| decide(id, false, true)>"Deny and blacklist"</button>
                        </div>
                    </Show>
                </div> }
            }
        </For>
    }
}

/// Blocklist of one feature (`base` is "appeals" or "reports"); only appeals can purge it.
#[component]
pub fn Blacklist(guild: String, base: &'static str, #[prop(optional)] purge: bool) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (user, msg) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let g = StoredValue::new(guild);
    let load = move || spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/guilds/{}/{base}/blacklist", g.get_value()), None).await {
            set_rows.set(v.as_array().cloned().unwrap_or_default());
        }
    });
    load();
    let post = move |path: String, body: Option<Value>| spawn_local(async move {
        match api("POST", &format!("/api/guilds/{}/{base}/blacklist{path}", g.get_value()), body).await {
            Ok(_) => { msg.set(String::new()); load() }
            Err(e) => msg.set(e),
        }
    });
    view! {
        <div class="card row">
            <input placeholder="User ID" prop:value=user on:input=move |e| user.set(event_target_value(&e)) />
            <button on:click=move |_| post(String::new(), Some(json!({"user_id": user.get_untracked()})))>"Blacklist user"</button>
            <Show when=move || purge><button class="ghost" on:click=move |_| if web_sys::window().is_some_and(|w| w.confirm_with_message("Remove every user from the appeals blacklist?").unwrap_or(false)) { post("/purge".into(), None) }>"Purge all"</button></Show>
            <span class="err">{move || msg.get()}</span>
        </div>
        <Show when=move || rows.get().is_empty()><div class="banner">"No blacklisted users.."</div></Show>
        <For each=move || rows.get() key=|r| s(&r["user_id"]) let:r>
            {
                let id = s(&r["user_id"]);
                view! { <div class="card row"><code>{id.clone()}</code><span class="muted">{s(&r["at"])}</span>
                    <button class="ghost" on:click=move |_| post(format!("/{id}/remove"), None)>"Remove"</button></div> }
            }
        </For>
    }
}

/// Public page a banned or timed-out user opens from the DM link.
#[component]
pub fn AppealForm(guild: String) -> impl IntoView {
    let info = RwSignal::new(Value::Null);
    let (answers, msg, done) = (RwSignal::new(Vec::<String>::new()), RwSignal::new(String::new()), RwSignal::new(false));
    let g = StoredValue::new(guild);
    spawn_local(async move {
        match api("GET", &format!("/api/appeal/{}", g.get_value()), None).await {
            Ok(v) => { answers.set(vec![String::new(); v["fields"].as_array().map_or(0, Vec::len)]); info.set(v) }
            Err(e) => msg.set(if e == "not found" { "Appeals are not enabled in this server.".into() } else { e }),
        }
    });
    let submit = move |_| spawn_local(async move {
        match api("POST", &format!("/api/appeal/{}", g.get_value()), Some(json!({"answers": answers.get_untracked()}))).await {
            Ok(_) => done.set(true),
            Err(e) => msg.set(e),
        }
    });
    view! {
        <h1>"Appeal"</h1>
        <p class="err">{move || msg.get()}</p>
        <Show when=move || done.get() fallback=move || view! {
            <Show when=move || !info.get().is_null()>
                {move || {
                    let i = info.get();
                    if i["blacklisted"] == true { return view! { <p>"You can no longer appeal in this server."</p> }.into_any(); }
                    if i["kind"].is_null() { return view! { <p>"You are not banned or timed out in this server."</p> }.into_any(); }
                    view! { <div class="card">
                        {i["fields"].as_array().cloned().unwrap_or_default().into_iter().enumerate().map(|(n, f)| {
                            let on = move |e| answers.update(|a| a[n] = event_target_value(&e));
                            view! { <div class="row"><label>{s(&f["label"])}</label>
                                {if f["long"] == true { view! { <textarea maxlength="1000" on:input=on></textarea> }.into_any() } else { view! { <input maxlength="1000" on:input=on /> }.into_any() }}</div> }
                        }).collect_view()}
                        <button on:click=submit>"Send appeal"</button>
                    </div> }.into_any()
                }}
            </Show>
        }><p>"Your appeal was sent. Staff will review it."</p></Show>
    }
}
