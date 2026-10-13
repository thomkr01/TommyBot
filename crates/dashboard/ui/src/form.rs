use crate::common::*;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};

fn get_path(v: &Value, p: &str) -> Value {
    p.split('.').fold(v, |c, k| &c[k]).clone()
}

/// Writes `x` at the dotted path, creating (or replacing non-object) intermediate objects.
fn set_path(v: &mut Value, p: &str, x: Value) {
    let mut c = v;
    for k in p.split('.') {
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

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn show(v: &Value) -> String {
    match v { Value::Null => String::new(), Value::String(s) => s.clone(), o => o.to_string() }
}

/// Display name for a channel/role id (falls back to the id).
pub fn name_of(dc: &Value, key: &str, id: &str) -> String {
    items(dc, key).into_iter().find(|(i, _)| i == id).map_or_else(|| id.to_string(), |(_, l)| l)
}

/// (id, label) pairs from `/discord` for `key` ("channels" or "roles").
fn items(dc: &Value, key: &str) -> Vec<(String, String)> {
    let pre = if key == "channels" { "#" } else { "@" };
    dc[key].as_array().map(|a| a.iter().map(|x| (x["id"].as_str().unwrap_or("").to_string(), format!("{pre}{}", x["name"].as_str().unwrap_or("?")))).collect()).unwrap_or_default()
}

#[component]
fn Field(f: Value, val: RwSignal<Value>, dc: RwSignal<Value>) -> impl IntoView {
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
            let unit = match path.rsplit('.').next().unwrap_or("") { n if n.ends_with("_secs") => "seconds", n if n.ends_with("_ms") => "ms", n if n.ends_with("_days") => "days", n if n.ends_with("points") => "points", "per_minute" => "/min", "per_hour" => "/hour", _ => "" };
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
        "list" => view! { <input prop:value=move || csv(&get()) on:change=move |e| set(from_csv(&event_target_value(&e))) /> }.into_any(),
        "choice" => {
            let opts = f["options"].as_array().cloned().unwrap_or_default();
            let none = f["none_label"].as_str().unwrap_or("off").to_string();
            view! { <select on:change=move |e| { let s = event_target_value(&e); set(if s.is_empty() { Value::Null } else { json!(s) }) }>
                {opts.iter().map(|o| { let v = show(o); let l = if v.is_empty() { none.clone() } else { v.clone() };
                    view! { <option value=v.clone() selected=move || show(&get()) == v>{l}</option> } }).collect_view()}
            </select> }.into_any()
        }
        "form" => {
            let upd = move |i: usize, k: &'static str, x: Value| { let mut cur = get(); cur[i][k] = x; set(cur) };
            view! { <div class="pick">
                {move || (0..get().as_array().map_or(0, Vec::len)).map(|i| view! { <div class="row">
                    <input prop:value=move || get()[i]["label"].as_str().unwrap_or("").to_string() on:change=move |e| upd(i, "label", json!(event_target_value(&e))) />
                    <label><input type="checkbox" prop:checked=move || get()[i]["long"].as_bool().unwrap_or(false) on:change=move |e| upd(i, "long", json!(event_target_checked(&e))) /> "long"</label>
                    <label><input type="checkbox" prop:checked=move || get()[i]["required"].as_bool().unwrap_or(true) on:change=move |e| upd(i, "required", json!(event_target_checked(&e))) /> "required"</label>
                    <button class="ghost" on:click=move |_| { let mut cur = get(); if let Some(a) = cur.as_array_mut() { a.remove(i); } set(cur) }>"Remove"</button>
                </div> }).collect_view()}
                <Show when=move || get().as_array().map_or(0, Vec::len) < 6>
                    <button class="ghost" on:click=move |_| { let mut cur = get(); if !cur.is_array() { cur = json!([]); } if let Some(a) = cur.as_array_mut() { a.push(json!({"label": "New question", "long": false, "required": true})); } set(cur) }>"Add field"</button>
                </Show>
            </div> }.into_any()
        }
        "channel" | "role" => {
            let key = if kind == "channel" { "channels" } else { "roles" };
            view! { <select on:change=move |e| { let s = event_target_value(&e); set(if s.is_empty() { Value::Null } else { json!(s) }) }>
                <option value="" selected=move || get().is_null()>"none"</option>
                {move || items(&dc.get(), key).into_iter().map(|(id, l)| view! { <option value=id.clone() selected=move || get().as_str() == Some(id.as_str())>{l}</option> }).collect_view()}
            </select> }.into_any()
        }
        "channels" | "roles" => {
            let key = if kind == "channels" { "channels" } else { "roles" };
            view! { <div class="pick">
                {move || items(&dc.get(), key).into_iter().map(|(id, l)| {
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
    let label = f["title"].as_str().map_or_else(|| tail.replace('_', " "), String::from);
    view! { <div class="row" class:tog=kind == "bool"><label>{label}</label>{input}{help}</div> }
}

fn raw_editor(val: RwSignal<Value>, msg: RwSignal<String>) -> impl IntoView {
    view! { <textarea prop:value=move || serde_json::to_string_pretty(&val.get()).unwrap_or_default()
        on:change=move |e| { match serde_json::from_str::<Value>(&event_target_value(&e)) { Ok(v) => val.set(v), Err(_) => msg.set("Not valid JSON".into()) } }></textarea> }
}

/// Schema-driven form for the sections the backend describes; raw JSON stays available underneath.
#[component]
pub fn Section(guild: String, name: String, #[prop(into, default = String::new())] part: String, enrolled: ReadSignal<bool>) -> impl IntoView {
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
                val.set(if v["config"][&n].is_object() { v["config"][&n].clone() } else { json!({}) });
                orig.set(val.get_untracked());
            }
            Err(e) => msg.set(e),
        }
        // Missing schema/discord data degrades to the raw editor.
        if let Ok(s) = api("GET", &format!("/api/guilds/{g}/schema"), None).await {
            if let Some(sec) = s.as_array().and_then(|a| a.iter().find(|x| x["name"] == n.as_str())) {
                // `part` picks the dashboard pages of the section: "" all fields, else a comma list where "general" means the fields without a page.
                let keep = |f: &Value| part.is_empty() || part.split(',').any(|p| if p == "general" { f["page"].is_null() } else { f["page"] == p });
                fields.set(sec["fields"].as_array().into_iter().flatten().filter(|f| keep(f)).cloned().collect());
                if let Ok(d) = api("GET", &format!("/api/guilds/{g}/discord"), None).await { dc.set(d); }
            }
        }
    });
    let save = move |_| {
        let (g, n, v, value) = (guild.clone(), name.clone(), version.get_untracked(), val.get_untracked());
        orig.set(value.clone());
        run_save(async move { api("PUT", &format!("/api/guilds/{g}/config/{n}"), Some(json!({"expected_version": v, "value": value}))).await }, version, msg, need);
    };
    view! {
        <Show when=move || !fields.get().is_empty() fallback=move || raw_editor(val, msg)>
            <div class="cards">
                {move || {
                    // One card per path prefix ("limits.ban" -> "limits ban"); unprefixed fields go under "General".
                    let mut groups: Vec<(String, Vec<Value>)> = vec![];
                    for f in fields.get() {
                        let p = f["path"].as_str().unwrap_or("");
                        let g = f["group"].as_str().map_or_else(|| p.rsplit_once('.').map_or("General".to_string(), |(g, _)| g.replace(['.', '_'], " ")), String::from);
                        match groups.iter_mut().find(|(k, _)| *k == g) { Some((_, v)) => v.push(f), None => groups.push((g, vec![f])) }
                    }
                    // A system that is switched off rings its cards red (the card holding the switch stays plain).
                    let off = val.get()["enabled"] == json!(false);
                    groups.into_iter().map(|(g, fs)| { let ring = off && !fs.iter().any(|f| f["path"] == "enabled"); view! { <div class="card" class:off=ring><h2>{g}</h2>{fs.into_iter().map(|f| view! { <Field f val dc /> }).collect_view()}</div> } }).collect_view()
                }}
            </div>
            <details><summary>"Advanced (raw JSON)"</summary>
                {raw_editor(val, msg)}
            </details>
        </Show>
        <Show when=move || val.get() != orig.get()>
            <div class="bar"><b>"Changes detected!"</b><button class="ghost" on:click=move |_| val.set(orig.get_untracked())>"Cancel"</button><button on:click=save.clone()>"Save"</button></div>
        </Show>
        <p class="info">{move || msg.get()}</p>
        <StepUp need enrolled />
    }
}
