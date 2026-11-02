use crate::{ApiError, AppState};
use axum::{
    extract::{FromRequestParts, Query, Request, State},
    http::{request::Parts, Method},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration as StdDuration, Instant};
use totp_rs::{Algorithm, Secret, TOTP};

const STEP_UP_MINUTES: i64 = 10;
/// Wrong TOTP codes allowed per user within `TOTP_WINDOW`.
const TOTP_FAILS: usize = 5;
const TOTP_WINDOW: StdDuration = StdDuration::from_secs(600);
/// Requests a session may make at once, refilled at `RATE` per second.
const BURST: f64 = 20.0;
const RATE: f64 = 20.0;

#[derive(Default)]
pub struct TotpAttempts {
    fails: Vec<Instant>,
    last_step: u64,
}

impl TotpAttempts {
    fn locked(&mut self, now: Instant) -> bool {
        self.fails.retain(|t| now.duration_since(*t) < TOTP_WINDOW);
        self.fails.len() >= TOTP_FAILS
    }

    /// Accepts `step` (the time step the code matched) once; a miss or a replay counts as a failure.
    fn accept(&mut self, step: Option<u64>, now: Instant) -> bool {
        match step.filter(|s| *s > self.last_step) {
            Some(s) => {
                self.last_step = s;
                true
            }
            None => {
                self.fails.push(now);
                false
            }
        }
    }
}

pub struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn take(&mut self, now: Instant) -> bool {
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * RATE).min(BURST);
        self.at = now;
        let ok = self.tokens >= 1.0;
        if ok {
            self.tokens -= 1.0;
        }
        ok
    }
}

/// 429 for a session that sends more than `BURST` requests at once (Discord's limit is the bot's, shared).
pub async fn rate_limit(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(sid) = CookieJar::from_headers(req.headers()).get("sid") {
        let now = Instant::now();
        let ok = {
            let mut m = st.rate.lock().unwrap();
            if m.len() > 10_000 {
                m.retain(|_, b| now.duration_since(b.at).as_secs_f64() < BURST / RATE);
            }
            m.entry(sid.value().to_string())
                .or_insert(Bucket {
                    tokens: BURST,
                    at: now,
                })
                .take(now)
        };
        if !ok {
            return ApiError::TooMany.into_response();
        }
    }
    next.run(req).await
}

pub fn token() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    B64.encode(b)
}

fn cookie(name: &'static str, val: String, secure: bool) -> Cookie<'static> {
    Cookie::build((name, val))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .path("/")
        .build()
}

pub struct Sess {
    pub id: String,
    pub user_id: i64,
    pub username: String,
    pub access_token: String,
    pub stepped_up: bool,
    pub csrf: String,
}

impl FromRequestParts<AppState> for Sess {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let jar = CookieJar::from_headers(&parts.headers);
        let sid = jar
            .get("sid")
            .ok_or(ApiError::Unauthorized)?
            .value()
            .to_string();
        let row: (i64, String, String, String, Option<DateTime<Utc>>) = sqlx::query_as(
            "select user_id, username, access_token, csrf, step_up_until from dash_session where id = $1 and expires_at > now()",
        ).bind(&sid).fetch_optional(&st.db).await?.ok_or(ApiError::Unauthorized)?;
        let (user_id, username, access_token, csrf, step_up) = row;
        // CSRF: every state-changing request must echo the per-session token.
        if !csrf_ok(
            &parts.method,
            parts
                .headers
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok()),
            &csrf,
        ) {
            return Err(ApiError::Forbidden("bad csrf token"));
        }
        Ok(Sess {
            id: sid,
            user_id,
            username,
            access_token,
            stepped_up: step_up.is_some_and(|t| t > Utc::now()),
            csrf,
        })
    }
}

/// Safe methods need no token; everything else must echo the session's.
fn csrf_ok(method: &Method, header: Option<&str>, csrf: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) || header == Some(csrf)
}

#[derive(Deserialize)]
pub struct Login {
    next: Option<String>,
}

pub async fn login(
    State(st): State<AppState>,
    jar: CookieJar,
    Query(q): Query<Login>,
) -> impl IntoResponse {
    let state = token();
    let url = st.discord.authorize_url(&state);
    let mut jar = jar.add(cookie("oauth_state", state, st.secure));
    if let Some(n) = q
        .next
        .filter(|n| (n.starts_with("/verify/") || n.starts_with("/#/")) && !n.contains("//"))
    {
        jar = jar.add(cookie("login_next", n, st.secure));
    }
    (jar, Redirect::to(&url))
}

