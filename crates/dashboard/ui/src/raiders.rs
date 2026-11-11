//! Raider List pages: a server's Matches tab, the bot owner's review page (#/raiders) and a listed
//! user's own appeal page (#/raider-appeal). All text from users is shown as plain text.
use crate::common::*;
use crate::records::{export_csv, fetch, pager};
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

const CATEGORIES: [&str; 4] = ["nuke", "raid", "scam", "spam"];

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn when(v: &Value) -> String {
    v.as_str()
        .map(|t| t.chars().take(16).collect::<String>().replace('T', " "))
        .unwrap_or_default()
}

fn enc(v: &str) -> String {
    js_sys::encode_uri_component(v).into()
}

/// What the Raider List did in this server, 50 a page.
#[component]
pub fn RaiderMatches(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (msg, loaded, page) = (Msg::default(), RwSignal::new(false), RwSignal::new(0i64));
    let list = StoredValue::new(format!("/api/guilds/{guild}/raider_matches?page="));
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(format!("{}{}", list.get_value(), page.get_untracked()), set_rows, msg, loaded)
    };
    load();
    view! {
        <Toast msg />
        <p class="muted">"Servers see only the category and the bot owner's short note, never the evidence or which servers it came from."</p>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"The Raider List has not matched anyone here yet."</p></Show>
        {move || rows.get().into_iter().map(|r| {
            let case = r["case_no"].as_i64().map(|n| format!("case #{n}"));
            let gone = (r["listed"] != true).then(|| view! { <span class="chip orange">"no longer listed"</span> });
            view! { <div class="card">
                <div class="row"><code>{s(&r["user_id"])}</code><b>{s(&r["category"])}</b><span>{s(&r["action"])}</span>{gone}{case.map(|c| view! { <span class="muted">{c}</span> })}<span class="muted">{when(&r["at"])}</span></div>
                <p>{s(&r["public_note"])}</p>
            </div> }
        }).collect_view()}
        {pager(page, move || rows.get().len(), load)}
    }
}

/// The user with avatar, name, id, account age and the distinct owner count.
fn who(e: &Value) -> impl IntoView {
    let id = s(&e["user_id"]);
    let avatar = e["avatar"].as_str().map(|a| format!("https://cdn.discordapp.com/avatars/{id}/{a}.png?size=64"));
    let name = e["name"].as_str().map(String::from).unwrap_or_else(|| "unknown user".into());
    let bot = (e["bot"] == true).then(|| view! { <span class="chip">"bot"</span> });
    view! { <div class="row">
        {avatar.map(|src| view! { <img src=src alt="" width="32" height="32" /> })}
        <b>{name}</b><code>{id}</code>{bot}
        <span class="muted">{format!("account made {}", when(&e["created_at"]))}</span>
        <span>{format!("{} different owners, {} evidence", e["owners"].as_i64().unwrap_or(0), e["evidence_total"].as_i64().unwrap_or(0))}</span>
    </div> }
}

/// Every shown evidence row: server (name and size), kind, time, case number and the counts.
fn evidence(e: &Value) -> impl IntoView {
    let rows = e["evidence"].as_array().cloned().unwrap_or_default();
    view! { <table>
        <thead><tr><th>"Server"</th><th>"Kind"</th><th>"Time"</th><th>"Case"</th><th>"Detail"</th></tr></thead>
        <tbody>{rows.into_iter().map(|ev| view! { <tr>
            <td>{format!("{} ({} members)", s(&ev["server"]), ev["members"].as_u64().unwrap_or(0))}</td>
            <td>{s(&ev["kind"]).replace('_', " ")}</td>
            <td>{when(&ev["at"])}</td>
            <td>{ev["case_no"].as_i64().map(|n| format!("#{n}"))}</td>
            <td><code>{ev["detail"].to_string()}</code>{ev["note"].as_str().map(|n| view! { <div>{format!("Reason: {n}")}</div> })}</td>
        </tr> }).collect_view()}</tbody>
    </table> }
}

/// The bot owner's review: Queue, Listed and Appeals. Every decision needs 2FA.
#[component]
pub fn RaiderReview(enrolled: ReadSignal<bool>) -> impl IntoView {
    let tab = RwSignal::new("queue");
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let tabs = [("queue", "Queue"), ("listed", "Listed"), ("appeals", "Appeals")];
    view! {
        <div class="head"><small>"TommyBot"</small><h1>"Raider List"</h1></div>
        <StepUp need enrolled />
        <div class="tabs">{tabs.map(|(k, t)| view! { <a href="#/raiders" class:on=move || tab.get() == k on:click=move |e| { e.prevent_default(); tab.set(k) }>{t}</a> }).to_vec()}</div>
        {move || match tab.get() {
            "listed" => view! { <Listed msg need /> }.into_any(),
            "appeals" => view! { <Appeals msg need /> }.into_any(),
            _ => view! { <Queue msg need /> }.into_any(),
        }}
        <Toast msg />
    }
}

