use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Filter chips writing into `sig`; every change reloads through `load`.
fn chips(
    sig: RwSignal<String>,
    opts: &'static [(&'static str, &'static str)],
    load: impl Fn() + Copy + 'static,
) -> impl IntoView {
    view! { <div class="chips">{opts.iter().map(|(k, l)| view! { <button class="ghost" class:on=move || sig.get() == *k on:click=move |_| { sig.set((*k).into()); load() }>{*l}</button> }).collect_view()}</div> }
}

/// Loads `/api/guilds/{g}/{path}?{query}` into `rows`, errors into `msg`; `loaded` turns on after the first answer.
pub(crate) fn fetch(path: String, rows: WriteSignal<Vec<Value>>, msg: Msg, loaded: RwSignal<bool>) {
    spawn_local(async move {
        match api("GET", &path, None).await {
            Ok(v) => {
                msg.clear();
                rows.set(v.as_array().cloned().unwrap_or_default())
            }
            Err(e) => msg.err(e),
        }
        loaded.set(true);
    });
}

/// "Export CSV": loads every page of `list` (a list URL ending in `page=`, so the export keeps the
/// current filter) and downloads it as `{name}.csv`.
pub fn export_csv(
    list: impl Fn() -> String + Copy + 'static,
    name: &'static str,
    cols: &'static [&'static str],
    msg: Msg,
) -> impl IntoView {
    let go = move |_| {
        let base = list();
        spawn_local(async move {
            let mut all = vec![];
            for p in 0.. {
                match api("GET", &format!("{base}{p}"), None).await {
                    Ok(v) => {
                        let page = v.as_array().cloned().unwrap_or_default();
                        let last = page.len() < 50;
                        all.extend(page);
                        if last {
                            break;
                        }
                    }
                    Err(e) => return msg.err(e),
                }
            }
            download(
                &format!("{name}.csv"),
                &format!("data:text/csv;charset=utf-8,{}", enc(&to_csv(cols, &all))),
            );
        });
    };
    view! { <button class="ghost" on:click=go>"Export CSV"</button> }
}

/// Every cell quoted; text starting with = + - @ gets a leading ' so spreadsheets do not run it as a formula.
fn csv_cell(v: &Value) -> String {
    let t = match v {
        Value::String(t) => t.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    let t = if t.starts_with(['=', '+', '-', '@']) {
        format!("'{t}")
    } else {
        t
    };
    format!("\"{}\"", t.replace('"', "\"\""))
}

fn to_csv(cols: &[&str], rows: &[Value]) -> String {
    let mut out = cols.join(",") + "\r\n";
    for r in rows {
        out += &cols
            .iter()
            .map(|c| csv_cell(&r[*c]))
            .collect::<Vec<_>>()
            .join(",");
        out += "\r\n";
    }
    out
}

/// Previous/next buttons for lists the API serves 50 rows a page.
pub fn pager(
    page: RwSignal<i64>,
    count: impl Fn() -> usize + Copy + Send + Sync + 'static,
    load: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let shown = move || page.get() > 0 || count() == 50;
    view! { <Show when=shown><div class="row pager">
        <button class="ghost" aria-label="previous page" disabled=move || page.get() == 0 on:click=move |_| { page.update(|p| *p -= 1); load() }>{crate::icons::icon("page_prev")}</button>
        <span>{move || format!("Page {}", page.get() + 1)}</span>
        <button class="ghost" aria-label="next page" disabled=move || count() < 50 on:click=move |_| { page.update(|p| *p += 1); load() }>{crate::icons::icon("page_next")}</button>
    </div></Show> }
}

/// Looks up member names for `ids` (25 per request, the API's cap); members who left keep their ID.
fn learn_names(
    guild: String,
    mut ids: Vec<String>,
    names: RwSignal<std::collections::HashMap<String, String>>,
) {
    ids.sort();
    ids.dedup();
    ids.retain(|i| !i.is_empty() && !names.with_untracked(|n| n.contains_key(i)));
    for chunk in ids.chunks(25) {
        let path = format!("/api/guilds/{guild}/members?ids={}", chunk.join(","));
        spawn_local(async move {
            if let Ok(v) = api("GET", &path, None).await {
                names.update(|n| {
                    n.extend(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|m| (s(&m["id"]), s(&m["name"]))),
                    )
                });
            }
        });
    }
}

