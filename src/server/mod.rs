//! HTTP + WebSocket server state and routing.

mod api;
mod media;
mod ws;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::auth::{friendly_client_name, sanitize_name, Auth, Session};
use crate::cli::{Cli, ControlMode};
use crate::ffmpeg::Ffmpeg;
use crate::input::InputHandle;
use crate::net;
use crate::stream::StreamManager;

const INDEX_HTML: &str = include_str!("../../assets/index.html");
const STYLE_CSS: &str = include_str!("../../assets/style.css");
const APP_JS: &str = include_str!("../../assets/app.js");
const FAVICON_SVG: &str = include_str!("../../assets/favicon.svg");
const LOGO_SVG: &str = include_str!("../../assets/logo.svg");

pub const SESSION_COOKIE: &str = "rss_session";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlUpdate {
    pub mode: &'static str,
    pub holder: Option<String>,
}

#[derive(Debug, Clone)]
struct Holder {
    client_id: u64,
    name: String,
}

pub struct ControlState {
    mode: ControlMode,
    holder: Mutex<Option<Holder>>,
    tx: broadcast::Sender<ControlUpdate>,
}

impl ControlState {
    pub fn new(mode: ControlMode) -> Arc<Self> {
        let (tx, _) = broadcast::channel(32);
        Arc::new(Self {
            mode,
            holder: Mutex::new(None),
            tx,
        })
    }

    pub fn mode(&self) -> ControlMode {
        self.mode
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ControlUpdate> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> ControlUpdate {
        ControlUpdate {
            mode: self.mode.as_str(),
            holder: self.holder.lock().unwrap().as_ref().map(|h| h.name.clone()),
        }
    }

    pub fn holder_name(&self) -> Option<String> {
        self.holder.lock().unwrap().as_ref().map(|h| h.name.clone())
    }

    pub fn holder_client_id(&self) -> Option<u64> {
        self.holder.lock().unwrap().as_ref().map(|h| h.client_id)
    }

    pub fn allowed(&self, client_id: u64) -> bool {
        match self.mode {
            ControlMode::None => false,
            ControlMode::All => true,
            ControlMode::Single => self
                .holder
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.client_id == client_id),
        }
    }

    /// Claim exclusive control (Single mode). No-op for All mode.
    pub fn claim(&self, client_id: u64, name: &str) -> Result<(), String> {
        match self.mode {
            ControlMode::None => Err("Remote control is disabled on this host".into()),
            ControlMode::All => Ok(()),
            ControlMode::Single => {
                let mut holder = self.holder.lock().unwrap();
                match holder.as_ref() {
                    Some(current) if current.client_id != client_id => Err(format!(
                        "{} is already controlling this screen",
                        current.name
                    )),
                    Some(_) => Ok(()),
                    None => {
                        *holder = Some(Holder {
                            client_id,
                            name: name.to_string(),
                        });
                        drop(holder);
                        self.broadcast();
                        Ok(())
                    }
                }
            }
        }
    }

    /// Release control if held by `client_id`.
    pub fn release(&self, client_id: u64) {
        if self.mode != ControlMode::Single {
            return;
        }
        let released = {
            let mut holder = self.holder.lock().unwrap();
            if holder.as_ref().is_some_and(|h| h.client_id == client_id) {
                *holder = None;
                true
            } else {
                false
            }
        };
        if released {
            self.broadcast();
        }
    }

    fn broadcast(&self) {
        let _ = self.tx.send(self.snapshot());
    }
}

pub struct AppState {
    pub cli: Arc<Cli>,
    pub auth: Arc<Auth>,
    pub ffmpeg: Arc<Ffmpeg>,
    pub streams: Arc<StreamManager>,
    pub input: InputHandle,
    pub control: Arc<ControlState>,
    pub started: Instant,
    pub hostname: String,
    pub tls: bool,
    pub cert_pem: Option<Vec<u8>>,
    pub thumbnails: Mutex<HashMap<String, (Instant, Bytes)>>,
    pub next_client_id: AtomicU64,
}

