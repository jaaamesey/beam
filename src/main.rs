mod capture;
mod config;
mod rtc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::SET_COOKIE},
    response::IntoResponse,
    routing::{get, post},
};
use config::Config;
use rand::{Rng, distr::Alphanumeric};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::RwLock};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

const ADDRESS: &str = "0.0.0.0:9470";
const COOKIE: &str = "beam_session";
type HttpResult<T> = Result<T, (StatusCode, String)>;

struct App {
    config: RwLock<Config>,
    sessions: RwLock<HashMap<String, Instant>>,
    media: Arc<rtc::Media>,
}

#[derive(Deserialize)]
struct Password {
    password: String,
}

#[derive(Deserialize)]
struct Offer {
    sdp: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("beam=info".parse()?),
        )
        .init();

    let app = Arc::new(App {
        config: RwLock::new(Config::load()?),
        sessions: RwLock::new(HashMap::new()),
        media: rtc::Media::new()?,
    });

    let files = ServeDir::new("web/dist").not_found_service(ServeFile::new("web/dist/index.html"));
    let router = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/session", post(login))
        .route("/api/offer", post(offer))
        .route("/api/admin/settings", get(get_settings).put(put_settings))
        .fallback_service(files)
        .layer(TraceLayer::new_for_http())
        .with_state(app);

    let listener = TcpListener::bind(ADDRESS).await?;
    tracing::info!(address = ADDRESS, "Beam is ready");
    if let Err(error) = open::that("http://127.0.0.1:9470/settings") {
        tracing::warn!(%error, "could not open settings in the browser");
    }
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn login(
    State(app): State<Arc<App>>,
    Json(input): Json<Password>,
) -> HttpResult<impl IntoResponse> {
    let valid: bool = input
        .password
        .as_bytes()
        .ct_eq(app.config.read().await.password.as_bytes())
        .into();
    if !valid {
        tokio::time::sleep(Duration::from_millis(250)).await;
        return Err((StatusCode::UNAUTHORIZED, "Incorrect password".into()));
    }
    let token: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    app.sessions
        .write()
        .await
        .insert(token.clone(), Instant::now() + Duration::from_secs(86_400));
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400"
        ))
        .unwrap(),
    );
    Ok((headers, Json(serde_json::json!({ "ok": true }))))
}

async fn offer(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<Offer>,
) -> HttpResult<Json<webrtc::peer_connection::sdp::session_description::RTCSessionDescription>> {
    require_session(&app, &headers).await?;
    app.media
        .answer(input.sdp)
        .await
        .map(Json)
        .map_err(internal)
}

async fn get_settings(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
) -> HttpResult<Json<Config>> {
    require_loopback(address)?;
    Ok(Json(app.config.read().await.clone()))
}

async fn put_settings(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    Json(input): Json<Password>,
) -> HttpResult<Json<serde_json::Value>> {
    require_loopback(address)?;
    if input.password.len() < 12 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Password must be at least 12 characters".into(),
        ));
    }
    let config = Config {
        password: input.password,
    };
    config.save().map_err(internal)?;
    *app.config.write().await = config;
    app.sessions.write().await.clear();
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn require_loopback(address: SocketAddr) -> HttpResult<()> {
    address.ip().is_loopback().then_some(()).ok_or((
        StatusCode::FORBIDDEN,
        "Host settings are available only on this machine".into(),
    ))
}

async fn require_session(app: &App, headers: &HeaderMap) -> HttpResult<()> {
    let token = headers
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .map(str::trim)
                .find_map(|cookie| cookie.strip_prefix(&format!("{COOKIE}=")))
        });
    let valid = match token {
        Some(token) => app
            .sessions
            .read()
            .await
            .get(token)
            .is_some_and(|expiry| *expiry > Instant::now()),
        None => false,
    };
    valid
        .then_some(())
        .ok_or((StatusCode::UNAUTHORIZED, "Authentication required".into()))
}

fn internal(error: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!(%error, "request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal server error".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_api_accepts_only_loopback() {
        assert!(require_loopback("127.0.0.1:1".parse().unwrap()).is_ok());
        assert!(require_loopback("[::1]:1".parse().unwrap()).is_ok());
        assert_eq!(
            require_loopback("192.168.1.10:1".parse().unwrap())
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
    }
}