#[derive(Deserialize)]
pub struct Callback {
    code: String,
    state: String,
}

pub async fn callback(
    State(st): State<AppState>,
    jar: CookieJar,
    Query(q): Query<Callback>,
) -> Result<impl IntoResponse, ApiError> {
    if jar.get("oauth_state").map(|c| c.value()) != Some(&q.state) {
        return Err(ApiError::Forbidden("oauth state mismatch"));
    }
    let tok = st.discord.exchange(&q.code).await?;
    let me = st.discord.me(&tok).await?;
    let (sid, csrf) = (token(), token());
    sqlx::query("insert into dash_session (id, user_id, username, access_token, csrf, expires_at) values ($1,$2,$3,$4,$5,$6)")
        .bind(&sid).bind(me.id.parse::<i64>().map_err(anyhow::Error::from)?).bind(me.username).bind(tok).bind(csrf)
        .bind(Utc::now() + Duration::days(7)).execute(&st.db).await?;
    let next = jar
        .get("login_next")
        .map_or("/".into(), |c| c.value().to_string());
    Ok((
        jar.remove(Cookie::from("oauth_state"))
            .remove(Cookie::from("login_next"))
            .add(cookie("sid", sid, st.secure)),
        Redirect::to(&next),
    ))
}

pub async fn logout(
    State(st): State<AppState>,
    s: Sess,
    jar: CookieJar,
) -> Result<impl IntoResponse, ApiError> {
    sqlx::query("delete from dash_session where id = $1")
        .bind(&s.id)
        .execute(&st.db)
        .await?;
    Ok((jar.remove(Cookie::from("sid")), Json(json!({"ok": true}))))
}

pub async fn me(State(st): State<AppState>, s: Sess) -> Result<Json<serde_json::Value>, ApiError> {
    let csrf = sqlx::query_scalar::<_, String>("select csrf from dash_session where id = $1")
        .bind(&s.id)
        .fetch_one(&st.db)
        .await?;
    let enrolled =
        sqlx::query_scalar::<_, bool>("select confirmed from dash_totp where user_id = $1")
            .bind(s.user_id)
            .fetch_optional(&st.db)
            .await?
            .unwrap_or(false);
    Ok(Json(
        json!({"user_id": s.user_id.to_string(), "username": s.username, "csrf": csrf, "totp_enrolled": enrolled, "stepped_up": s.stepped_up,
            "support_url": std::env::var("SUPPORT_URL").ok().filter(|u| !u.is_empty())}),
    ))
}

pub(crate) fn totp(secret_b32: &str, user: &str) -> Result<TOTP, ApiError> {
    let raw = Secret::Encoded(secret_b32.into())
        .to_bytes()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        raw,
        Some("TommyBot".into()),
        user.into(),
    )
    .map_err(anyhow::Error::from)?)
}

/// Starts (or restarts, until confirmed) enrollment; returns the otpauth URI to render as a QR code.
pub async fn totp_enroll(
    State(st): State<AppState>,
    s: Sess,
) -> Result<Json<serde_json::Value>, ApiError> {
    if sqlx::query_scalar::<_, bool>("select confirmed from dash_totp where user_id = $1")
        .bind(s.user_id)
        .fetch_optional(&st.db)
        .await?
        .unwrap_or(false)
    {
        return Err(ApiError::Forbidden("totp already enrolled"));
    }
    let secret = Secret::generate_secret().to_encoded().to_string();
    let dm_code = format!("{:06}", rand::rngs::OsRng.next_u32() % 1_000_000);
    dm_enroll_code(&st, s.user_id, &dm_code).await?;
    sqlx::query("insert into dash_totp (user_id, secret, dm_code) values ($1,$2,$3) on conflict (user_id) do update set secret = $2, dm_code = $3")
        .bind(s.user_id).bind(&secret).bind(&dm_code).execute(&st.db).await?;
    Ok(Json(
        json!({"secret": secret, "uri": totp(&secret, &s.username)?.get_url()}),
    ))
}