/// POSTs a decision; `done` runs after it went through.
fn decide(path: String, body: Value, msg: Msg, need: RwSignal<bool>, ok: &'static str, done: impl Fn() + 'static) {
    run_act(async move { api("POST", &path, Some(body)).await }, msg, need, move |_| {
        msg.ok(ok);
        done()
    })
}

#[component]
fn Queue(msg: Msg, need: RwSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (loaded, page) = (RwSignal::new(false), RwSignal::new(0i64));
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(format!("/api/raiders/queue?page={}", page.get_untracked()), set_rows, msg, loaded)
    };
    load();
    view! {
        <p class="muted">"Automatic listings that wait for your check come first, then pending entries with the most different owners."</p>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"Nothing to review."</p></Show>
        {move || rows.get().into_iter().map(|e| {
            let id = s(&e["user_id"]);
            let auto = e["status"] == "confirmed";
            let cat = RwSignal::new(s(&e["category"]));
            let note = RwSignal::new(String::new());
            let (i1, i2, i3, i4) = (id.clone(), id.clone(), id.clone(), id.clone());
            let approve = move |_| decide(format!("/api/raiders/{i1}/confirm"), json!({"category": cat.get_untracked(), "note": note.get_untracked()}), msg, need, "Confirmed. Servers with the Raider List on will see it.", load);
            let verify = move |_| decide(format!("/api/raiders/{i2}/verify"), json!({}), msg, need, "Verified.", load);
            let remove = move |_| if confirm(&format!("Take {i3} off the list? Bans the list made are lifted where the server allows it.")) { decide(format!("/api/raiders/{i3}/remove"), json!({}), msg, need, "Removed.", load) };
            let reject = move |_| decide(format!("/api/raiders/{i4}/reject"), json!({}), msg, need, "Rejected.", load);
            view! { <div class="card">
                {who(&e)}
                <div class="row">{if auto { view! { <span class="chip orange">"listed automatically, not verified"</span> }.into_any() } else { view! { <span class="chip">"pending"</span> }.into_any() }}<span class="muted">{format!("since {}", when(&e["listed_at"]))}</span></div>
                {evidence(&e)}
                <div class="row">
                    <select aria-label="category" on:change=move |ev| cat.set(event_target_value(&ev))>{CATEGORIES.map(|c| view! { <option value=c prop:selected=move || cat.get() == c>{c}</option> }).to_vec()}</select>
                    <input placeholder="Public note (servers and the user see it)" aria-label="public note" maxlength="200" prop:value=note on:input=move |ev| note.set(event_target_value(&ev)) />
                    <button class="go" on:click=approve>"Confirm"</button>
                    {if auto { view! { <button on:click=verify>"Verify"</button><button class="danger" on:click=remove>"Remove"</button> }.into_any() } else { view! { <button class="danger" on:click=reject>"Reject"</button> }.into_any() }}
                </div>
            </div> }
        }).collect_view()}
        {pager(page, move || rows.get().len(), load)}
    }
}

#[component]
fn Listed(msg: Msg, need: RwSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (loaded, page, q) = (RwSignal::new(false), RwSignal::new(0i64), RwSignal::new(String::new()));
    let list = move || format!("/api/raiders/listed?q={}&page=", enc(&q.get_untracked()));
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(format!("{}{}", list(), page.get_untracked()), set_rows, msg, loaded)
    };
    let first = move || {
        page.set(0);
        load()
    };
    load();
    view! {
        <div class="card row">
            <input placeholder="User ID or note" aria-label="search" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| first() />
            <button class="ghost" on:click=move |_| first()>"Search"</button>
            {export_csv(list, "raider_list", &["user_id", "category", "verified", "public_note", "evidence", "owners", "reviewed_at", "expires_at", "updated_at"], msg)}
        </div>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No one is listed."</p></Show>
        {move || rows.get().into_iter().map(|r| {
            let id = s(&r["user_id"]);
            let id2 = id.clone();
            let remove = move |_| if confirm(&format!("Take {id2} off the list? Bans the list made are lifted where the server allows it.")) { decide(format!("/api/raiders/{id2}/remove"), json!({}), msg, need, "Removed.", load) };
            let unverified = (r["verified"] != true).then(|| view! { <span class="chip orange">"not verified"</span> });
            let ends = r["expires_at"].is_string().then(|| view! { <span class="muted">{format!("ends {}", when(&r["expires_at"]))}</span> });
            view! { <div class="card">
                <div class="row"><code>{id}</code><b>{s(&r["category"])}</b>{unverified}
                    <span>{format!("{} different owners, {} evidence", r["owners"].as_i64().unwrap_or(0), r["evidence"].as_i64().unwrap_or(0))}</span>
                    <span class="muted">{when(&r["updated_at"])}</span>{ends}
                    <button class="danger" on:click=remove>"Remove"</button></div>
                <p>{s(&r["public_note"])}</p>
            </div> }
        }).collect_view()}
        {pager(page, move || rows.get().len(), load)}
    }
}

