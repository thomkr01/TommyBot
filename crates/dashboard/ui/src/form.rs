use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

/// A number segment indexes an array (`custom.0.roles`).
fn get_path(v: &Value, p: &str) -> Value {
    p.split('.').fold(v, |c, k| match (c, k.parse::<usize>()) { (Value::Array(a), Ok(i)) => a.get(i).unwrap_or(&Value::Null), _ => &c[k] }).clone()
}

/// Writes `x` at the dotted path, creating (or replacing non-object) intermediate objects.
fn set_path(v: &mut Value, p: &str, x: Value) {
    let mut c = v;
    for k in p.split('.') {
        if let (Some(a), Ok(i)) = (c.as_array().map(Vec::len), k.parse::<usize>()) {
            if i < a { c = &mut c[i]; continue; }
        }
        if !c.is_object() { *c = json!({}); }
        let Some(o) = c.as_object_mut() else { return };
        c = o.entry(k).or_insert(Value::Null);
    }
    *c = x;
}

/// (name, seconds) for the duration unit switcher.
const UNITS: [(&str, f64); 4] = [("second", 1.0), ("minute", 60.0), ("hour", 3600.0), ("day", 86400.0)];

fn best_unit(secs: f64) -> usize {
    UNITS.iter().rposition(|(_, m)| secs >= *m && secs % *m == 0.0).unwrap_or(0)
}

pub fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn show(v: &Value) -> String {
    match v { Value::Null => String::new(), Value::String(s) => s.clone(), o => o.to_string() }
}

/// Display name for a channel/role id (falls back to the id).
pub fn name_of(dc: &Value, key: &str, id: &str) -> String {
    items(dc, key).into_iter().find(|(i, _)| i == id).map_or_else(|| id.to_string(), |(_, l)| l)
}

/// Wick's wording for the stored option values; anything else just gets a capital letter.
fn opt_label(v: &str) -> String {
    match v {
        "Nothing" => "Keep Unverified".into(),
        "SuspiciousOnly" => "Suspicious".into(),
        "Dm" => "DMs".into(),
        "Web" => "Website".into(),
        "Button" => "Instant Access".into(),
        "ThirdParty" => "Third-party captcha".into(),
        "Captcha" => "TommyBot Captcha (Channel)".into(),
        "SimpleCaptcha" => "Simple Captcha (Channel)".into(),
        _ => v.chars().next().map_or(String::new(), |c| c.to_uppercase().chain(v.chars().skip(1)).collect()),
    }
}

/// What the trigger action does, for the help text under an automod filter's action.
fn action_desc(a: &str) -> &'static str {
    match a {
        "timeout" => "Discord's native timeout feature used to prevent the member from sending messages or joining voice channels",
        "warn" => "issuing a warn to the member triggering this filter, multiple warns lead to an actual punishment",
        "kick" => "Discord's native kick feature",
        "ban" => "Discord's native ban feature",
        "delete" => "deleting the message",
        _ => "left to the strike system",
    }
}

/// (anti-nuke action, verb, things, per-second noun) for the limit sentences.
const NUKE_WORDS: [(&str, &str, &str, &str); 7] = [
    ("Ban", "kick/ban", "members", "removal"),
    ("RoleCreate", "create", "roles", "creation"), ("RoleDelete", "delete", "roles", "deletion"),
    ("ChannelCreate", "create", "channels", "creation"), ("ChannelDelete", "delete", "channels", "deletion"),
    ("WebhookCreate", "create", "webhooks", "creation"), ("WebhookDelete", "delete", "webhooks", "deletion"),
];