impl AppState {
    pub fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
        }
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Sign in to continue")
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": self.message }))).into_response();
        if let Some(retry) = self.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&retry.to_string()).unwrap_or(HeaderValue::from_static("60")),
            );
        }
        response
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::internal(err.to_string())
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/style.css", get(style_css))
        .route("/app.js", get(app_js))
        .route("/favicon.svg", get(favicon))
        .route("/logo.svg", get(logo))
        .route("/healthz", get(healthz))
        .route("/api/preauth", get(api::preauth))
        .route("/api/login", post(api::login))
        .route("/api/logout", post(api::logout))
        .route("/api/session", get(api::session))
        .route("/api/info", get(api::info))
        .route("/api/sources", get(api::sources))
        .route("/api/thumbnail", get(api::thumbnail))
        .route("/api/settings", post(api::settings))
        .route("/api/stats", get(api::stats))
        .route("/api/qr.svg", get(api::qr_svg))
        .route("/api/cert", get(api::cert))
        .route("/api/stream", get(media::video_mse))
        .route("/api/audio", get(media::audio))
        .route("/api/ws", get(ws::video_ws))
        .route("/api/control", get(ws::control_ws))
        .fallback(fallback)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = Response::new(axum::body::Body::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn index() -> Response {
    asset(INDEX_HTML, "text/html; charset=utf-8")
}
async fn style_css() -> Response {
    asset(STYLE_CSS, "text/css; charset=utf-8")
}
async fn app_js() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}
async fn favicon() -> Response {
    asset(FAVICON_SVG, "image/svg+xml")
}
async fn logo() -> Response {
    asset(LOGO_SVG, "image/svg+xml")
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "name": "rustscreen",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn fallback(req: Request) -> Response {
    if req.uri().path().starts_with("/api/") {
        ApiError::not_found("Not found").into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            "Not found. <a href=\"/\">Open RustScreen</a>",
        )
            .into_response()
    }
}

pub fn session_cookie(token: &str, secure: bool, max_age_secs: u64) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().to_string())
}

/// Extract a session from an `Authorization: Bearer` header, `?token=` or cookie.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Option<Session> {
    let token = query_token
        .map(|t| t.to_string())
        .or_else(|| cookie_value(headers, SESSION_COOKIE))
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|v| v.trim().to_string())
        })?;
    if token.is_empty() {
        return None;
    }
    state.auth.validate(&token).await
}

pub async fn require_session(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<Session, ApiError> {
    authenticate(state, headers, query_token)
        .await
        .ok_or_else(ApiError::unauthorized)
}

/// Reject cross-site WebSocket hijacking: if an Origin header is present it
/// must match the Host header.
pub fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let origin_host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .trim_end_matches('/');
    origin_host.eq_ignore_ascii_case(host)
}

pub fn client_display_name(headers: &HeaderMap, requested: Option<String>) -> String {
    match requested {
        Some(name) if !name.trim().is_empty() => sanitize_name(&name),
        _ => {
            let ua = headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            friendly_client_name(ua)
        }
    }
}

pub fn server_url(state: &AppState, headers: &HeaderMap) -> String {
    let scheme = if state.tls { "https" } else { "http" };
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        return format!("{scheme}://{host}/");
    }
    let ip = net::preferred_lan_ip()
        .map(std::net::IpAddr::V4)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    net::base_url(state.tls, ip, state.cli.port)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; \
             script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Resolve the client IP from ConnectInfo (used for login rate limiting).
pub fn client_ip(addr: SocketAddr) -> std::net::IpAddr {
    addr.ip()
}

pub const SESSION_MAX_AGE_SECS: u64 = 24 * 60 * 60;
pub const WAIT_READY_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn parses_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; rss_session=deadbeef; b=2"),
        );
        assert_eq!(
            cookie_value(&headers, "rss_session").as_deref(),
            Some("deadbeef")
        );
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn origin_check() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("192.168.1.4:8443"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://192.168.1.4:8443"),
        );
        assert!(origin_allowed(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!origin_allowed(&headers));
    }

    #[test]
    fn single_control_lease() {
        let control = ControlState::new(ControlMode::Single);
        assert!(control.claim(1, "A").is_ok());
        assert!(control.allowed(1));
        assert!(!control.allowed(2));
        assert!(control.claim(2, "B").is_err());
        control.release(1);
        assert!(control.claim(2, "B").is_ok());
        assert_eq!(control.holder_name().as_deref(), Some("B"));
    }

    #[test]
    fn disabled_control() {
        let control = ControlState::new(ControlMode::None);
        assert!(control.claim(1, "A").is_err());
        assert!(!control.allowed(1));
    }
}