fn enc(v: &str) -> String {
    js_sys::encode_uri_component(v).into()
}

#[component]
pub fn CaseList(guild: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let need = RwSignal::new(false);
    // Owners and extra owners may report a ban, kick or quarantine case; the server checks again.
    let owner = expect_context::<Access>().rank() == 4;
    let loaded = RwSignal::new(false);
    let (q, kind, state, msg) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        Msg::default(),
    );
    let g = StoredValue::new(guild);
    let page = RwSignal::new(0i64);
    let list = move || {
        format!(
            "/api/guilds/{}/cases?q={}&kind={}&state={}&page=",
            g.get_value(),
            enc(&q.get_untracked()),
            kind.get_untracked(),
            state.get_untracked()
        )
    };
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(
            format!("{}{}", list(), page.get_untracked()),
            set_rows,
            msg,
            loaded,
        )
    };
    let first = move || {
        page.set(0);
        load()
    };
    load();
    let names = RwSignal::new(std::collections::HashMap::<String, String>::new());
    Effect::new(move || {
        learn_names(
            g.get_value(),
            rows.get()
                .iter()
                .flat_map(|r| [s(&r["user_id"]), s(&r["mod_id"])])
                .collect(),
            names,
        )
    });
    // Name with the ID on hover; the bare ID until the name arrives or when the member left.
    let who = move |id: String| {
        let tip = id.clone();
        view! { <b title=tip>{move || names.with(|n| n.get(&id).cloned()).unwrap_or_else(|| id.clone())}</b> }
    };
    let post = move |no: i64, path: &str, body: Value| {
        let url = format!("/api/guilds/{}/cases/{no}{path}", g.get_value());
        spawn_local(async move {
            match api("POST", &url, Some(body)).await {
                Ok(_) => load(),
                Err(e) => msg.err(e),
            }
        });
    };
    view! {
        <div class="card row">
            {chips(state, &[("", "All"), ("open", "Open"), ("closed", "Closed")], first)}
            <select aria-label="case type" on:change=move |e| { kind.set(event_target_value(&e)); first() }>
                <option value="">"Any type"</option>
                {["ban", "kick", "timeout", "warn", "quarantine", "sanitize", "purge", "slowmode", "verify"].map(|k| view! { <option value=k>{k}</option> }).to_vec()}
            </select>
            <input placeholder="User or mod ID" aria-label="user or mod ID" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| first() />
            <button class="ghost" on:click=move |_| first()>"Search"</button>
            {export_csv(list, "cases", &["case_no", "kind", "user_id", "mod_id", "reason", "proof", "points", "active", "at"], msg)}
            <Toast msg />
        </div>
        <StepUp need enrolled />
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No cases found."</p></Show>
        <For each=move || rows.get() key=|r| (r["case_no"].as_i64(), r["active"].as_bool(), r["reason"].as_str().map(String::from), r["proof"].as_str().map(String::from)) let:r>
            {
                let no = r["case_no"].as_i64().unwrap_or(0);
                let active = r["active"].as_bool().unwrap_or(true);
                let (reason, proof) = (RwSignal::new(s(&r["reason"])), RwSignal::new(s(&r["proof"])));
                let save = move |active: bool| post(no, "", json!({"reason": reason.get_untracked(), "proof": proof.get_untracked(), "active": active}));
                view! { <div class="card">
                    <div class="row"><b>{format!("#{no} {}", s(&r["kind"]))}</b><span class="muted">{if active { "open" } else { "closed" }}</span>
                        <span>"user "{who(s(&r["user_id"]))}</span><span>"mod "{who(s(&r["mod_id"]))}</span>
                        <span class="muted">{format!("{} points, {}", r["points"], s(&r["at"]))}</span></div>
                    <div class="row"><input placeholder="Reason" aria-label="reason" prop:value=reason on:input=move |e| reason.set(event_target_value(&e)) /><input placeholder="Proof" aria-label="proof" prop:value=proof on:input=move |e| proof.set(event_target_value(&e)) /></div>
                    <div class="row">
                        <button on:click=move |_| save(active)>"Save"</button>
                        <button class="ghost" on:click=move |_| save(!active)>{if active { "Close" } else { "Reopen" }}</button>
                        <button class="ghost" on:click=move |_| if confirm("Delete this case for good?") { post(no, "/delete", json!({})) }>"Delete"</button>
                        {(owner && matches!(s(&r["kind"]).as_str(), "ban" | "kick" | "quarantine")).then(|| report(g.get_value(), no, msg, need))}
                    </div>
                </div> }
            }
        </For>
        {pager(page, move || rows.get().len(), load)}
    }
}