/// (id, label) pairs from `/discord` for `key`: "channels" (no categories), "categories" or "roles".
fn items(dc: &Value, key: &str) -> Vec<(String, String)> {
    let (list, pre) = match key { "roles" => ("roles", "@"), "categories" => ("channels", ""), _ => ("channels", "#") };
    dc[list].as_array().map(|a| a.iter().filter(|x| list == "roles" || (x["kind"] == "category") == (key == "categories"))
        .map(|x| (x["id"].as_str().unwrap_or("").to_string(), format!("{pre}{}", x["name"].as_str().unwrap_or("?")))).collect()).unwrap_or_default()
}

/// Wick's member select: chips of the picked users, a search box that also takes a pasted ID.
#[component]
fn UserPick(get: Signal<Value>, set: Callback<Value>) -> impl IntoView {
    let g = StoredValue::new(crate::hash().into_iter().next().unwrap_or_default());
    let names = RwSignal::new(std::collections::HashMap::<String, String>::new());
    let found = RwSignal::new(Vec::<(String, String)>::new());
    let learn = move |v: &Value| { let l: Vec<(String, String)> = v.as_array().into_iter().flatten().map(|m| (show(&m["id"]), show(&m["name"]))).collect(); names.update(|n| n.extend(l.clone())); l };
    let ids = strs(&get.get_untracked());
    if !ids.is_empty() {
        spawn_local(async move { if let Ok(v) = api("GET", &format!("/api/guilds/{}/members?ids={}", g.get_value(), ids.join(",")), None).await { learn(&v); } });
    }
    let add = move |id: String| { let mut cur = strs(&get.get_untracked()); if !cur.contains(&id) { cur.push(id); } set.run(json!(cur)); found.set(vec![]); };
    let search = move |q: String| {
        let q = q.trim().to_string();
        if q.len() >= 17 && q.bytes().all(|b| b.is_ascii_digit()) { return add(q); }
        if q.is_empty() { return found.set(vec![]); }
        spawn_local(async move { if let Ok(v) = api("GET", &format!("/api/guilds/{}/members?q={}", g.get_value(), js_sys::encode_uri_component(&q)), None).await { found.set(learn(&v)); } });
    };
    view! { <div class="pick">
        {move || strs(&get.get()).into_iter().map(|id| {
            let label = names.with(|n| n.get(&id).cloned()).unwrap_or_else(|| id.clone());
            view! { <span class="chip">{label}" "<a aria-label="remove" on:click=move |_| { let mut cur = strs(&get.get_untracked()); cur.retain(|x| *x != id); set.run(json!(cur)) }>"×"</a></span> }
        }).collect_view()}
        <input placeholder="Search a member or paste an ID" on:change=move |e| search(event_target_value(&e)) />
        {move || found.get().into_iter().map(|(id, name)| view! { <button class="ghost" on:click=move |_| add(id.clone())>{name}</button> }).collect_view()}
    </div> }
}

