use gloo_net::http::Request;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};
use std::cell::RefCell;

thread_local! { pub static CSRF: RefCell<String> = const { RefCell::new(String::new()) }; }

pub async fn api(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let csrf = CSRF.with(|c| c.borrow().clone());
    let b = match method {
        "GET" => Request::get(path),
        "PUT" => Request::put(path),
        _ => Request::post(path),
    }
    .header("x-csrf-token", &csrf);
    let req = match body {
        Some(v) => b.json(&v).map_err(|e| e.to_string())?,
        None => b.build().map_err(|e| e.to_string())?,
    };
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let ok = resp.ok();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if ok { Ok(v) } else { Err(v["error"].as_str().unwrap_or("request failed").to_string()) }
}

pub fn csv(v: &Value) -> String {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

pub fn from_csv(s: &str) -> Value {
    json!(s.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>())
}

/// Asks for a TOTP code (or enrollment) when a save was refused with `step_up_required`.
#[component]
pub fn StepUp(need: RwSignal<bool>, enrolled: ReadSignal<bool>) -> impl IntoView {
    let (code, set_code) = signal(String::new());
    let (info, set_info) = signal(String::new());
    let enroll = move |_| spawn_local(async move {
        match api("POST", "/api/totp/enroll", None).await {
            Ok(v) => set_info.set(format!("Add this key to your authenticator app, then enter a code: {} ({})", v["secret"], v["uri"])),
            Err(e) => set_info.set(e),
        }
    });
    let verify = move |_| spawn_local(async move {
        match api("POST", "/api/totp/verify", Some(json!({"code": code.get_untracked()}))).await {
            Ok(_) => { need.set(false); set_info.set(String::new()); }
            Err(e) => set_info.set(e),
        }
    });
    view! {
        <Show when=move || need.get()>
            <div class="card">
                <b>"Confirm it's you"</b>
                <p class="muted">"This change needs your authenticator code. After verifying, save again."</p>
                <Show when=move || !enrolled.get()><button class="ghost" on:click=enroll>"Set up authenticator"</button></Show>
                <div class="row">
                    <input placeholder="123456" prop:value=code on:input=move |e| set_code.set(event_target_value(&e)) />
                    <button on:click=verify>"Verify"</button>
                </div>
                <p class="err">{move || info.get()}</p>
            </div>
        </Show>
    }
}

/// Runs a request, routing `step_up_required` to the prompt and other errors to `msg`.
pub fn run_act(fut: impl std::future::Future<Output = Result<Value, String>> + 'static, msg: RwSignal<String>, need: RwSignal<bool>, ok: impl FnOnce(Value) + 'static) {
    spawn_local(async move {
        match fut.await {
            Ok(v) => ok(v),
            Err(e) if e == "step_up_required" => need.set(true),
            Err(e) => msg.set(e),
        }
    });
}

/// Runs a save, recording the new version.
pub fn run_save(fut: impl std::future::Future<Output = Result<Value, String>> + 'static, version: RwSignal<i64>, msg: RwSignal<String>, need: RwSignal<bool>) {
    run_act(fut, msg, need, move |v| { version.set(v["version"].as_i64().unwrap_or(0)); msg.set(format!("Saved as version {}", version.get_untracked())); });
}
