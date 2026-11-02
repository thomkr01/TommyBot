use gloo_net::http::Request;
use leptos::{prelude::*, task::spawn_local};
use serde_json::{json, Value};
use std::{cell::RefCell, time::Duration};

/// How long a closing bar, toast or dialog fades out; matches `--dur` in style.css.
pub const OUT: Duration = Duration::from_millis(200);

thread_local! { pub static CSRF: RefCell<String> = const { RefCell::new(String::new()) }; }

pub async fn api(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let csrf = CSRF.with(|c| c.borrow().clone());
    let b = match method {
        "GET" => Request::get(path),
        "PUT" => Request::put(path),
        "DELETE" => Request::delete(path),
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

pub fn confirm(text: &str) -> bool {
    web_sys::window().is_some_and(|w| w.confirm_with_message(text).unwrap_or(false))
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
    let (dm_code, set_dm_code) = signal(String::new());
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
        match api("POST", "/api/totp/verify", Some(json!({"code": code.get_untracked(), "dm_code": dm_code.get_untracked()}))).await {
            Ok(_) => { need.set(false); set_err.set(String::new()); set_qr.set(String::new()); }
            Err(e) => set_err.set(e),
        }
    });
    view! {
        <AnimatedShow when=need hide_class="out" hide_delay=OUT>
            <div class="card">
                <b>"Confirm it's you"</b>
                <p class="muted">"This change needs your authenticator code. After verifying, save again."</p>
                <Show when=move || !enrolled.get()><button class="ghost" on:click=enroll>"Set up authenticator"</button></Show>
                <Show when=move || !qr.get().is_empty()>
                    <p class="info">"Scan this QR code with your authenticator app, then enter a code below."</p>
                    <div class="qr" inner_html=move || qr.get()></div>
                    <p class="muted">"Can't scan? Enter this key manually: " <code>{move || secret.get()}</code></p>
                    <p class="info">"TommyBot also sent you a code in Discord DMs. Enter it here to confirm the setup."</p>
                    <input placeholder="Discord DM code" aria-label="code from Discord DM" autocomplete="off" prop:value=dm_code on:input=move |e| set_dm_code.set(event_target_value(&e)) />
                </Show>
                <div class="row">
                    <input placeholder="123456" aria-label="authenticator code" autocomplete="one-time-code" prop:value=code on:input=move |e| set_code.set(event_target_value(&e)) />
                    <button on:click=verify>"Verify"</button>
                </div>
                <p class="err">{move || err.get()}</p>
            </div>
        </AnimatedShow>
    }
}

/// Page header: small label over the big title (the server name unless `big` is given; an empty `big` hides it).
#[component]
pub fn Head(guild: String, page: String, #[prop(optional)] big: Option<String>) -> impl IntoView {
    let guilds = expect_context::<ReadSignal<Vec<Value>>>();
    let title = move || big.clone().unwrap_or_else(|| guilds.get().iter().find(|g| g["id"] == guild.as_str()).and_then(|g| g["name"].as_str().map(String::from)).unwrap_or_else(|| guild.clone()));
    view! { <div class="head"><small>{page}</small>{move || Some(title()).filter(|t| !t.is_empty()).map(|t| view! { <h1>{t}</h1> })}</div> }
}

/// Runs a request, routing `step_up_required` to the prompt and other errors to `msg`.
pub fn run_act(fut: impl std::future::Future<Output = Result<Value, String>> + 'static, msg: Msg, need: RwSignal<bool>, ok: impl FnOnce(Value) + 'static) {
    spawn_local(async move {
        match fut.await {
            Ok(v) => ok(v),
            Err(e) if e == "step_up_required" => need.set(true),
            Err(e) => msg.err(e),
        }
    });
}

/// A toast's text and whether it reports a failure.
#[derive(Clone, Copy, Default)]
pub struct Msg { text: RwSignal<String>, bad: RwSignal<bool> }

impl Msg {
    pub fn ok(self, t: impl Into<String>) { self.bad.set(false); self.text.set(t.into()) }
    pub fn err(self, e: impl Into<String>) { self.bad.set(true); self.text.set(e.into()) }
    pub fn clear(self) { self.text.set(String::new()) }
    pub fn get(self) -> String { self.text.get() }
}

/// Click-to-close message, red-edged for failures; keeps its last text while it fades out.
#[component]
pub fn Toast(msg: Msg) -> impl IntoView {
    let text = Memo::new(move |prev: Option<&String>| { let m = msg.get(); if m.is_empty() { prev.cloned().unwrap_or_default() } else { m } });
    view! { <AnimatedShow when=Signal::derive(move || !msg.get().is_empty()) hide_class="out" hide_delay=OUT><p class="toast" class:bad=move || msg.bad.get() role=move || if msg.bad.get() { "alert" } else { "status" } on:click=move |_| msg.clear()>{move || text.get()}</p></AnimatedShow> }
}

/// Runs a save, recording the new version.
pub fn run_save(fut: impl std::future::Future<Output = Result<Value, String>> + 'static, version: RwSignal<i64>, msg: Msg, need: RwSignal<bool>) {
    run_act(fut, msg, need, move |v| { version.set(v["version"].as_i64().unwrap_or(0)); msg.ok(format!("Saved as version {}", version.get_untracked())); });
}

fn focusables(root: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let Ok(l) = root.query_selector_all("button:not([disabled]), input:not([disabled]), select, textarea, a[href]") else { return vec![] };
    (0..l.length()).filter_map(|i| l.item(i)?.dyn_into().ok()).collect()
}

/// Escape closes a dialog and Tab cycles inside it; `e` is the dialog's own keydown.
pub fn dialog_keys(e: &leptos::ev::KeyboardEvent, close: impl FnOnce()) {
    use wasm_bindgen::JsCast;
    if e.key() == "Escape" { return close() }
    if e.key() != "Tab" { return }
    let Some(root) = e.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
    let list = focusables(&root);
    let (Some(first), Some(last)) = (list.first(), list.last()) else { return };
    let active = web_sys::window().and_then(|w| w.document()).and_then(|d| d.active_element());
    let on = |el: &web_sys::HtmlElement| active.as_ref().is_some_and(|a| a.is_same_node(Some(el)));
    let to = if e.shift_key() && on(first) { last } else if !e.shift_key() && on(last) { first } else { return };
    e.prevent_default();
    let _ = to.focus();
}

/// Moves focus into a dialog once it is on the page.
pub fn focus_on_open(r: NodeRef<leptos::html::Div>) {
    Effect::new(move || if let Some(d) = r.get() { if let Some(f) = focusables(&d).first() { let _ = f.focus(); } });
}