#[component]
pub fn Field(f: Value, val: RwSignal<Value>, dc: RwSignal<Value>) -> impl IntoView {
    let path = f["path"].as_str().unwrap_or("").to_string();
    let kind = f["kind"].as_str().unwrap_or("text").to_string();
    let p = StoredValue::new(path.clone());
    let default = StoredValue::new(f["default"].clone());
    let get = move || { let v = get_path(&val.get(), &p.get_value()); if v.is_null() { default.get_value() } else { v } };
    let set = move |x: Value| val.update(|v| set_path(v, &p.get_value(), x));
    let tail = path.rsplit('.').next().unwrap_or("").to_string();
    let dur = kind == "int" && (tail.ends_with("_secs") || tail.ends_with("_ms"));
    let input = match kind.as_str() {
        "bool" => view! { <input type="checkbox" class="sw" prop:checked=move || get().as_bool().unwrap_or(false) on:change=move |e| set(json!(event_target_checked(&e))) /> }.into_any(),
        "int" if dur => {
            let per = if tail.ends_with("_ms") { 0.001 } else { 1.0 }; // seconds per stored unit
            let secs = move || get().as_f64().unwrap_or(0.0) * per;
            let chosen = RwSignal::new(None::<usize>);
            let unit = move || chosen.get().unwrap_or_else(|| best_unit(secs()));
            let set_n = move |n: f64| {
                let v = (n.max(0.0) * UNITS[unit()].1 / per).round() as i64;
                set(if v == 0 && default.get_value().is_null() { Value::Null } else { json!(v) });
            };
            view! { <div class="step"><button class="ghost" aria-label="less" on:click=move |_| set_n(secs() / UNITS[unit()].1 - 1.0)>"-"</button>
                <input prop:value=move || format!("{}", secs() / UNITS[unit()].1) on:change=move |e| { if let Ok(x) = event_target_value(&e).trim().parse::<f64>() { set_n(x) } } />
                <button class="ghost" aria-label="more" on:click=move |_| set_n(secs() / UNITS[unit()].1 + 1.0)>"+"</button>
                <select aria-label="unit" on:change=move |e| chosen.set(UNITS.iter().position(|(u, _)| *u == event_target_value(&e)))>
                    {UNITS.iter().map(|(u, _)| view! { <option value=*u selected=move || UNITS[unit()].0 == *u>{*u}</option> }).collect_view()}
                </select></div>
                <span class="chip">{move || { let s = secs(); let u = best_unit(s); let n = s / UNITS[u].1; format!("{n} {}{}", UNITS[u].0, if n == 1.0 { "" } else { "s" }) }}</span> }.into_any()
        }
        "int" | "float" => {
            let float = kind == "float";
            let step = move |d: f64| {
                let x = get().as_f64().unwrap_or(0.0) + d;
                set(if float { json!(x) } else { json!(x as i64) })
            };
            let unit = match path.rsplit('.').next().unwrap_or("") { n if n.ends_with("_secs") => "seconds", n if n.ends_with("_ms") => "ms", n if n.ends_with("_days") => "days", n if n.ends_with("points") => "Points", n if n.ends_with("_pct") || n == "heat" => "%", "per_minute" => "/min", "per_hour" => "/hour", "width" | "height" | "font_size" => "Pixels", "length" => "Characters", _ => "" };
            view! { <div class="step"><button class="ghost" on:click=move |_| step(-1.0)>"-"</button>
                <input prop:value=move || show(&get()) on:change=move |e| {
                    let s = event_target_value(&e);
                    let s = s.trim();
                    if s.is_empty() { set(Value::Null) }
                    else if float { if let Ok(x) = s.parse::<f64>() { set(json!(x)) } }
                    else if let Ok(x) = s.parse::<i64>() { set(json!(x)) }
                } />
                <button class="ghost" on:click=move |_| step(1.0)>"+"</button>{(!unit.is_empty()).then(|| view! { <span>{unit}</span> })}</div> }.into_any()
        }
        "list" | "lines" => {
            let box_ = if kind == "lines" {
                view! { <textarea style="min-height:80px" prop:value=move || strs(&get()).join("\n") on:change=move |e| set(json!(event_target_value(&e).lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>())) /> }.into_any()
            } else {
                view! { <input prop:value=move || csv(&get()) on:change=move |e| set(from_csv(&event_target_value(&e))) /> }.into_any()
            };
            let export = move || format!("data:text/plain;charset=utf-8,{}", js_sys::encode_uri_component(&strs(&get()).join("\n")));
            // Merges a text file (one entry per line, `#` comments skipped) into the list.
            let import = move |e: leptos::ev::Event| {
                let Some(file) = event_target::<web_sys::HtmlInputElement>(&e).files().and_then(|l| l.get(0)) else { return };
                spawn_local(async move {
                    let Ok(text) = wasm_bindgen_futures::JsFuture::from(file.text()).await else { return };
                    let mut cur = strs(&get());
                    for l in text.as_string().unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                        if !cur.iter().any(|c| c == l) { cur.push(l.to_string()) }
                    }
                    set(json!(cur));
                });
            };
            view! { <div style="flex:1">{box_}<div class="io">
                <a class="ghost" download=format!("{tail}.txt") href=export>"Export"</a>
                <label class="ghost">"Import"<input type="file" accept=".txt,text/plain" hidden=true on:change=import /></label>
            </div></div> }.into_any()
        }
        "color" => view! { <input type="color" prop:value=move || show(&get()) on:change=move |e| set(json!(event_target_value(&e))) /> }.into_any(),
        "choice" => {
            let opts = f["options"].as_array().cloned().unwrap_or_default();
            let none = f["none_label"].as_str().unwrap_or("off").to_string();
            view! { <select on:change=move |e| { let s = event_target_value(&e); set(if s.is_empty() { Value::Null } else { json!(s) }) }>
                {opts.iter().map(|o| { let v = show(o); let l = if v.is_empty() { none.clone() } else { opt_label(&v) };
                    view! { <option value=v.clone() selected=move || show(&get()) == v>{l}</option> } }).collect_view()}
            </select> }.into_any()
        }
        "form" => {
            let upd = move |i: usize, k: &'static str, x: Value| { let mut cur = get(); cur[i][k] = x; set(cur) };
            let count = move || get().as_array().map_or(0, Vec::len);
            view! { <div class="pick">
                <div class="row">
                    <Show when=move || count() < 6>
                        <button on:click=move |_| { let mut cur = get().as_array().cloned().unwrap_or_default(); cur.push(json!({"label": "New question", "long": false, "required": true})); set(json!(cur)) }>"Add new field"</button>
                    </Show>
                    <button class="danger" on:click=move |_| set(json!([]))>"Delete all fields"</button>
                </div>
                {move || (0..count()).map(|i| view! { <div class="row">
                    <input prop:value=move || get()[i]["label"].as_str().unwrap_or("").to_string() on:change=move |e| upd(i, "label", json!(event_target_value(&e))) />
                    <Show when=move || get()[i]["required"].as_bool().unwrap_or(true)><b class="err">"*"</b></Show>
                    <span class="chip blue">{move || if get()[i]["long"].as_bool().unwrap_or(false) { "Textarea" } else { "Text" }}</span>
                    <Show when=move || get()[i]["label"] == "Appeal Message"><span class="chip">"default"</span></Show>
                    <label><input type="checkbox" prop:checked=move || get()[i]["long"].as_bool().unwrap_or(false) on:change=move |e| upd(i, "long", json!(event_target_checked(&e))) /> "long"</label>
                    <label><input type="checkbox" prop:checked=move || get()[i]["required"].as_bool().unwrap_or(true) on:change=move |e| upd(i, "required", json!(event_target_checked(&e))) /> "required"</label>
                    <button class="ghost" on:click=move |_| { let mut cur = get(); if let Some(a) = cur.as_array_mut() { a.remove(i); } set(cur) }>"Remove"</button>
                </div> }).collect_view()}
            </div> }.into_any()
        }
        "warn_actions" => {
            let upd = move |i: usize, k: &'static str, x: Value| { let mut cur = get(); cur[i][k] = x; set(cur) };
            let count = move || get().as_array().map_or(0, Vec::len);
            view! { <div class="pick">
                <div class="row">
                    <button on:click=move |_| { let mut cur = get().as_array().cloned().unwrap_or_default(); cur.push(json!({"points": 10, "action": "timeout", "secs": 3600})); set(json!(cur)) }>"Add new action"</button>
                    <button class="danger" on:click=move |_| set(json!([]))>"Delete all actions"</button>
                </div>
                <Show when=move || count() == 0><span class="muted">"No Custom Actions"</span></Show>
                {move || (0..count()).map(|i| view! { <div class="row">
                    <input aria-label="points" prop:value=move || show(&get()[i]["points"]) on:change=move |e| { if let Ok(x) = event_target_value(&e).trim().parse::<i64>() { upd(i, "points", json!(x)) } } /><span>"points"</span>
                    <select aria-label="action" on:change=move |e| upd(i, "action", json!(event_target_value(&e)))>
                        {["timeout", "kick", "ban"].map(|a| view! { <option value=a selected=move || get()[i]["action"] == a>{a}</option> }).collect_view()}
                    </select>
                    <Show when=move || get()[i]["action"] == "timeout">
                        <input aria-label="seconds" prop:value=move || show(&get()[i]["secs"]) on:change=move |e| { if let Ok(x) = event_target_value(&e).trim().parse::<i64>() { upd(i, "secs", json!(x)) } } /><span>"seconds"</span>
                    </Show>
                    <button class="ghost" on:click=move |_| { let mut cur = get(); if let Some(a) = cur.as_array_mut() { a.remove(i); } set(cur) }>"Remove"</button>
                </div> }).collect_view()}
            </div> }.into_any()
        }
        "customs" => {
            let opts = f["options"].as_array().cloned().unwrap_or_default();
            view! { <div class="pick">
                {move || (0..get().as_array().map_or(0, Vec::len)).map(|i| {
                    let boxes = opts.iter().map(|o| {
                        let k = show(o);
                        let label = k.replace('_', " ");
                        let k2 = k.clone();
                        view! { <label><input type="checkbox" prop:checked=move || strs(&get()[i]["events"]).contains(&k) on:change=move |e| {
                            let mut cur = get();
                            let mut ev = strs(&cur[i]["events"]);
                            ev.retain(|x| *x != k2);
                            if event_target_checked(&e) { ev.push(k2.clone()); }
                            cur[i]["events"] = json!(ev);
                            set(cur);
                        } /> {label}</label> }
                    }).collect_view();
                    view! { <div class="row">
                        <select aria-label="channel" on:change=move |e| { let mut cur = get(); cur[i]["channel"] = json!(event_target_value(&e)); set(cur) }>
                            <option value="" selected=move || get()[i]["channel"].is_null()>"choose a channel"</option>
                            {move || items(&dc.get(), "channels").into_iter().map(|(id, l)| view! { <option value=id.clone() selected=move || get()[i]["channel"].as_str() == Some(id.as_str())>{l}</option> }).collect_view()}
                        </select>
                        <div class="pick">{boxes}</div>
                        <button class="ghost" on:click=move |_| { let mut cur = get(); if let Some(a) = cur.as_array_mut() { a.remove(i); } set(cur) }>"Remove"</button>
                    </div> }
                }).collect_view()}
                <button class="ghost" on:click=move |_| { let mut cur = get(); if !cur.is_array() { cur = json!([]); } if let Some(a) = cur.as_array_mut() { a.push(json!({"events": [], "channel": null})); } set(cur) }>"Add a new logging type"</button>
            </div> }.into_any()
        }
        "channel" | "role" => {
            let key = if kind == "channel" { "channels" } else { "roles" };
            view! { <select on:change=move |e| { let s = event_target_value(&e); set(if s.is_empty() { Value::Null } else { json!(s) }) }>
                <option value="" selected=move || get().is_null()>"none"</option>
                {move || items(&dc.get(), key).into_iter().map(|(id, l)| view! { <option value=id.clone() selected=move || get().as_str() == Some(id.as_str())>{l}</option> }).collect_view()}
            </select> }.into_any()
        }
        "users" => view! { <UserPick get=Signal::derive(get) set=Callback::new(set) /> }.into_any(),
        "channels" | "categories" | "roles" => {
            let key = StoredValue::new(kind.clone());
            view! { <div class="pick">
                {move || items(&dc.get(), &key.get_value()).into_iter().map(|(id, l)| {
                    let id2 = id.clone();
                    view! { <label><input type="checkbox" prop:checked=move || strs(&get()).contains(&id) on:change=move |e| {
                        let mut cur = strs(&get());
                        cur.retain(|x| *x != id2);
                        if event_target_checked(&e) { cur.push(id2.clone()); }
                        set(json!(cur));
                    } /> {l}</label> }
                }).collect_view()}
            </div> }.into_any()
        }
        _ => view! { <input prop:value=move || show(&get()) on:change=move |e| { let s = event_target_value(&e); set(if s.is_empty() { Value::Null } else { json!(s) }) } /> }.into_any(),
    };
    let help = f["help"].as_str().map(|h| view! { <span class="muted">{h.to_string()}</span> });
    let helper = f["helper"].is_object().then(|| {
        let h = f["helper"].clone();
        // Heat weights live at `<section>.heat.<w>`; max and decay are their siblings, the strike duration their cousin.
        let heat = path.rsplit_once('.').map_or(String::new(), |(p, _)| p.to_string());
        let strike = heat.rsplit_once('.').map_or(String::new(), |(p, _)| format!("{p}.strike.strike_timeout_ms"));
        view! { <span class="muted">{move || {
            let v = val.get();
            let num = |p: &str, d: &Value| get_path(&v, p).as_f64().or(d.as_f64()).unwrap_or(0.0);
            let (max, decay) = (num(&format!("{heat}.max"), &h["max"]), num(&format!("{heat}.decay_per_sec"), &h["decay"]));
            let w = get().as_f64().unwrap_or(0.0);
            let secs = num(&strike, &h["strike_ms"]) / 1000.0;
            let u = best_unit(secs);
            let n = if w > 0.0 { (max * (1.0 - decay) / w).max(1.0) } else { f64::INFINITY };
            format!("A member approximately needs to send {n:.2} {} before getting timed out for {} {}(s), the Auto Timeouts regular strike duration.", h["noun"].as_str().unwrap_or("messages"), secs / UNITS[u].1, UNITS[u].0)
        }}</span> }
    });
    let nuke = path.strip_prefix("antinuke.limits.").and_then(|r| r.split_once('.')).and_then(|(a, k)| NUKE_WORDS.iter().find(|w| w.0 == a).map(|w| (w, k == "per_hour"))).filter(|_| tail == "per_minute" || tail == "per_hour").map(|(w, hour)| view! { <span class="muted">{move || {
        let n = get().as_f64().unwrap_or(0.0);
        let (span, per, div) = if hour { ("1 hour", "minute", 60.0) } else { ("60 seconds", "second", 60.0) };
        format!("A staff member needs to {} {n} {} in less than {span}, That's an average of {:.2} {} every single {per} that they need to stay under if they want TommyBot not coming after them.", w.1, w.2, n / div, w.3)
    }}</span> });
    let action = (path.starts_with("automod.filters.") && tail == "action").then(|| view! { <span class="muted">{move || {
        let a = get();
        let a = a.as_str().unwrap_or("");
        format!("Trigger Action represents what TommyBot does once this filter triggers and the total heat accumulated for the member surpasses the total allowed heat. {} is {}.", if a.is_empty() { "strike system" } else { a }, action_desc(a))
    }}</span> });
    let label = f["title"].as_str().map_or_else(|| tail.replace('_', " "), String::from);
    view! { <div class="row" class:tog=kind == "bool"><label>{label}</label>{input}{help}{helper}{nuke}{action}</div> }
}