/// "3 days ago" for an age in seconds.
fn ago(secs: f64) -> String {
    let d = (secs / 86400.0).floor() as i64;
    match d {
        i64::MIN..=0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

/// The change against the period before: "new" when that one was empty.
fn trend(now: i64, prev: i64) -> (String, &'static str) {
    match (now, prev) {
        (0, 0) => ("–".into(), "muted"),
        (_, 0) => ("new".into(), "info"),
        _ => {
            let pct = (now - prev) * 100 / prev;
            let cls = if pct > 0 { "warn" } else if pct < 0 { "muted" } else { "" };
            (format!("{}{pct} %", if pct > 0 { "+" } else { "" }), cls)
        }
    }
}

/// Small bar row of a member's actions over the period, oldest first.
fn bars(series: &[i64]) -> impl IntoView {
    let max = series.iter().copied().max().unwrap_or(0).max(1) as f64;
    let w = series.len().max(1) * 4;
    let rects = series
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(i, n)| {
            let h = (*n as f64 / max * 20.0).max(2.0);
            view! { <rect x=(i * 4).to_string() y=format!("{:.1}", 20.0 - h) width="3" height=format!("{h:.1}") rx="1" /> }
        })
        .collect_view();
    view! { <svg class="bars" viewBox=format!("0 0 {w} 20") preserveAspectRatio="none" aria-hidden="true">{rects}</svg> }
}

#[component]
pub fn ModStats(guild: String) -> impl IntoView {
    let data = RwSignal::new(Value::Null);
    let open = RwSignal::new(None::<String>);
    let detail = RwSignal::new(Vec::<Value>::new());
    let (days, msg) = (RwSignal::new("30".to_string()), Msg::default());
    let g = StoredValue::new(guild);
    let load = move || {
        if days.is_disposed() {
            return;
        }
        let d = days.get_untracked();
        let path = format!("/api/guilds/{}/mod_stats?days={d}", g.get_value());
        spawn_local(async move {
            let r = api("GET", &path, None).await;
            // A slower answer for a period chip clicked earlier must not overwrite the newer one.
            if days.try_get_untracked().as_deref() != Some(d.as_str()) {
                return;
            }
            match r {
                Ok(v) => {
                    msg.clear();
                    data.set(v)
                }
                Err(e) => msg.err(e),
            }
        });
    };
    load();
    let names = RwSignal::new(std::collections::HashMap::<String, String>::new());
    Effect::new(move || {
        let ids: Vec<String> = data.with(|d| d["mods"].as_array().into_iter().flatten().map(|m| s(&m["mod_id"])).collect());
        learn_names(g.get_value(), ids, names)
    });
    let name = move |id: &str| names.with(|n| n.get(id).cloned()).unwrap_or_else(|| id.to_string());
    let show = move |id: String| {
        if open.get_untracked().as_deref() == Some(&id) {
            return open.set(None);
        }
        detail.set(vec![]);
        open.set(Some(id.clone()));
        let path = format!("/api/guilds/{}/mod_stats/{id}", g.get_value());
        spawn_local(async move {
            let r = api("GET", &path, None).await;
            // Another row may have been opened meanwhile, or the page left.
            if open.try_get_untracked().flatten().as_deref() != Some(id.as_str()) {
                return;
            }
            match r {
                Ok(v) => {
                    let ids = v.as_array().into_iter().flatten().map(|r| s(&r["target_id"])).collect();
                    learn_names(g.get_value(), ids, names);
                    detail.set(v.as_array().cloned().unwrap_or_default())
                }
                Err(e) => {
                    open.set(None);
                    msg.err(e)
                }
            }
        });
    };
    let kinds = move || data.with(|d| d["kinds"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>());
    let csv = move |_| {
        let ks = kinds();
        let mut out = format!("mod_id,name,{},total,previous,last_action\r\n", ks.join(","));
        data.with_untracked(|d| {
            for m in d["mods"].as_array().into_iter().flatten() {
                let id = s(&m["mod_id"]);
                let mut row = vec![json!(id), json!(name(&id))];
                row.extend(ks.iter().map(|k| m["by_kind"][k].clone()));
                row.extend([m["total"].clone(), m["prev_total"].clone(), m["last_at"].clone()]);
                out += &row.iter().map(csv_cell).collect::<Vec<_>>().join(",");
                out += "\r\n";
            }
        });
        download("mod-stats.csv", &format!("data:text/csv;charset=utf-8,{}", enc(&out)));
    };
    let now = js_sys::Date::now();
    let age = move |m: &Value| m["last_at"].as_str().map(|t| ago((now - js_sys::Date::parse(t)) / 1000.0)).unwrap_or_default();
    let rows = move || {
        let ks = kinds();
        data.with(|d| d["mods"].as_array().cloned().unwrap_or_default()).into_iter().map(|m| {
            let id = s(&m["mod_id"]);
            let total = m["total"].as_i64().unwrap_or(0);
            let (change, cls) = trend(total, m["prev_total"].as_i64().unwrap_or(0));
            let series: Vec<i64> = m["series"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
            let (tip, key, who) = (id.clone(), id.clone(), id.clone());
            let cells = ks.iter().map(|k| view! { <td>{m["by_kind"][k].as_i64().unwrap_or(0)}</td> }).collect_view();
            let span = (ks.len() + 5).to_string();
            view! {
                <tr class="click" class:quiet=total == 0 on:click=move |_| show(key.clone())>
                    <td><b title=tip>{move || name(&who)}</b></td>{cells}<td><b>{total}</b></td>
                    <td class=cls>{change}</td><td class="muted">{age(&m)}</td><td>{bars(&series)}</td>
                </tr>
                <Show when={ let id = id.clone(); move || open.get().as_deref() == Some(&id) }>
                    <tr class="detail"><td colspan=span.clone()>
                        <Show when=move || !detail.get().is_empty() fallback=|| view! { <span class="muted">"Loading…"</span> }>
                            <ul>{move || detail.get().into_iter().map(|r| {
                                let t = s(&r["target_id"]);
                                let case = r["case_no"].as_i64().map(|n| format!(" (case #{n})")).unwrap_or_default();
                                let at = s(&r["at"]).chars().take(16).collect::<String>().replace('T', " ");
                                view! { <li><span class="muted">{at}</span>" "<b>{s(&r["kind"])}</b>" "{move || name(&t)}{case}<span class="muted">{format!(" · {}", s(&r["source"]))}</span></li> }
                            }).collect_view()}</ul>
                        </Show>
                    </td></tr>
                </Show>
            }
        }).collect_view()
    };
    let empty = move || data.with(|d| d["mods"].as_array().is_some_and(|m| m.is_empty()));
    view! {
        <div class="card row">
            {chips(days, &[("7", "7 days"), ("30", "30 days"), ("90", "90 days"), ("365", "1 year")], move || { open.set(None); load() })}
            <button class="ghost" on:click=csv>"Export CSV"</button>
            <Toast msg />
        </div>
        <p class="muted">"Actions per staff member in the period, against the period before. Bans, kicks and timeouts done in the Discord app count too. Staff with nothing in the period are dimmed. Click a row for their last 50 actions."</p>
        <Show when=empty fallback=move || view! {
            <div class="card scroll"><table class="permits stats">
                <thead><tr><th>"Staff"</th>{move || kinds().into_iter().map(|k| view! { <th>{k}</th> }).collect_view()}<th>"Total"</th><th>"Change"</th><th>"Last action"</th><th>"Over time"</th></tr></thead>
                <tbody>{rows}</tbody>
            </table></div>
        }><p class="card empty muted">"No staff actions recorded yet."</p></Show>
    }
}

/// "Report to Raider List": a category and a button. The report waits for the bot owner's review.
fn report(guild: String, no: i64, msg: Msg, need: RwSignal<bool>) -> impl IntoView {
    let cat = RwSignal::new("raid".to_string());
    let reason = RwSignal::new(String::new());
    let path = StoredValue::new(format!("/api/guilds/{guild}/cases/{no}/raider_report"));
    let go = move |_| {
        if !confirm("Report this user to the Raider List? The bot owner reviews every report before other servers use it.") {
            return;
        }
        let body = json!({"category": cat.get_untracked(), "reason": reason.get_untracked()});
        let path = path.get_value();
        run_act(async move { api("POST", &path, Some(body)).await }, msg, need, move |_| msg.ok("Reported. The bot owner reviews it."));
    };
    view! {
        <select aria-label="Raider List category" on:change=move |e| cat.set(event_target_value(&e))>
            {["raid", "nuke", "scam", "spam"].map(|c| view! { <option value=c prop:selected=move || cat.get() == c>{c}</option> }).to_vec()}
        </select>
        <input placeholder="Reason (only the bot owner sees it)" aria-label="Raider List report reason" maxlength="400" prop:value=reason on:input=move |e| reason.set(event_target_value(&e)) />
        <button class="ghost" on:click=go>"Report to Raider List"</button>
    }
}

#[component]
pub fn NoteList(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let loaded = RwSignal::new(false);
    let (q, text, msg) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        Msg::default(),
    );
    let g = StoredValue::new(guild);
    let page = RwSignal::new(0i64);
    let list = move || {
        format!(
            "/api/guilds/{}/notes?q={}&page=",
            g.get_value(),
            enc(&q.get_untracked())
        )
    };
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(
            format!("{}{}", list(), page.get_untracked()),
            set_rows,
            msg,
            loaded,
        )
    };
    let first = move || {
        page.set(0);
        load()
    };
    load();
    let post = move |path: String, body: Value| {
        spawn_local(async move {
            match api(
                "POST",
                &format!("/api/guilds/{}/notes{path}", g.get_value()),
                Some(body),
            )
            .await
            {
                Ok(_) => load(),
                Err(e) => msg.err(e),
            }
        })
    };
    view! {
        <div class="card row">
            <input placeholder="User ID" aria-label="user ID" prop:value=q on:input=move |e| q.set(event_target_value(&e)) on:change=move |_| first() />
            <button class="ghost" on:click=move |_| first()>"Search"</button>
            <input placeholder="New note" aria-label="new note" maxlength="500" prop:value=text on:input=move |e| text.set(event_target_value(&e)) />
            <button on:click=move |_| { post(String::new(), json!({"user_id": q.get_untracked(), "text": text.get_untracked()})); text.set(String::new()) }>"Add note"</button>
            <button class="ghost" on:click=move |_| if confirm("Remove every note of this user?") { post("/sweep".into(), json!({"user_id": q.get_untracked()})) }>"Sweep user"</button>
            <button class="ghost" on:click=move |_| if confirm("Remove every note in this server? This cannot be undone.") { post("/sweep".into(), json!({"all": true})) }>"Sweep server"</button>
            {export_csv(list, "notes", &["id", "user_id", "author_id", "text", "at"], msg)}
            <Toast msg />
        </div>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No notes found."</p></Show>
        <For each=move || rows.get() key=|r| (s(&r["id"]), s(&r["text"])) let:r>
            {
                let id = s(&r["id"]);
                let del = id.clone();
                let body = RwSignal::new(s(&r["text"]));
                let edits = r["edits"].as_array().cloned().unwrap_or_default();
                view! { <div class="card">
                    <div class="row"><b>{id.clone()}</b><code>{s(&r["user_id"])}</code><span class="muted">{format!("Created {} by {}", s(&r["at"]), s(&r["author_id"]))}</span></div>
                    {(!edits.is_empty()).then(|| view! { <details><summary>{format!("History ({})", edits.len())}</summary><ul>
                        {edits.iter().map(|e| view! { <li><span class="muted">{format!("{} by {}: ", s(&e["at"]), s(&e["by"]))}</span>{s(&e["old"])}" → "{s(&e["new"])}</li> }).collect_view()}
                    </ul></details> })}
                    <div class="row"><input maxlength="500" aria-label="note text" prop:value=body on:input=move |e| body.set(event_target_value(&e)) />
                        <button on:click=move |_| post(format!("/{id}"), json!({"text": body.get_untracked()}))>"Save"</button>
                        <button class="ghost" on:click=move |_| if confirm("Delete this note?") { post(format!("/{del}/delete"), json!({})) }>"Delete"</button></div>
                </div> }
            }
        </For>
        {pager(page, move || rows.get().len(), load)}
    }
}

#[component]
pub fn ReportList(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let loaded = RwSignal::new(false);
    let (state, msg) = (RwSignal::new("open".to_string()), Msg::default());
    let g = StoredValue::new(guild);
    let page = RwSignal::new(0i64);
    let list = move || {
        format!(
            "/api/guilds/{}/reports?state={}&page=",
            g.get_value(),
            state.get_untracked()
        )
    };
    let load = move || {
        if page.is_disposed() {
            return;
        }
        fetch(
            format!("{}{}", list(), page.get_untracked()),
            set_rows,
            msg,
            loaded,
        )
    };
    let first = move || {
        page.set(0);
        load()
    };
    load();
    let set_open = move |id: i64, open: bool| {
        spawn_local(async move {
            match api(
                "POST",
                &format!("/api/guilds/{}/reports/{id}/status", g.get_value()),
                Some(json!({"open": open})),
            )
            .await
            {
                Ok(_) => load(),
                Err(e) => msg.err(e),
            }
        })
    };
    view! {
        <div class="card row">
            {chips(state, &[("open", "Open"), ("dismissed", "Dismissed"), ("actioned", "Actioned"), ("", "All")], first)}
            <span class="muted">"Warn, timeout, kick and ban are done with the buttons on the report in Discord."</span>
            {export_csv(list, "reports", &["id", "reporter_id", "target_id", "channel_id", "message_id", "content", "status", "action", "handled_by", "at"], msg)}
            <Toast msg />
        </div>
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No reports."</p></Show>
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
        {pager(page, move || rows.get().len(), load)}
    }
}

/// Every account each join raid caught; the raid trigger log links here.
#[component]
pub fn RaidLog(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let msg = Msg::default();
    let loaded = RwSignal::new(false);
    fetch(
        format!("/api/guilds/{guild}/raid_log"),
        set_rows,
        msg,
        loaded,
    );
    view! {
        <Toast msg />
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"No join raids caught yet."</p></Show>
        {move || rows.get().into_iter().map(|r| {
            let accounts = r["accounts"].as_array().cloned().unwrap_or_default();
            view! { <div class="card">
                <h3>{format!("Raid at {} ({} caught)", crate::recovery::when(r["raid_ms"].as_i64().unwrap_or(0) / 1000), accounts.len())}</h3>
                {accounts.iter().map(|a| view! { <div class="row"><code>{s(&a["user_id"])}</code><span>{s(&a["name"])}</span></div> }).collect_view()}
            </div> }
        }).collect_view()}
    }
}

/// Every Join Gate punishment: the filters that matched, the bot and who added it, and Discord's refusal.
#[component]
pub fn JoinGateLog(guild: String) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<Value>::new());
    let (msg, loaded, page) = (Msg::default(), RwSignal::new(false), RwSignal::new(0i64));
    let load = move || {
        fetch(
            format!("/api/guilds/{guild}/joingate_log?page={}", page.get_untracked()),
            set_rows,
            msg,
            loaded,
        )
    };
    let load = StoredValue::new(load);
    load.get_value()();
    let reload = move || load.get_value()();
    view! {
        <Toast msg />
        <Show when=move || loaded.get() && rows.get().is_empty()><p class="card empty muted">"Join Gate has not punished anyone yet."</p></Show>
        {move || rows.get().into_iter().map(|r| {
            let filters: Vec<String> = r["filters"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect();
            let bot = r["is_bot"].as_bool().unwrap_or(false).then(|| match r["adder_id"].as_str() {
                Some(a) => format!("Bot added by {a}"),
                None => "Bot, adder unknown".into(),
            });
            let result = match r["ok"].as_bool() {
                Some(true) => "Done".to_string(),
                _ => format!("Failed: {}", s(&r["error"])),
            };
            view! { <div class="card">
                <div class="row"><b>{s(&r["action"])}</b><code>{s(&r["user_id"])}</code><span class="muted">{s(&r["at"])}</span><span>{result}</span></div>
                <p>{s(&r["reason"])}{(!filters.is_empty()).then(|| format!(" ({})", filters.join(", ")))}</p>
                {bot.map(|b| view! { <p class="muted">{b}</p> })}
            </div> }
        }).collect_view()}
        {pager(page, move || rows.get().len(), reload)}
    }
}

