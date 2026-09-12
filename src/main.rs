#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod capture;
mod config;
mod audio;
mod input;
mod rtc;
mod tls;
mod tray;

use anyhow::Result;
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::IntoResponse,
    routing::{get, post},
};
use config::Config;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    net::{SocketAddr, UdpSocket},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    path::PathBuf,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use tokio_rustls::TlsAcceptor;
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use tracing_subscriber::fmt::MakeWriter;

const PORT: u16 = 9470;
const COOKIE: &str = "beam_session";
const RESTART_CHILD: &str = "--beam-restart-child";
type HttpResult<T> = Result<T, (StatusCode, String)>;

struct App {
    config: RwLock<Config>,
    sessions: RwLock<HashMap<String, Instant>>,
    media: Arc<rtc::Media>,
    shutdown: Arc<AtomicBool>,
    network_address: String,
    logs: LogBuffer,
}

const MAX_LOG_BYTES: usize = 100_000;

#[derive(Clone, Default)]
struct LogBuffer {
    bytes: Arc<Mutex<VecDeque<u8>>>,
}

struct LogWriter {
    logs: LogBuffer,
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter { logs: self.clone() }
    }
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        io::stderr().write_all(bytes)?;
        let mut log = self.logs.bytes.lock().unwrap();
        log.extend(bytes);
        while log.len() > MAX_LOG_BYTES {
            log.pop_front();
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

#[derive(Deserialize, Serialize)]
struct Password {
    password: String,
}

#[derive(Deserialize)]
struct SettingsUpdate {
    password: String,
    persistent_sessions: Option<bool>,
}

#[derive(Serialize)]
struct Settings {
    password: String,
    address: String,
    persistent_sessions: bool,
}

#[derive(Serialize)]
struct Logs {
    logs: String,
}

#[derive(Deserialize)]
struct Offer {
    sdp: String,
}

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.first().is_some_and(|argument| argument == RESTART_CHILD) {
        wait_for_port(PORT);
        return replace_process(arguments.into_iter().skip(1).collect());
    }
    if arguments.iter().any(|argument| argument == "--beam-self-test-restarted") {
        println!("restart self-test passed: second process generation started");
        return Ok(());
    }
    if arguments.iter().any(|argument| argument == "--beam-self-test-watcher-restarted") {
        println!("restart watcher self-test passed: second process generation started");
        return Ok(());
    }
    if arguments.iter().any(|argument| argument == "--beam-self-test-watcher") {
        spawn_restart_watcher()?;
        return Ok(());
    }
    if arguments.iter().any(|argument| argument == "--beam-self-test-restart") {
        return restart_process_with_args(["--beam-self-test-restarted"]);
    }
    let logs = LogBuffer::default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("beam=info".parse()?),
        )
        .with_writer(logs.clone())
        .init();

    let runtime = tokio::runtime::Runtime::new()?;
    let listener = runtime.block_on(TcpListener::bind(("0.0.0.0", PORT)))?;
    let bound_address = listener.local_addr()?;
    let config = Config::load()?;
    let persistent_sessions = config.persistent_sessions;
    let network_address = format!("http://{}:{PORT}", local_ip());
    let open_settings = !Config::settings_opened()?;
    let settings_url = format!("https://127.0.0.1:{PORT}/settings#{}", config.admin_token);
    let first_settings_url = settings_url.replacen("https://", "http://", 1);
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    // Linux portals serialize permission requests. Establish persistent input
    // before starting persistent capture so the input prompt cannot be delayed
    // behind the screen-capture portal session.
    let persistent_input = persistent_sessions.then(input::new).transpose()?;
    let input_session = input::spawn(input_rx, persistent_input, shutdown_flag.clone());
    let app = Arc::new(App {
        config: RwLock::new(config),
        sessions: RwLock::new(HashMap::new()),
        media: rtc::Media::new(input_tx, persistent_sessions)?,
        shutdown: shutdown_flag.clone(),
        network_address,
        logs,
    });

    let web_root = resource_path("web/dist");
    let secure = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/session", post(login))
        .route("/api/offer", post(offer))
        .route("/api/admin/settings", get(get_settings).put(put_settings))
        .route("/api/admin/logs", get(get_logs))
        .route("/api/admin/settings-opened", post(settings_opened))
        .route("/api/admin/shutdown", post(shutdown))
        .route("/api/admin/restart", post(restart))
        .fallback_service(
            ServeDir::new(&web_root).not_found_service(ServeFile::new(web_root.join("index.html"))),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(app.clone());
    let welcome_router = if open_settings {
        Router::new()
            .route("/api/admin/settings", get(get_settings).put(put_settings))
            .route("/api/admin/logs", get(get_logs))
            .route("/api/admin/settings-opened", post(settings_opened))
            .route("/api/admin/shutdown", post(shutdown))
            .route("/api/admin/restart", post(restart))
            .fallback_service(
                ServeDir::new(&web_root)
                    .not_found_service(ServeFile::new(web_root.join("index.html"))),
            )
            .with_state(app.clone())
    } else {
        Router::new().fallback(welcome).with_state(app.clone())
    };
    let tls = tls::acceptor()?;
    runtime.spawn(run_server(listener, tls, secure, welcome_router));
    tracing::info!(address = %bound_address, "Beam is ready");
    if open_settings && let Err(error) = open::that(&first_settings_url) {
        tracing::warn!(%error, "could not open settings in the browser");
    }
    let result = tray::run(settings_url, shutdown_flag);
    runtime.block_on(app.media.shutdown());
    input_session.shutdown();
    drop(app);
    drop(runtime);
    result
}

