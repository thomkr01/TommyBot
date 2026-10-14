use crate::{ApiError, AppState};
use axum::{
    extract::{FromRequestParts, Query, State},
    http::{request::Parts, Method},
    response::{IntoResponse, Redirect},
    Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use totp_rs::{Algorithm, Secret, TOTP};

const STEP_UP_MINUTES: i64 = 10;

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
        if parts.method != Method::GET
            && parts
                .headers
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                != Some(&csrf)
        {
            return Err(ApiError::Forbidden("bad csrf token"));
        }
        Ok(Sess {
            id: sid,
            user_id,
            username,
            access_token,
            stepped_up: step_up.is_some_and(|t| t > Utc::now()),
        })
    }
}

pub async fn login(State(st): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    let state = token();
    let url = st.discord.authorize_url(&state);
    (
        jar.add(cookie("oauth_state", state, st.secure)),
        Redirect::to(&url),
    )
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
    Ok((
        jar.remove(Cookie::from("oauth_state"))
            .add(cookie("sid", sid, st.secure)),
        Redirect::to("/"),
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

fn totp(secret_b32: &str, user: &str) -> Result<TOTP, ApiError> {
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
    sqlx::query("insert into dash_totp (user_id, secret) values ($1,$2) on conflict (user_id) do update set secret = $2")
        .bind(s.user_id).bind(&secret).execute(&st.db).await?;
    Ok(Json(
        json!({"secret": secret, "uri": totp(&secret, &s.username)?.get_url()}),
    ))
}

#[derive(Deserialize)]
pub struct Code {
    code: String,
}

/// Confirms enrollment on first use, and grants step-up for STEP_UP_MINUTES.
pub async fn totp_verify(
    State(st): State<AppState>,
    s: Sess,
    Json(c): Json<Code>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let secret = sqlx::query_scalar::<_, String>("select secret from dash_totp where user_id = $1")
        .bind(s.user_id)
        .fetch_optional(&st.db)
        .await?
        .ok_or(ApiError::Forbidden("totp not enrolled"))?;
    if !totp(&secret, &s.username)?
        .check_current(&c.code)
        .map_err(anyhow::Error::from)?
    {
        return Err(ApiError::Forbidden("invalid code"));
    }
    sqlx::query("update dash_totp set confirmed = true where user_id = $1")
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
