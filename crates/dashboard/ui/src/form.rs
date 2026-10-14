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

/// (id, label) pairs from `/discord` for `key` ("channels" or "roles").
fn items(dc: &Value, key: &str) -> Vec<(String, String)> {
    let pre = if key == "channels" { "#" } else { "@" };
    dc[key].as_array().map(|a| a.iter().map(|x| (x["id"].as_str().unwrap_or("").to_string(), format!("{pre}{}", x["name"].as_str().unwrap_or("?")))).collect()).unwrap_or_default()
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
            let unit = match path.rsplit('.').next().unwrap_or("") { n if n.ends_with("_secs") => "seconds", n if n.ends_with("_ms") => "ms", n if n.ends_with("_days") => "days", n if n.ends_with("points") => "points", n if n.ends_with("_pct") || n == "heat" => "%", "per_minute" => "/min", "per_hour" => "/hour", "width" | "height" => "px", "length" => "characters", _ => "" };
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
        "color" => view! { <input type="color" prop:value=move || show(&get()) on:change=move |e| set(json!(event_target_value(&e))) /> }.into_any(),
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
    let label = f["title"].as_str().map_or_else(|| tail.replace('_', " "), String::from);
    view! { <div class="row" class:tog=kind == "bool"><label>{label}</label>{input}{help}{helper}</div> }
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
pub fn Section(guild: String, name: String, #[prop(into, default = String::new())] part: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    // The whole config, so fields of other sections share one form and one save.
    let val = RwSignal::new(json!({}));
    let version = RwSignal::new(0i64);
    let dc = RwSignal::new(json!({}));
    let fields = RwSignal::new(Vec::<Value>::new());
    let orig = RwSignal::new(json!({}));
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let gid0 = StoredValue::new(guild.clone());
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
        <Show when=move || !fields.get().is_empty() fallback=move || raw_editor(val, n.get_value(), msg)>
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
                    let gid = gid0.get_value();
                    groups.into_iter().map(|(g, fs)| { let ring = off && !fs.iter().any(|f| f["path"] == switch.as_str()); view! { <div class="card" class:off=ring><h2>{g}</h2>{fs.into_iter().map(|f| view! { <Field f val dc /> }).collect_view()}<a class="bell" href=format!("#/{gid}/logging") title="Alerts for this system go to your log channels" aria-label="alerts">"🔔"</a></div> } }).collect_view()
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
