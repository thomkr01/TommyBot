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
    let (kind, q, msg) = (RwSignal::new(String::new()), RwSignal::new(String::new()), Msg::default());
    let loaded = RwSignal::new(false);
    let (g, page) = (StoredValue::new(guild), RwSignal::new(0i64));
    let load = move || spawn_local(async move {
        let path = format!("/api/guilds/{}/appeals?kind={}&q={}&page={}", g.get_value(), kind.get_untracked(), js_sys::encode_uri_component(&q.get_untracked()), page.get_untracked());
        match api("GET", &path, None).await {
            Ok(v) => { msg.clear(); set_rows.set(v.as_array().cloned().unwrap_or_default()) }
            Err(e) => msg.err(e),
        }
        loaded.set(true);
    });
    let first = move || { page.set(0); load() };
    load();
    let reply = RwSignal::new(String::new());
    let decide = move |id: i64, accept: bool, blacklist: bool, dismiss: bool| spawn_local(async move {
        let path = format!("/api/guilds/{}/appeals/{id}/decide", g.get_value());
        let body = json!({"accept": accept, "blacklist": blacklist, "dismiss": dismiss, "reply": reply.get_untracked()});
        match api("POST", &path, Some(body)).await {
            Ok(_) => load(),
            Err(e) => msg.err(e),
        }
    });
    view! {
        <div class="card row">
            <div class="chips">
                {["ban", "timeout"].map(|k| view! { <button class="ghost" class:on=move || kind.get() == k on:click=move |_| { kind.set(if kind.get_untracked() == k { String::new() } else { k.into() }); first() }>{k}</button> }).collect_view()}
            </div>
            <input placeholder="User ID/tag" aria-label="user ID or tag" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| first() />
            <button class="ghost" on:click=move |_| first()>"Search"</button>
            <input placeholder="Reply to the member (optional)" aria-label="reply to the member" prop:value=reply on:input=move |e| reply.set(event_target_value(&e)) />
            <Toast msg />
        </div>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No pending appeals."</p></Show>
        <For each=move || rows.get() key=|r| (r["id"].as_i64(), r["status"].as_str().map(String::from)) let:r>
            {
                let id = r["id"].as_i64().unwrap_or(0);
                let open = r["status"] == "open";
                view! { <div class="card">
                    <div class="row"><b>{s(&r["username"])}</b><code>{s(&r["user_id"])}</code><span class="muted">{format!("{} appeal, {}", s(&r["kind"]), s(&r["status"]))}</span></div>
                    {r["answers"].as_array().cloned().unwrap_or_default().into_iter().map(|a| view! { <p><b>{s(&a["label"])}</b><br/>{s(&a["answer"])}</p> }).collect_view()}
                    <Show when=move || open>
                        <div class="row">
                            <button on:click=move |_| decide(id, true, false, false)>"Accept"</button>
                            <button class="ghost" on:click=move |_| decide(id, false, false, false)>"Deny"</button>
                            <button class="ghost" on:click=move |_| decide(id, false, true, false)>"Deny and blacklist"</button>
                            <button class="ghost" on:click=move |_| decide(id, false, false, true)>"Dismiss"</button>
                        </div>
                    </Show>
                </div> }
            }
        </For>
        {crate::records::pager(page, move || rows.get().len(), load)}
    }
}

