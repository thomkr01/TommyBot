mod access;
mod api;
mod appeals;
mod auth;
mod diff;
mod discord;
mod lockdown;
mod ops;
mod recovery;
mod schema;
mod sections;
mod store;
mod verify;

use axum::{
    http::{header::CACHE_CONTROL, HeaderValue, StatusCode},
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

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub discord: discord::Discord,
    pub secure: bool,
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden(&'static str),
    /// Needs a fresh TOTP code (or enrollment) before this write.
    StepUp,
    NotFound,
    Conflict(i32),
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
            ApiError::Conflict(v) => (
                StatusCode::CONFLICT,
                json!({"error": "config changed, reload", "current_version": v}),
            ),
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
        .route(&format!("{g}/config"), get(api::get_config))
        .route(&format!("{g}/config/{{section}}"), put(api::put_section))
        .route(&format!("{g}/permits"), put(api::put_permits))
        .route(&format!("{g}/history"), get(api::history))
        .route(
            &format!("{g}/history/{{from}}/{{to}}"),
            get(api::diff_versions),
        )
        .route(
            &format!("{g}/history/{{version}}/rollback"),
            post(api::rollback),
        )
        .merge(sections::routes())
        .merge(ops::routes())
        .merge(lockdown::routes())
        .merge(recovery::routes())
        .merge(appeals::routes())
        .merge(verify::routes())
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
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(st)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let env = |k: &str| std::env::var(k).map_err(|_| anyhow::anyhow!("{k} not set"));
    let base = env("DASHBOARD_URL")?;
    let db = tommybot_core::db::connect(&env("DATABASE_URL")?).await?;
    let st = AppState {
        db,
        secure: base.starts_with("https://"),
        discord: discord::Discord {
            http: reqwest::Client::new(),
            client_id: env("DISCORD_CLIENT_ID")?,
            client_secret: env("DISCORD_CLIENT_SECRET")?,
            bot_token: env("DISCORD_TOKEN")?,
            redirect_uri: format!("{}/auth/callback", base.trim_end_matches('/')),
        },
    };
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
