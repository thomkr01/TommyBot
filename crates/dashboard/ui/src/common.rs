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
    let (err, set_err) = signal(String::new());
    let (qr, set_qr) = signal(String::new());
    let (secret, set_secret) = signal(String::new());
    let enroll = move |_| spawn_local(async move {
        match api("POST", "/api/totp/enroll", None).await {
            Ok(v) => {
                let svg = qrcode::QrCode::new(v["uri"].as_str().unwrap_or("")).map(|q| q.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).build());
                set_qr.set(svg.unwrap_or_default());
                set_secret.set(v["secret"].as_str().unwrap_or("").to_string());
            }
            Err(e) => set_err.set(e),
        }
    });
    let verify = move |_| spawn_local(async move {
        match api("POST", "/api/totp/verify", Some(json!({"code": code.get_untracked()}))).await {
            Ok(_) => { need.set(false); set_err.set(String::new()); set_qr.set(String::new()); }
            Err(e) => set_err.set(e),
        }
    });
    view! {
        <Show when=move || need.get()>
            <div class="card">
                <b>"Confirm it's you"</b>
                <p class="muted">"This change needs your authenticator code. After verifying, save again."</p>
                <Show when=move || !enrolled.get()><button class="ghost" on:click=enroll>"Set up authenticator"</button></Show>
                <Show when=move || !qr.get().is_empty()>
                    <p class="info">"Scan this QR code with your authenticator app, then enter a code below."</p>
                    <div class="qr" inner_html=move || qr.get()></div>
                    <p class="muted">"Can't scan? Enter this key manually: " <code>{move || secret.get()}</code></p>
                </Show>
                <div class="row">
                    <input placeholder="123456" prop:value=code on:input=move |e| set_code.set(event_target_value(&e)) />
                    <button on:click=verify>"Verify"</button>
                </div>
                <p class="err">{move || err.get()}</p>
            </div>
        </Show>
    }
}

/// Page header: the page name over the server name.
#[component]
pub fn Head(guild: String, page: String) -> impl IntoView {
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    view! { <div class="head"><small>{page}</small><h1>{move || guilds.get().iter().find(|g| g["id"] == guild.as_str()).and_then(|g| g["name"].as_str().map(String::from)).unwrap_or_else(|| guild.clone())}</h1></div> }
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