/// DMs the enrollment code to the logged-in Discord account; a stolen session cookie can't read it.
async fn dm_enroll_code(st: &AppState, user: i64, code: &str) -> Result<(), ApiError> {
    const NO_DM: &str = "couldn't DM you; allow DMs from server members and try again";
    let ch = st
        .discord
        .call(
            Method::POST,
            "/users/@me/channels",
            Some(json!({"recipient_id": user.to_string()})),
        )
        .await
        .map_err(|_| ApiError::BadRequest(NO_DM.into()))?;
    let ch = ch["id"]
        .as_str()
        .ok_or(ApiError::BadRequest(NO_DM.into()))?;
    let text = format!("Your TommyBot authenticator setup code is **{code}**. If you didn't start this, ignore it and log out of the dashboard everywhere.");
    st.discord
        .call(
            Method::POST,
            &format!("/channels/{ch}/messages"),
            Some(json!({"content": text, "allowed_mentions": {"parse": []}})),
        )
        .await
        .map_err(|_| ApiError::BadRequest(NO_DM.into()))?;
    Ok(())
}

#[derive(Deserialize)]
pub struct Code {
    code: String,
    /// The code from the bot's DM; only needed to confirm a new enrollment.
    #[serde(default)]
    dm_code: String,
}

/// Confirms enrollment on first use, and grants step-up for STEP_UP_MINUTES.
pub async fn totp_verify(
    State(st): State<AppState>,
    s: Sess,
    Json(c): Json<Code>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (secret, confirmed, dm_code) = sqlx::query_as::<_, (String, bool, Option<String>)>(
        "select secret, confirmed, dm_code from dash_totp where user_id = $1",
    )
    .bind(s.user_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or(ApiError::Forbidden("totp not enrolled"))?;
    let totp = totp(&secret, &s.username)?;
    let now_step = std::time::UNIX_EPOCH
        .elapsed()
        .map_err(anyhow::Error::from)?
        .as_secs()
        / 30;
    // Same window as `check_current` (skew 1), but we need the step to refuse a replay.
    let step = (now_step - 1..=now_step + 1).find(|t| totp.generate(t * 30) == c.code);
    // A wrong DM code counts as a miss, so it shares the TOTP lockout.
    let step = step.filter(|_| confirmed || dm_code.as_deref() == Some(c.dm_code.as_str()));
    {
        let mut m = st.totp.lock().unwrap();
        let a = m.entry(s.user_id).or_default();
        if a.locked(Instant::now()) {
            return Err(ApiError::TooMany);
        }
        if !a.accept(step, Instant::now()) {
            return Err(ApiError::Forbidden("invalid code"));
        }
    }
    sqlx::query("update dash_totp set confirmed = true, dm_code = null where user_id = $1")
        .bind(s.user_id)
        .execute(&st.db)
        .await?;
    sqlx::query("update dash_session set step_up_until = $2 where id = $1")
        .bind(&s.id)
        .bind(Utc::now() + Duration::minutes(STEP_UP_MINUTES))
        .execute(&st.db)
        .await?;
    Ok(Json(json!({"stepped_up": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_required_on_writes() {
        for m in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(!csrf_ok(&m, None, "t"));
            assert!(!csrf_ok(&m, Some("x"), "t"));
            assert!(csrf_ok(&m, Some("t"), "t"));
        }
        assert!(csrf_ok(&Method::GET, None, "t"));
    }

    #[test]
    fn totp_locks_after_five_misses_and_refuses_replay() {
        let now = Instant::now();
        let mut a = TotpAttempts::default();
        assert!(a.accept(Some(100), now));
        assert!(!a.accept(Some(100), now), "same step twice");
        assert!(!a.accept(Some(99), now), "older step");
        assert!(a.accept(Some(101), now));
        for _ in 0..3 {
            assert!(!a.locked(now));
            a.accept(None, now);
        }
        assert!(a.locked(now));
        assert!(!a.locked(now + TOTP_WINDOW), "misses expire");
    }

    #[test]
    fn bucket_allows_burst_then_refills() {
        let now = Instant::now();
        let mut b = Bucket {
            tokens: BURST,
            at: now,
        };
        assert!((0..20).all(|_| b.take(now)));
        assert!(!b.take(now));
        assert!(b.take(now + StdDuration::from_millis(100)));
    }
}