/// One transcript line, `[time] Name (Role): text`, as (time, name, role, text).
fn log_line(l: &str) -> (&str, &str, &str, &str) {
    let Some((time, rest)) = l.strip_prefix('[').and_then(|r| r.split_once("] ")) else {
        return ("", "", "", l);
    };
    let Some((who, text)) = rest.split_once(": ") else {
        return (time, "", "", rest);
    };
    match who.rsplit_once(" (") {
        Some((name, role)) => (time, name, role.trim_end_matches(')'), text),
        None => (time, who, "", text),
    }
}

/// A closed ticket's conversation, opened from the link on its ticket log.
#[component]
pub fn TicketLog(guild: String, channel: String, message: String) -> impl IntoView {
    let (text, set_text) = signal(String::new());
    let msg = Msg::default();
    let path = format!("/api/guilds/{guild}/tickets/log/{channel}/{message}");
    spawn_local(async move {
        match api("GET", &path, None).await {
            Ok(v) => set_text.set(v["text"].as_str().unwrap_or("").to_string()),
            Err(e) => msg.err(e),
        }
    });
    view! {
        <Toast msg />
        <div class="card">
            <h3>{crate::icons::icon("modmail")}" Ticket transcript"</h3>
            {move || text.get().lines().map(|l| {
                let (time, name, role, body) = log_line(l);
                let chip = match role { "User" => "chip blue", "Staff" => "chip", _ => "chip muted" };
                view! { <div class="row">
                    <span class="muted">{time.to_string()}</span>
                    <b>{name.to_string()}</b>
                    {(!role.is_empty()).then(|| view! { <span class=chip>{role.to_string()}</span> })}
                    <span>{body.to_string()}</span>
                </div> }
            }).collect_view()}
        </div>
    }
}

