//! OAuth 2.1 for claude.ai (MCP authorization spec): login goes through Discord and only `owner_id`
//! gets a token. Stateless: client ids, codes and tokens are HMAC-signed JSON, so the read-only
//! database role never has to write. Rotating `MCP_OAUTH_SECRET` revokes everything at once.
use axum::{
    extract::{Query, State as S},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Form, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Clone)]
pub struct OAuth {
    pub secret: String,
    /// Public base URL, e.g. https://mcp.example.com (no trailing slash).
    pub base: String,
    pub owner_id: String,
    pub client_id: String,
    pub client_secret: String,
    pub discord_api: String,
    pub http: reqwest::Client,
}

const ACCESS_S: u64 = 3600;
const REFRESH_S: u64 = 30 * 86400;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl OAuth {
    fn mac(&self) -> Hmac<Sha256> {
        Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).unwrap()
    }

    /// `typ` keeps a value of one kind from being used as another (a client id as a token, say).
    pub fn sign(&self, typ: &str, ttl: u64, mut v: Value) -> String {
        v["typ"] = json!(typ);
        v["exp"] = json!(now() + ttl);
        let p = B64.encode(v.to_string());
        let mut m = self.mac();
        m.update(p.as_bytes());
        format!("{p}.{}", B64.encode(m.finalize().into_bytes()))
    }

    pub fn open(&self, s: &str, typ: &str) -> Option<Value> {
        let (p, sig) = s.split_once('.')?;
        let mut m = self.mac();
        m.update(p.as_bytes());
        m.verify_slice(&B64.decode(sig).ok()?).ok()?;
        let v: Value = serde_json::from_slice(&B64.decode(p).ok()?).ok()?;
        (v["typ"] == typ && v["exp"].as_u64()? > now()).then_some(v)
    }

    /// A valid access token for the owner.
    pub fn access_ok(&self, tok: &str) -> bool {
        self.open(tok, "access")
            .is_some_and(|v| v["sub"] == self.owner_id.as_str())
    }

    pub fn challenge_header(&self) -> String {
        format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            self.base
        )
    }
}

type St = S<crate::State>;

fn oauth(st: &crate::State) -> Result<&OAuth, StatusCode> {
    st.oauth.as_ref().ok_or(StatusCode::NOT_FOUND)
}

fn bad(code: StatusCode, err: &str, desc: &str) -> Response {
    (code, Json(json!({"error": err, "error_description": desc}))).into_response()
}

pub async fn resource_meta(S(st): St) -> Response {
    let Ok(o) = oauth(&st) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(json!({"resource": format!("{}/mcp", o.base), "authorization_servers": [o.base], "bearer_methods_supported": ["header"]}))
        .into_response()
}

pub async fn server_meta(S(st): St) -> Response {
    let Ok(o) = oauth(&st) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(json!({
        "issuer": o.base,
        "authorization_endpoint": format!("{}/authorize", o.base),
        "token_endpoint": format!("{}/token", o.base),
        "registration_endpoint": format!("{}/register", o.base),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["read"],
    }))
    .into_response()
}

fn allowed_redirect(u: &str) -> bool {
    u.starts_with("https://")
        || u.starts_with("http://localhost")
        || u.starts_with("http://127.0.0.1")
}