/// Blocklist of one feature (`base` is "appeals" or "reports"); only appeals can purge it.
#[component]
pub fn Blacklist(guild: String, base: &'static str, #[prop(optional)] purge: bool) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (user, msg) = (RwSignal::new(String::new()), Msg::default());
    let loaded = RwSignal::new(false);
    let g = StoredValue::new(guild);
    let load = move || spawn_local(async move {
        match api("GET", &format!("/api/guilds/{}/{base}/blacklist", g.get_value()), None).await {
            Ok(v) => set_rows.set(v.as_array().cloned().unwrap_or_default()),
            Err(e) => msg.err(e),
        }
        loaded.set(true);
    });
    load();
    let post = move |path: String, body: Option<Value>| spawn_local(async move {
        match api("POST", &format!("/api/guilds/{}/{base}/blacklist{path}", g.get_value()), body).await {
            Ok(_) => { msg.clear(); load() }
            Err(e) => msg.err(e),
        }
    });
    view! {
        <div class="card row">
            <input placeholder="User ID" aria-label="user ID" prop:value=user on:input=move |e| user.set(event_target_value(&e)) />
            <button on:click=move |_| post(String::new(), Some(json!({"user_id": user.get_untracked()})))>"Blacklist user"</button>
            <Show when=move || purge><button class="ghost" on:click=move |_| if confirm("Remove every user from the appeals blacklist?") { post("/purge".into(), None) }>"Purge all"</button></Show>
            <Toast msg />
        </div>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No blacklisted users."</p></Show>
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
    let (answers, msg, done) = (RwSignal::new(Vec::<String>::new()), Msg::default(), RwSignal::new(false));
    let g = StoredValue::new(guild);
    let mine = RwSignal::new(Vec::<Value>::new());
    let load_mine = move || spawn_local(async move {
        if let Ok(v) = api("GET", &format!("/api/appeal/{}/mine", g.get_value()), None).await {
            mine.set(v.as_array().cloned().unwrap_or_default());
        }
    });
    load_mine();
    let withdraw = move |_| spawn_local(async move {
        match api("POST", &format!("/api/appeal/{}/withdraw", g.get_value()), None).await {
            Ok(_) => load_mine(),
            Err(e) => msg.err(e),
        }
    });
    spawn_local(async move {
        match api("GET", &format!("/api/appeal/{}", g.get_value()), None).await {
            Ok(v) => { answers.set(vec![String::new(); v["fields"].as_array().map_or(0, Vec::len)]); info.set(v) }
            Err(e) => msg.err(if e == "not found" { "Appeals are not enabled in this server.".into() } else { e }),
        }
    });
    let submit = move |_| spawn_local(async move {
        match api("POST", &format!("/api/appeal/{}", g.get_value()), Some(json!({"answers": answers.get_untracked()}))).await {
            Ok(_) => done.set(true),
            Err(e) => msg.err(e),
        }
    });
    view! {
        <div class="head"><h1>"Appeal"</h1></div>
        <Toast msg />
        <For each=move || mine.get() key=|r| (r["id"].as_i64(), r["status"].as_str().map(String::from)) let:r>
            {
                let open = r["status"] == "open";
                view! { <div class="card row">
                    <b>{format!("Appeal #{}", r["id"])}</b><span class="muted">{format!("{} appeal, {}", s(&r["kind"]), s(&r["status"]))}</span>
                    {r["reply"].as_str().map(|t| view! { <span>{format!("Staff reply: {t}")}</span> })}
                    <Show when=move || open><button class="ghost" on:click=withdraw>"Withdraw"</button></Show>
                </div> }
            }
        </For>
        <Show when=move || done.get() fallback=move || view! {
            <Show when=move || !info.get().is_null()>
                {move || {
                    let i = info.get();
                    if i["blacklisted"] == true { return view! { <p>"You can no longer appeal in this server."</p> }.into_any(); }
                    if i["kind"].is_null() { return view! { <p>"You are not banned or timed out in this server."</p> }.into_any(); }
                    view! { <div class="card">
                        {i["fields"].as_array().cloned().unwrap_or_default().into_iter().enumerate().map(|(n, f)| {
                            let on = move |e| answers.update(|a| a[n] = event_target_value(&e));
                            let opts: Vec<String> = f["options"].as_array().map(|o| o.iter().map(s).collect()).unwrap_or_default();
                            let max = f["max"].as_u64().unwrap_or(1000).to_string();
                            let input = if opts.is_empty() {
                                if f["long"] == true { view! { <textarea class="short" aria-label=s(&f["label"]) maxlength=max on:input=on></textarea> }.into_any() } else { view! { <input aria-label=s(&f["label"]) maxlength=max on:input=on /> }.into_any() }
                            } else if f["multi"] == true {
                                view! { <div class="pick" role="group" aria-label=s(&f["label"])>{opts.into_iter().map(|o| { let o2 = o.clone(); view! { <label><input type="checkbox" on:change=move |e| answers.update(|a| {
                                    let mut picked: Vec<String> = a[n].lines().filter(|l| *l != o2).map(String::from).collect();
                                    if event_target_checked(&e) { picked.push(o2.clone()) }
                                    a[n] = picked.join("\n");
                                }) /> {o}</label> } }).collect_view()}</div> }.into_any()
                            } else {
                                view! { <select aria-label=s(&f["label"]) on:change=on><option value="">"Choose"</option>{opts.into_iter().map(|o| { let v = o.clone(); view! { <option value=v>{o}</option> } }).collect_view()}</select> }.into_any()
                            };
                            view! { <div class="row"><label>{s(&f["label"])}</label>{input}</div> }
                        }).collect_view()}
                        <button on:click=submit>"Send appeal"</button>
                    </div> }.into_any()
                }}
            </Show>
        }><p>"Your appeal was sent. Staff will review it."</p></Show>
    }
}