pub(crate) fn spawn_restart_watcher() -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg(RESTART_CHILD)
        .args(std::env::args_os().skip(1));
    if std::env::args().any(|argument| argument == "--beam-self-test-watcher") {
        command.arg("--beam-self-test-watcher-restarted");
    }
    command
        .spawn()
        .map_err(|error| anyhow::anyhow!("start Beam restart watcher: {error}"))?;
    Ok(())
}

fn wait_for_port(port: u16) {
    while std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn restart_process_with_args<const N: usize>(extra_args: [&str; N]) -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(std::env::args_os().skip(1));
    command.args(extra_args);
    replace_command(command)
}

fn replace_process(arguments: Vec<std::ffi::OsString>) -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(arguments);
    replace_command(command)
}

fn replace_command(mut command: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        return Err(anyhow::anyhow!("restart Beam server: {error}"));
    }
    #[cfg(not(unix))]
    {
        command
            .spawn()
            .map_err(|error| anyhow::anyhow!("restart Beam server: {error}"))?;
        Ok(())
    }
}

fn resource_path(relative: &str) -> PathBuf {
    let development = PathBuf::from(relative);
    if development.exists() {
        return development;
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .and_then(|macos| macos.parent().map(PathBuf::from))
        .map(|contents| contents.join("Resources").join(relative))
        .unwrap_or_else(|| PathBuf::from(relative))
}

async fn run_server(listener: TcpListener, tls: TlsAcceptor, secure: Router, welcome: Router) {
    loop {
        let (stream, address) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::error!(%error, "could not accept connection");
                continue;
            }
        };
        let (tls, secure, welcome) = (tls.clone(), secure.clone(), welcome.clone());
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, address, tls, secure, welcome).await {
                tracing::debug!(%address, %error, "connection closed");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    address: SocketAddr,
    tls: TlsAcceptor,
    secure: Router,
    welcome: Router,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut first = [0];
    tokio::time::timeout(Duration::from_secs(5), stream.peek(&mut first)).await??;
    if first[0] == 22 {
        let stream = tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await??;
        serve_connection(stream, address, secure).await
    } else {
        serve_connection(stream, address, welcome).await
    }
}

