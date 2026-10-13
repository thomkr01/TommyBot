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
    let get = move || get_path(&val.get(), &p.get_value());
    let set = move |x: Value| val.update(|v| set_path(v, &p.get_value(), x));
    let input = match kind.as_str() {
        "bool" => view! { <input type="checkbox" class="sw" prop:checked=move || get().as_bool().unwrap_or(false) on:change=move |e| set(json!(event_target_checked(&e))) /> }.into_any(),
        "int" | "float" => {
            let float = kind == "float";
            let step = move |d: f64| {
                let x = get().as_f64().unwrap_or(0.0) + d;
                set(if float { json!(x) } else { json!(x as i64) })
            };
            let unit = match path.rsplit('.').next().unwrap_or("") { n if n.ends_with("_secs") => "seconds", n if n.ends_with("_ms") => "ms", "per_minute" => "/min", "per_hour" => "/hour", _ => "" };
            view! { <div class="step"><button class="ghost" on:click=move |_| step(-1.0)>"-"</button>
                <input prop:value=move || show(&get()) on:change=move |e| {
                    let s = event_target_value(&e);
                    let s = s.trim();
                    if s.is_empty() { set(Value::Null) }
                    else if float { if let Ok(x) = s.parse::<f64>() { set(json!(x)) } }
                    else if let Ok(x) = s.parse::<i64>() { set(json!(x)) }
                } />
                <button class="ghost" on:click=move |_| step(1.0)>"+"</button><span>{unit}</span></div> }.into_any()
        }
        "list" => view! { <input prop:value=move || csv(&get()) on:change=move |e| set(from_csv(&event_target_value(&e))) /> }.into_any(),
        "choice" => {
            let opts = f["options"].as_array().cloned().unwrap_or_default();
            view! { <select on:change=move |e| { let s = event_target_value(&e); set(if s.is_empty() { Value::Null } else { json!(s) }) }>
                {opts.iter().map(|o| { let v = show(o); let l = if v.is_empty() { "off".to_string() } else { v.clone() };
                    view! { <option value=v.clone() selected=move || show(&get()) == v>{l}</option> } }).collect_view()}
            </select> }.into_any()
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
    let label = path.rsplit('.').next().unwrap_or(&path).replace('_', " ");
    view! { <div class="row" class:tog=kind == "bool"><label>{label}</label>{input}{help}</div> }
}

fn raw_editor(val: RwSignal<Value>, msg: RwSignal<String>) -> impl IntoView {
    view! { <textarea prop:value=move || serde_json::to_string_pretty(&val.get()).unwrap_or_default()
        on:change=move |e| { match serde_json::from_str::<Value>(&event_target_value(&e)) { Ok(v) => val.set(v), Err(_) => msg.set("Not valid JSON".into()) } }></textarea> }
}

/// Schema-driven form for the sections the backend describes; raw JSON stays available underneath.
#[component]
pub fn Section(guild: String, name: String, enrolled: ReadSignal<bool>) -> impl IntoView {
    let val = RwSignal::new(json!({}));
    let version = RwSignal::new(0i64);
    let dc = RwSignal::new(json!({}));
    let fields = RwSignal::new(Vec::<Value>::new());
    let (msg, need) = (RwSignal::new(String::new()), RwSignal::new(false));
    let (g, n) = (guild.clone(), name.clone());
    spawn_local(async move {
        match api("GET", &format!("/api/guilds/{g}/config"), None).await {
            Ok(v) => {
                version.set(v["version"].as_i64().unwrap_or(0));
                val.set(if v["config"][&n].is_object() { v["config"][&n].clone() } else { json!({}) });
            }
            Err(e) => msg.set(e),
        }
        // Missing schema/discord data degrades to the raw editor.
        if let Ok(s) = api("GET", &format!("/api/guilds/{g}/schema"), None).await {
            if let Some(sec) = s.as_array().and_then(|a| a.iter().find(|x| x["name"] == n.as_str())) {
                fields.set(sec["fields"].as_array().cloned().unwrap_or_default());
                if let Ok(d) = api("GET", &format!("/api/guilds/{g}/discord"), None).await { dc.set(d); }
            }
        }
    });
    let save = move |_| {
        let (g, n, v, value) = (guild.clone(), name.clone(), version.get_untracked(), val.get_untracked());
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
                        let g = p.rsplit_once('.').map_or("General".to_string(), |(g, _)| g.replace(['.', '_'], " "));
                        match groups.iter_mut().find(|(k, _)| *k == g) { Some((_, v)) => v.push(f), None => groups.push((g, vec![f])) }
                    }
                    groups.into_iter().map(|(g, fs)| view! { <div class="card"><h2>{g}</h2>{fs.into_iter().map(|f| view! { <Field f val dc /> }).collect_view()}</div> }).collect_view()
                }}
            </div>
            <details><summary>"Advanced (raw JSON)"</summary>
                {raw_editor(val, msg)}
            </details>
        </Show>
        <div class="row"><button on:click=save>"Save"</button><span>{move || msg.get()}</span></div>
        <StepUp need enrolled />
    }
}
