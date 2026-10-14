//! Web and third-party verification: a member signs in with Discord, passes Cloudflare Turnstile,
//! and the bot is told (as a `verify_grant` job) to hand out the roles.
use crate::{auth::Sess, store, ApiError, AppState};
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new().route("/verify/{guild}", get(page).post(submit))
}

/// Turnstile keys from the environment; both must be set for the widget to show.
fn keys() -> Option<(String, String)> {
    let k = |n| std::env::var(n).ok().filter(|v| !v.is_empty());
    Some((k("TURNSTILE_SITE_KEY")?, k("TURNSTILE_SECRET_KEY")?))
}

/// Whether this guild's verification config asks for the page, and whether it needs a captcha.
fn gate(cfg: &Value, have_keys: bool) -> Result<bool, &'static str> {
    let v = &cfg["verification"];
    let third = v["mode"] == "ThirdParty";
    if v["enabled"] != true || !(third || v["mode"] == "Web") {
        return Err("Web verification is not turned on for this server.");
    }
    if v["roles"].as_array().is_none_or(|r| r.is_empty()) {
        return Err("This server has no verification roles set.");
    }
    if third && !have_keys {
        return Err("The captcha is not configured on this bot. Ask the server staff.");
    }
    Ok(have_keys)
}

fn shell(body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Verify</title><style>body{{background:#1c1c1c;color:#eee;font:16px Poppins,system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0}}
main{{background:#212121;padding:2rem;border-radius:1rem;max-width:26rem;text-align:center}}button{{background:#0d9df8;color:#fff;border:0;border-radius:.5rem;padding:.7rem 1.4rem;font:inherit;cursor:pointer;text-transform:uppercase}}</style>
<main>{body}</main>"#
    ))
}

async fn page(
    State(st): State<AppState>,
    Path(guild): Path<i64>,
    sess: Result<Sess, ApiError>,
) -> Result<Response, ApiError> {
    let Ok(s) = sess else {
        return Ok(Redirect::to(&format!("/auth/login?next=/verify/{guild}")).into_response());
    };
    let (_, cfg) = store::current(&st.db, guild).await?;
    let keys = keys();
    let captcha = match gate(&cfg, keys.is_some()) {
        Ok(c) => c,
        Err(m) => return Ok(shell(&format!("<p>{m}</p>")).into_response()),
    };
    let widget = match (&keys, captcha) {
        (Some((site, _)), true) => format!(
            r#"<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script><div class="cf-turnstile" data-sitekey="{site}"></div>"#
        ),
        _ => String::new(),
    };
    Ok(shell(&format!(
        r#"<h2>Verify</h2><p>Signed in as {}.</p>{widget}<p><button id="go">Verify me</button></p><p id="msg"></p>
<script>go.onclick=async()=>{{go.disabled=true;
const t=document.querySelector('[name=cf-turnstile-response]');
const r=await fetch(location.pathname,{{method:'POST',headers:{{'content-type':'application/json','x-csrf-token':'{}'}},body:JSON.stringify({{token:t?t.value:''}})}});
const j=await r.json().catch(()=>({{}}));
msg.textContent=r.ok?'Done! Your roles arrive in a few seconds. You can close this page.':(j.error||'Something went wrong.');
if(!r.ok){{go.disabled=false;window.turnstile&&turnstile.reset()}}}}</script>"#,
        s.username.replace('&', "&amp;").replace('<', "&lt;"),
        s.csrf
    ))
    .into_response())
}

#[derive(Deserialize)]
struct Body {
    token: String,
}

async fn turnstile_ok(st: &AppState, secret: &str, token: &str) -> Result<bool, ApiError> {
    let r: Value = st
        .discord
        .http
        .post("https://challenges.cloudflare.com/turnstile/v0/siteverify")
        .form(&[("secret", secret), ("response", token)])
        .send()
        .await?
        .json()
        .await?;
    Ok(r["success"] == true)
}

async fn submit(
    State(st): State<AppState>,
    Path(guild): Path<i64>,
    s: Sess,
    Json(b): Json<Body>,
) -> Result<Json<Value>, ApiError> {
    let (_, cfg) = store::current(&st.db, guild).await?;
    let keys = keys();
    let captcha = gate(&cfg, keys.is_some()).map_err(|m| ApiError::BadRequest(m.into()))?;
    if let (true, Some((_, secret))) = (captcha, &keys) {
        if !turnstile_ok(&st, secret, &b.token).await? {
            return Err(ApiError::BadRequest("Captcha failed, try again.".into()));
        }
    }
    let in_guild = st
        .discord
        .user_guilds(&s.access_token)
        .await?
        .iter()
        .any(|g| g.id == guild.to_string());
    if !in_guild {
        return Err(ApiError::BadRequest("Join the server first.".into()));
    }
    tommybot_core::jobs::enqueue(
        &st.db,
        guild,
        "verify_grant",
        json!({ "user": s.user_id }),
        0,
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_follows_mode_roles_and_keys() {
        let cfg = |mode, roles: Value| json!({"verification": {"enabled": true, "mode": mode, "roles": roles}});
        assert_eq!(gate(&cfg("Web", json!(["1"])), false), Ok(false));
        assert_eq!(gate(&cfg("Web", json!(["1"])), true), Ok(true));
        assert!(gate(&cfg("ThirdParty", json!(["1"])), false).is_err());
        assert_eq!(gate(&cfg("ThirdParty", json!(["1"])), true), Ok(true));
        assert!(gate(&cfg("Captcha", json!(["1"])), true).is_err());
        assert!(gate(&cfg("Web", json!([])), true).is_err());
        assert!(gate(&json!({}), true).is_err());
    }
}