/// Dynamic client registration. Anyone may register; only the owner can finish a login.
pub async fn register(S(st): St, Json(req): Json<Value>) -> Response {
    let o = match oauth(&st) {
        Ok(o) => o,
        Err(r) => return r.into_response(),
    };
    let uris: Vec<String> = req["redirect_uris"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if uris.is_empty()
        || uris.len() > 5
        || !uris.iter().all(|u| allowed_redirect(u) && u.len() < 500)
    {
        return bad(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "https redirect_uris required",
        );
    }
    let id = o.sign("client", 10 * 365 * 86400, json!({"uris": uris}));
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": id,
            "redirect_uris": uris,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct AuthorizeQ {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: Option<String>,
    state: Option<String>,
}

pub async fn authorize(S(st): St, Query(q): Query<AuthorizeQ>) -> Response {
    let o = match oauth(&st) {
        Ok(o) => o,
        Err(r) => return r.into_response(),
    };
    // Never redirect anywhere before the redirect_uri is known to belong to the client.
    let registered = o.open(&q.client_id, "client").is_some_and(|c| {
        c["uris"]
            .as_array()
            .is_some_and(|a| a.iter().any(|u| u == q.redirect_uri.as_str()))
    });
    if !registered {
        return bad(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "unknown client or redirect_uri",
        );
    }
    if q.response_type != "code"
        || q.code_challenge_method.as_deref() != Some("S256")
        || q.code_challenge.len() < 43
    {
        return bad(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "need response_type=code and an S256 code_challenge",
        );
    }
    let state = o.sign("login", 600, json!({"cid": q.client_id, "ruri": q.redirect_uri, "chal": q.code_challenge, "st": q.state}));
    Redirect::to(&format!(
        "https://discord.com/oauth2/authorize?response_type=code&client_id={}&scope=identify&state={}&redirect_uri={}",
        o.client_id,
        enc(&state),
        enc(&format!("{}/oauth/callback", o.base))
    ))
    .into_response()
}

#[derive(Deserialize)]
pub struct CallbackQ {
    code: Option<String>,
    state: String,
}

async fn discord_user(o: &OAuth, code: &str) -> anyhow::Result<String> {
    let t: Value = o
        .http
        .post(format!("{}/oauth2/token", o.discord_api))
        .form(&[
            ("client_id", o.client_id.as_str()),
            ("client_secret", &o.client_secret),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &format!("{}/oauth/callback", o.base)),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tok = t["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token"))?;
    let me: Value = o
        .http
        .get(format!("{}/users/@me", o.discord_api))
        .bearer_auth(tok)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    me["id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("no user id"))
}

pub async fn callback(S(st): St, Query(q): Query<CallbackQ>) -> Response {
    let o = match oauth(&st) {
        Ok(o) => o,
        Err(r) => return r.into_response(),
    };
    let Some(login) = o.open(&q.state, "login") else {
        return (
            StatusCode::BAD_REQUEST,
            Html("Innloggingen er utløpt. Prøv igjen fra Claude."),
        )
            .into_response();
    };
    let Some(code) = q.code else {
        return (
            StatusCode::BAD_REQUEST,
            Html("Discord-innloggingen ble avbrutt."),
        )
            .into_response();
    };
    let user = match discord_user(o, &code).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("mcp oauth: discord login failed: {e:#}");
            return (
                StatusCode::BAD_GATEWAY,
                Html("Discord svarte ikke som ventet. Prøv igjen."),
            )
                .into_response();
        }
    };
    if user != o.owner_id {
        tracing::warn!(user, "mcp oauth: login refused, not the owner");
        return (
            StatusCode::FORBIDDEN,
            Html("Bare bot-eieren har tilgang til TommyBot MCP."),
        )
            .into_response();
    }
    tracing::info!(user, "mcp oauth: owner logged in");
    // shortcut: stateless codes can be replayed for 2 minutes, but only together with the PKCE verifier.
    let ours = o.sign(
        "code",
        120,
        json!({"cid": login["cid"], "ruri": login["ruri"], "chal": login["chal"], "sub": user}),
    );
    let ruri = login["ruri"].as_str().unwrap_or_default();
    let sep = if ruri.contains('?') { '&' } else { '?' };
    let mut to = format!("{ruri}{sep}code={}", enc(&ours));
    if let Some(s) = login["st"].as_str() {
        to += &format!("&state={}", enc(s));
    }
    Redirect::to(&to).into_response()
}

fn tokens(o: &OAuth, sub: &str, cid: &str) -> Response {
    Json(json!({
        "access_token": o.sign("access", ACCESS_S, json!({"sub": sub})),
        "token_type": "Bearer",
        "expires_in": ACCESS_S,
        "refresh_token": o.sign("refresh", REFRESH_S, json!({"sub": sub, "cid": cid})),
        "scope": "read",
    }))
    .into_response()
}

pub async fn token(S(st): St, Form(f): Form<HashMap<String, String>>) -> Response {
    let o = match oauth(&st) {
        Ok(o) => o,
        Err(r) => return r.into_response(),
    };
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
    let invalid = || {
        bad(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "invalid or expired grant",
        )
    };
    match get("grant_type") {
        "authorization_code" => {
            let Some(c) = o.open(get("code"), "code") else {
                return invalid();
            };
            let pkce = B64.encode(Sha256::digest(get("code_verifier").as_bytes()));
            if c["cid"] != get("client_id")
                || c["ruri"] != get("redirect_uri")
                || c["chal"] != pkce.as_str()
            {
                return invalid();
            }
            tokens(o, c["sub"].as_str().unwrap_or_default(), get("client_id"))
        }
        "refresh_token" => {
            let Some(r) = o.open(get("refresh_token"), "refresh") else {
                return invalid();
            };
            let cid = r["cid"].as_str().unwrap_or_default();
            if r["sub"] != o.owner_id.as_str()
                || (!get("client_id").is_empty() && get("client_id") != cid)
            {
                return invalid();
            }
            tokens(o, &o.owner_id, cid)
        }
        _ => bad(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
    }
}