#[cfg(test)]
mod log_tests {
    #[test]
    fn transcript_lines_split_into_parts() {
        assert_eq!(
            super::log_line("[2026-11-03 08:55:29] ann (User): hi: there"),
            ("2026-11-03 08:55:29", "ann", "User", "hi: there")
        );
        assert_eq!(super::log_line("odd"), ("", "", "", "odd"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_stats_texts() {
        assert_eq!(ago(10.0), "today");
        assert_eq!(ago(86400.0 * 1.5), "yesterday");
        assert_eq!(ago(86400.0 * 45.0), "45 days ago");
        assert_eq!(trend(0, 0).0, "–");
        assert_eq!(trend(4, 0).0, "new");
        assert_eq!(trend(14, 10), ("+40 %".into(), "warn"));
        assert_eq!(trend(5, 10).0, "-50 %");
    }

    #[test]
    fn csv_quotes_cells_and_defuses_formulas() {
        let rows = [
            json!({"a": "say \"hi\"", "b": 3, "c": null}),
            json!({"a": "=HYPERLINK(1)", "b": true}),
        ];
        assert_eq!(
            to_csv(&["a", "b", "c"], &rows),
            "a,b,c\r\n\"say \"\"hi\"\"\",\"3\",\"\"\r\n\"'=HYPERLINK(1)\",\"true\",\"\"\r\n"
        );
    }
}