#[component]
fn Appeals(msg: Msg, need: RwSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (loaded, page) = (RwSignal::new(false), RwSignal::new(0i64));
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(format!("/api/raiders/appeals?page={}", page.get_untracked()), set_rows, msg, loaded)
    };
    load();
    view! {
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No open appeals."</p></Show>
        {move || rows.get().into_iter().map(|a| {
            let id = a["id"].as_i64().unwrap_or_default();
            let accept = move |_| if confirm("Accept this appeal? The user is taken off the list and the bans it made are lifted.") { decide(format!("/api/raiders/appeals/{id}/accept"), json!({}), msg, need, "Appeal accepted.", load) };
            let deny = move |_| decide(format!("/api/raiders/appeals/{id}/deny"), json!({}), msg, need, "Appeal denied. The user can appeal again in 30 days.", load);
            view! { <div class="card">
                {who(&a)}
                <div class="row"><b>{s(&a["category"])}</b><span class="muted">{s(&a["status"])}</span><span>{s(&a["public_note"])}</span><span class="muted">{format!("appealed {}", when(&a["at"]))}</span></div>
                <p style="white-space:pre-wrap">{s(&a["text"])}</p>
                {evidence(&a)}
                <div class="row"><button class="go" on:click=accept>"Accept"</button><button class="danger" on:click=deny>"Deny"</button></div>
            </div> }
        }).collect_view()}
        {pager(page, move || rows.get().len(), load)}
    }
}

/// A signed-in user's own Raider List status, and their appeal.
#[component]
pub fn RaiderAppeal() -> impl IntoView {
    let data = RwSignal::new(Value::Null);
    let text = RwSignal::new(String::new());
    let (msg, need) = (Msg::default(), RwSignal::new(false));
    let load = move || {
        if data.is_disposed() {
            return;
        }
        spawn_local(async move {
            let r = api("GET", "/api/raider-appeal", None).await;
            if data.is_disposed() {
                return;
            }
            match r {
                Ok(v) => data.set(v),
                Err(e) => msg.err(e),
            }
        })
    };
    load();
    let send = move |_| {
        let body = json!({"text": text.get_untracked()});
        run_act(async move { api("POST", "/api/raider-appeal", Some(body)).await }, msg, need, move |_| {
            msg.ok("Appeal sent. The bot owner reads every appeal.");
            text.set(String::new());
            load()
        })
    };
    view! {
        <div class="head"><small>"TommyBot"</small><h1>"Raider List"</h1></div>
        <div class="card">
            <p class="muted">"The Raider List holds accounts TommyBot caught raiding, nuking, spamming or scamming in several servers. Servers that use it may ban, kick or quarantine listed accounts when they join."</p>
            {move || {
                let d = data.get();
                if d.is_null() {
                    return ().into_any();
                }
                if d["listed"] != true {
                    let removed = d["status"] == "removed";
                    return view! { <p><b>{if removed { "You were taken off the Raider List." } else { "You are not on the Raider List." }}</b></p> }.into_any();
                }
                let ends = d["expires_at"].is_string().then(|| view! { <p class="muted">{format!("This listing ends by itself on {}.", when(&d["expires_at"]))}</p> });
                let last = d["appeal"]["status"].as_str().map(|st| match st {
                    "open" => "Your appeal is waiting for the bot owner.".to_string(),
                    "denied" => format!("Your last appeal was denied. You can appeal again after {}.", when(&d["appeal_after"])),
                    _ => format!("Your last appeal was {st}."),
                });
                view! {
                    <p><b>{format!("You are on the Raider List as: {}", s(&d["category"]))}</b></p>
                    <p>{s(&d["public_note"])}</p>
                    {ends}
                    {last.map(|l| view! { <p class="info">{l}</p> })}
                    <Show when=move || data.get()["can_appeal"] == true>
                        <label for="raider-appeal">"Why should you be taken off the list? (at most 1000 characters)"</label>
                        <textarea id="raider-appeal" maxlength="1000" style="min-height:120px" prop:value=text on:input=move |e| text.set(event_target_value(&e))></textarea>
                        <div class="row"><button class="go" disabled=move || text.get().trim().is_empty() on:click=send>"Send appeal"</button></div>
                    </Show>
                }.into_any()
            }}
        </div>
        <Toast msg />
    }
}