async fn serve_connection<I>(stream: I, address: SocketAddr, router: Router) -> Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = router.layer(Extension(ConnectInfo(address)));
    Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(stream), TowerToHyperService::new(service))
        .await
        .map_err(|error| anyhow::anyhow!("serve connection: {error}"))?;
    Ok(())
}

async fn welcome(method: Method, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<axum::http::uri::Authority>().ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = uri.path_and_query().map_or("/", |value| value.as_str());
    let target = format!("https://{host}{path}").replace('&', "&amp;");
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action https:",
            ),
        ],
        format!(
            r#"<!doctype html><meta name="viewport" content="width=device-width"><title>Beam</title><style>body{{margin:0;background:#080b10;color:#eef2ff;font:16px system-ui;display:grid;place-items:center;min-height:100vh}}main{{max-width:32rem;padding:2rem}}a{{display:inline-block;margin-top:1rem;padding:.8rem 1rem;border-radius:.7rem;background:#67e8f9;color:#082f49;text-decoration:none;font-weight:700}}</style><main><h1>Continue securely</h1><p>Beam uses a self-signed certificate, so your browser will show a warning the first time.</p><a href="{target}">Open Beam over HTTPS</a></main>"#
        ),
    )
        .into_response()
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
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}={token}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400"
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
    headers: HeaderMap,
) -> HttpResult<Json<Settings>> {
    require_admin(&app, address, &headers).await?;
    Ok(Json(Settings {
        password: app.config.read().await.password.clone(),
        address: app.network_address.clone(),
        persistent_sessions: app.config.read().await.persistent_sessions,
    }))
}

async fn get_logs(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> HttpResult<Json<Logs>> {
    require_admin(&app, address, &headers).await?;
    let bytes: Vec<u8> = app.logs.bytes.lock().unwrap().iter().copied().collect();
    Ok(Json(Logs {
        logs: String::from_utf8_lossy(&bytes).into_owned(),
    }))
}

async fn put_settings(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<SettingsUpdate>,
) -> HttpResult<Json<serde_json::Value>> {
    require_admin(&app, address, &headers).await?;
    if input.password.len() < 4 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Password must be at least 4 characters".into(),
        ));
    }
    let current = app.config.read().await.clone();
    let config = Config {
        password: input.password,
        admin_token: current.admin_token,
        persistent_sessions: input.persistent_sessions.unwrap_or(current.persistent_sessions),
    };
    config.save().map_err(internal)?;
    *app.config.write().await = config;
    app.sessions.write().await.clear();
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn shutdown(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> HttpResult<Json<serde_json::Value>> {
    require_admin(&app, address, &headers).await?;
    let shutdown = app.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
    });
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn restart(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> HttpResult<Json<serde_json::Value>> {
    require_admin(&app, address, &headers).await?;
    spawn_restart_watcher().map_err(internal)?;
    let shutdown = app.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
    });
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn settings_opened(
    State(app): State<Arc<App>>,
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> HttpResult<Json<serde_json::Value>> {
    require_admin(&app, address, &headers).await?;
    Config::mark_settings_opened().map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn require_admin(app: &App, address: SocketAddr, headers: &HeaderMap) -> HttpResult<()> {
    require_loopback(address)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let valid: bool = token
        .as_bytes()
        .ct_eq(app.config.read().await.admin_token.as_bytes())
        .into();
    valid
        .then_some(())
        .ok_or((StatusCode::UNAUTHORIZED, "Browser is not authorized".into()))
}

fn require_loopback(address: SocketAddr) -> HttpResult<()> {
    address.ip().is_loopback().then_some(()).ok_or((
        StatusCode::FORBIDDEN,
        "Host settings are available only on this machine".into(),
    ))
}

fn local_ip() -> std::net::IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:80")?;
            socket.local_addr()
        })
        .map(|address| address.ip())
        .unwrap_or_else(|_| "127.0.0.1".parse().unwrap())
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

    #[test]
    fn restart_waits_until_the_server_port_is_released() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(listener);
        });
        let started = Instant::now();
        wait_for_port(port);
        release.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
    }
}