/// Raw JSON of one section of the config in `val`.
fn raw_editor(val: RwSignal<Value>, name: String, msg: RwSignal<String>) -> impl IntoView {
    let n = StoredValue::new(name);
    view! { <textarea prop:value=move || serde_json::to_string_pretty(&val.get()[n.get_value()]).unwrap_or_default()
        on:change=move |e| { match serde_json::from_str::<Value>(&event_target_value(&e)) { Ok(v) => val.update(|c| c[n.get_value()] = v), Err(_) => msg.set("Not valid JSON".into()) } }></textarea> }
}

/// Schema-driven form for the sections the backend describes; raw JSON stays available underneath.
/// Fields of other sections (`"section"` in the schema) are shown and saved alongside.
#[component]
pub fn Section(
    guild: String,
    name: String,
    #[prop(into, default = String::new())] part: String,
    enrolled: ReadSignal<bool>,
    /// Wizard mode: these cards one at a time instead of the grouped form.
    #[prop(optional)] cards: Option<&'static [crate::wizard::Card]>,
    /// Called by the forward arrow on the last card.
    #[prop(optional)] next: Option<Callback<()>>,
) -> impl IntoView {
    // The whole config, so fields of other sections share one form and one save.
    let val = RwSignal::new(json!({}));
    let version = RwSignal::new(0i64);
    let dc = RwSignal::new(json!({}));
    let fields = RwSignal::new(Vec::<Value>::new());
    let orig = RwSignal::new(json!({}));
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let (g, n) = (guild.clone(), name.clone());
    spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/config"), None).await {
            Ok(v) => {
                version.set(v["version"].as_i64().unwrap_or(0));
                let mut c = if v["config"].is_object() { v["config"].clone() } else { json!({}) };
                if !c[&n].is_object() { c[&n] = json!({}); }
                val.set(c);
                orig.set(val.get_untracked());
            }
            Err(e) => msg.set(e),
        }
        // Missing schema/discord data degrades to the raw editor.
        if let Ok(s) = api("GET", &format!("/api/guilds/{g}/schema"), None).await {
            if let Some(sec) = s.as_array().and_then(|a| a.iter().find(|x| x["name"] == n.as_str())) {
                // `part` picks the dashboard pages of the section: "" all fields, else a comma list where "general" means the fields without a page.
                let keep = |f: &Value| part.is_empty() || part.split(',').any(|p| if p == "general" { f["page"].is_null() } else { f["page"] == p });
                fields.set(sec["fields"].as_array().into_iter().flatten().filter(|f| keep(f)).map(|f| {
                    let mut f = f.clone();
                    f["path"] = json!(format!("{}.{}", f["section"].as_str().unwrap_or(&n), f["path"].as_str().unwrap_or("")));
                    f
                }).collect());
                if let Ok(d) = api("GET", &format!("/api/guilds/{g}/discord"), None).await { dc.set(d); }
            }
        }
    });
    let n = StoredValue::new(name);
    let reload = {
        let g = guild.clone();
        move |v: i64| { let g = g.clone(); version.set(v); spawn_local(async move {
            if let Ok(c) = api("GET", &format!("/api/guilds/{g}/config"), None).await { val.set(c["config"].clone()); orig.set(c["config"].clone()); }
        }) }
    };
    let card_view = cards.map(|cs| {
        let at = RwSignal::new(0usize);
        let g = guild.clone();
        let act = move |path: &'static str| {
            let (g, v, reload) = (g.clone(), version.get_untracked(), reload.clone());
            run_act(async move { api("POST", &format!("/api/guilds/{g}/{path}"), Some(json!({"expected_version": v}))).await }, msg, need, move |r| { msg.set("Done".into()); reload(r["version"].as_i64().unwrap_or(0)) });
        };
        view! { <div class="card wiz">
            <button class="nav back" aria-label="previous" disabled=move || at.get() == 0 on:click=move |_| at.update(|i| *i = i.saturating_sub(1))>"‹"</button>
            <div class="wizbody">
                {move || { let c = &cs[at.get()]; let all = fields.get();
                    let fs: Vec<Value> = c.fields.iter().filter_map(|(p, l)| all.iter().find(|f| f["path"] == *p).map(|f| { let mut f = f.clone(); if !l.is_empty() { f["title"] = json!(l); } f })).collect();
                    let act = act.clone();
                    view! {
                        <h2>{c.title}</h2><p class="muted">{c.desc}</p>
                        {c.notes.iter().map(|t| view! { <p class="note">{*t}</p> }).collect_view()}
                        <div class="split"><div>{fs.into_iter().map(|f| view! { <Field f val dc /> }).collect_view()}
                            {c.action.map(|(label, path)| view! { <button class="go" disabled=move || val.get() != orig.get() on:click=move |_| act(path)>{label}</button> })}</div>
                            <div class="preview">{move || (cs[at.get_untracked()].preview)(&val.get(), &dc.get())}</div></div>
                    } }}
                <p class="muted">{move || format!("{} / {}", at.get() + 1, cs.len())}</p>
            </div>
            <button class="nav" aria-label="next" on:click=move |_| if at.get() + 1 < cs.len() { at.update(|i| *i += 1) } else if let Some(n) = next { n.run(()) }>"›"</button>
        </div> }
    });
    let save = move |_| {
        let (g, cur, before) = (guild.clone(), val.get_untracked(), orig.get_untracked());
        orig.set(cur.clone());
        let mut names = vec![n.get_value()];
        names.extend(fields.get_untracked().iter().filter_map(|f| f["section"].as_str().map(String::from)));
        names.sort();
        names.dedup();
        run_save(async move {
            let mut last = json!({"version": version.get_untracked()});
            for s in names.iter().filter(|s| cur[s.as_str()] != before[s.as_str()]) {
                let body = json!({"expected_version": last["version"], "value": cur[s.as_str()]});
                last = api("PUT", &format!("/api/guilds/{g}/config/{s}"), Some(body)).await?;
            }
            Ok(last)
        }, version, msg, need);
    };
    view! {
        {card_view}
        <Show when=move || cards.is_none() && !fields.get().is_empty() fallback=move || cards.is_none().then(|| raw_editor(val, n.get_value(), msg))>
            <div class="cards">
                {move || {
                    // One card per path prefix ("limits.ban" -> "limits ban"); unprefixed fields go under "General".
                    let mut groups: Vec<(String, Vec<Value>)> = vec![];
                    for f in fields.get() {
                        let p = f["path"].as_str().unwrap_or("").split_once('.').map_or("", |(_, p)| p);
                        let g = f["group"].as_str().map_or_else(|| p.rsplit_once('.').map_or("General".to_string(), |(g, _)| g.replace(['.', '_'], " ")), String::from);
                        match groups.iter_mut().find(|(k, _)| *k == g) { Some((_, v)) => v.push(f), None => groups.push((g, vec![f])) }
                    }
                    // A system that is switched off rings its cards red (the card holding the switch stays plain).
                    let sec = n.get_value();
                    let off = val.get()[&sec]["enabled"] == json!(false);
                    let switch = format!("{sec}.enabled");
                    groups.into_iter().map(|(g, fs)| { let ring = off && !fs.iter().any(|f| f["path"] == switch.as_str()); view! { <div class="card" class:off=ring><h2>{g}</h2>{fs.into_iter().map(|f| view! { <Field f val dc /> }).collect_view()}{ring.then(|| view! { <details class="notes"><summary><span class="bell">"!"</span>"Notes"</summary><p>{format!("Your whole {sec} is disabled, the settings of this card have no effect.")}</p></details> })}</div> } }).collect_view()
                }}
            </div>
            <details><summary>"Advanced (raw JSON)"</summary>
                {raw_editor(val, n.get_value(), msg)}
            </details>
        </Show>
        <Show when=move || val.get() != orig.get()>
            <div class="bar"><b>"Changes detected!"</b><button class="ghost" on:click=move |_| val.set(orig.get_untracked())>"Cancel"</button><button on:click=save.clone()>"Save"</button></div>
        </Show>
        <Show when=move || !msg.get().is_empty()><p class="toast" on:click=move |_| msg.set(String::new())>{move || msg.get()}</p></Show>
        <StepUp need enrolled />
    }
}
