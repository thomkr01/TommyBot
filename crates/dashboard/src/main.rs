mod access;
mod api;
mod appeals;
mod auth;
mod channels;
mod diff;
mod discord;
mod embeds;
mod lockdown;
mod mcp;
mod ops;
mod otp;
mod records;
mod recovery;
mod schema;
mod sections;
mod store;
mod update;
mod verify;

use axum::{
    extract::DefaultBodyLimit,
    http::{
        header::{
            CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
            X_FRAME_OPTIONS,
        },
        HeaderValue, StatusCode,
    },
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde_json::json;
use sqlx::PgPool;
use tower_http::{
    services::ServeDir,
    set_header::SetResponseHeaderLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;

// shortcut: in-memory state, correct only while the dashboard runs as one process; move to the DB if it scales out.
pub type Shared<K, V> = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<K, V>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub discord: discord::Discord,
    pub secure: bool,
    /// Per-user TOTP failures and last accepted step.
    pub totp: Shared<i64, auth::TotpAttempts>,
    /// Per-session request buckets.
    pub rate: Shared<String, auth::Bucket>,
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden(&'static str),
    /// Needs a fresh TOTP code (or enrollment) before this write.
    StepUp,
    NotFound,
    TooMany,
    Conflict(i32),
    /// 409 with its own message.
    Busy(&'static str),
    BadRequest(String),
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError::Internal(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, body) = match self {
            ApiError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, json!({"error": "login required"}))
            }
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, json!({"error": m})),
            ApiError::StepUp => (StatusCode::FORBIDDEN, json!({"error": "step_up_required"})),
            ApiError::NotFound => (StatusCode::NOT_FOUND, json!({"error": "not found"})),
            ApiError::TooMany => (
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error": "too many requests, slow down"}),
            ),
            ApiError::Conflict(v) => (
                StatusCode::CONFLICT,
                json!({"error": "config changed, reload", "current_version": v}),
            ),
            ApiError::Busy(m) => (StatusCode::CONFLICT, json!({"error": m})),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, json!({"error": m})),
            ApiError::Internal(e) => {
                tracing::error!("{e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "internal error"}),
                )
            }
        };
        (code, Json(body)).into_response()
    }
}

pub fn router(st: AppState, dist: &str) -> Router {
    let g = "/api/guilds/{guild}";
    Router::new()
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/api/me", get(auth::me))
        .route("/api/totp/enroll", post(auth::totp_enroll))
        .route("/api/totp/verify", post(auth::totp_verify))
        .route("/api/guilds", get(api::guilds))
        .route("/api/permit-catalog", get(api::permit_catalog))
        .route("/api/previews", get(api::previews))
        .route("/api/commands", get(api::commands))
        .route(&format!("{g}/config"), get(api::get_config))
        .route(&format!("{g}/access"), get(api::page_access))
        .route(
            &format!("{g}/config/{{section}}"),
            put(api::put_section).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(&format!("{g}/permits"), put(api::put_permits))
        .route(&format!("{g}/import"), post(api::import))
        .route(&format!("{g}/history"), get(api::history))
        .route(
            &format!("{g}/history/{{from}}/{{to}}"),
            get(api::diff_versions),
        )
        .route(
            &format!("{g}/history/{{version}}/rollback"),
            post(api::rollback),
        )
        .route(&format!("{g}/purge"), post(api::purge))
        .merge(sections::routes())
        .merge(ops::routes())
        .merge(lockdown::routes())
        .merge(recovery::routes())
        .merge(channels::routes())
        .merge(appeals::routes())
        .merge(records::routes())
        .merge(verify::routes())
        .merge(embeds::routes())
        .merge(update::routes())
        .merge(mcp::routes())
        // Only routes, not the static UI: one page load fetches many assets.
        .route_layer(middleware::from_fn_with_state(st.clone(), auth::rate_limit))
        // Hash routing means the server never needs an index.html fallback; one would also hide
        // missing assets by answering a stale hashed .js/.wasm request with HTML.
        .fallback_service(ServeDir::new(dist).not_found_service(
            axum::routing::any(|| async {
                (StatusCode::NOT_FOUND, "Not found. If this is the dashboard root, the web UI was not built into the image: rebuild with docker compose up -d --build.")
            }),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        // Only frame-ancestors: a full policy would have to allow the WASM UI and Turnstile.
        .layer(SetResponseHeaderLayer::overriding(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("frame-ancestors 'none'"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(st)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _log = tommybot_core::log::init(
        "dashboard",
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    );
    let env = |k: &str| std::env::var(k).map_err(|_| anyhow::anyhow!("{k} not set"));
    let base = env("DASHBOARD_URL")?;
    let db = tommybot_core::db::connect(&env("DATABASE_URL")?).await?;
    let st = AppState {
        db,
        secure: base.starts_with("https://"),
        totp: Default::default(),
        rate: Default::default(),
        discord: discord::Discord {
            http: reqwest::Client::new(),
            client_id: env("DISCORD_CLIENT_ID")?,
            client_secret: env("DISCORD_CLIENT_SECRET")?,
            bot_token: env("DISCORD_TOKEN")?,
            redirect_uri: format!("{}/auth/callback", base.trim_end_matches('/')),
            api: std::env::var("DISCORD_API_BASE")
                .unwrap_or_else(|_| "https://discord.com/api/v10".into()),
            memo: Default::default(),
        },
    };
    if let Err(e) = st.discord.load_emojis().await {
        tracing::warn!(?e, "custom emojis not loaded, using Unicode fallbacks");
    }
    let dist =
        std::env::var("DASHBOARD_DIST").unwrap_or_else(|_| "crates/dashboard/ui/dist".into());
    if !std::path::Path::new(&dist).join("index.html").is_file() {
        tracing::error!(
            "no index.html in {dist}: the web UI was not built or copied, pages will be blank"
        );
    }
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("dashboard listening on {bind}, UI from {dist}, public URL {base}");
    axum::serve(listener, router(st, &dist)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn api_routes_are_rate_limited_and_every_response_has_security_headers() {
        let st = AppState {
            db: PgPool::connect_lazy("postgres://x@127.0.0.1:1/x").unwrap(),
            discord: discord::Discord {
                http: reqwest::Client::new(),
                client_id: String::new(),
                client_secret: String::new(),
                bot_token: String::new(),
                redirect_uri: String::new(),
                api: String::new(),
                memo: Default::default(),
            },
            secure: false,
            totp: Default::default(),
            rate: Default::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(st, "/nonexistent");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        // /auth/login answers without the database, so only the limiter decides the status.
        let get = |path: &str| {
            http.get(format!("{base}{path}"))
                .header("cookie", "sid=x")
                .send()
        };
        for _ in 0..20 {
            assert_ne!(get("/auth/login").await.unwrap().status(), 429);
        }
        let limited = get("/auth/login").await.unwrap();
        assert_eq!(limited.status(), 429);
        assert_eq!(limited.headers()["x-frame-options"], "DENY");
        let asset = get("/app.wasm").await.unwrap();
        assert_eq!(asset.status(), 404, "static files are not rate limited");
        assert_eq!(
            asset.headers()["content-security-policy"],
            "frame-ancestors 'none'"
        );
        assert_eq!(asset.headers()["x-content-type-options"], "nosniff");
        assert_eq!(asset.headers()["referrer-policy"], "same-origin");
    }
}
