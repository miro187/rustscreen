//! REST API handlers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    client_display_name, client_ip, cookie_value, require_session, server_url, session_cookie,
    ApiError, AppState, ControlUpdate, SESSION_COOKIE, SESSION_MAX_AGE_SECS,
};
use crate::auth::LoginError;
use crate::capture;
use crate::stream::StreamSettings;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub client_name: String,
}

impl From<&crate::auth::Session> for SessionView {
    fn from(session: &crate::auth::Session) -> Self {
        Self {
            client_name: session.client_name.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreauthResponse {
    pub hostname: String,
    pub os: &'static str,
    pub version: &'static str,
    pub auth_required: bool,
    pub tls: bool,
    pub control_mode: &'static str,
    pub audio: bool,
}

pub async fn preauth(State(state): State<Arc<AppState>>) -> Json<PreauthResponse> {
    Json(PreauthResponse {
        hostname: state.hostname.clone(),
        os: crate::util::os_name(),
        version: env!("CARGO_PKG_VERSION"),
        auth_required: state.auth.requires_auth(),
        tls: state.tls,
        control_mode: state.control.mode().as_str(),
        audio: state.streams.audio_enabled(),
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub token: String,
    pub session: SessionView,
    pub settings: StreamSettings,
    pub control: ControlUpdate,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let client_name = client_display_name(&headers, body.name.clone());
    let session = state
        .auth
        .login(client_ip(addr), &body.password, client_name.clone())
        .await
        .map_err(|err| match err {
            LoginError::InvalidPassword => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Wrong PIN or password. Check the code shown on the host.",
            ),
            LoginError::TooManyAttempts { retry_after_secs } => {
                let mut e = ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Too many attempts. Try again in {retry_after_secs}s."),
                );
                e.retry_after = Some(retry_after_secs);
                e
            }
        })?;

    tracing::info!(client = %client_name, ip = %addr.ip(), "viewer signed in");

    let payload = LoginResponse {
        token: session.token.clone(),
        session: SessionView::from(&session),
        settings: state.streams.settings(),
        control: state.control.snapshot(),
    };
    let cookie = session_cookie(&session.token, state.tls, SESSION_MAX_AGE_SECS);
    let mut response = Json(payload).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| ApiError::internal("bad cookie"))?,
    );
    Ok(response)
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        state.auth.logout(&token).await;
    }
    let cookie = super::clear_cookie(state.tls);
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| ApiError::internal("bad cookie"))?,
    );
    Ok(response)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    pub session: SessionView,
    pub settings: StreamSettings,
    pub control: ControlUpdate,
    pub hostname: String,
    pub version: &'static str,
    pub tls: bool,
}

pub async fn session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, ApiError> {
    let session = require_session(&state, &headers, None).await?;
    Ok(Json(SessionResponse {
        session: SessionView::from(&session),
        settings: state.streams.settings(),
        control: state.control.snapshot(),
        hostname: state.hostname.clone(),
        version: env!("CARGO_PKG_VERSION"),
        tls: state.tls,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioInfo {
    pub enabled: bool,
    pub device: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoResponse {
    pub hostname: String,
    pub os: &'static str,
    pub version: &'static str,
    pub tls: bool,
    pub auth_required: bool,
    pub control: ControlUpdate,
    pub session: SessionView,
    pub settings: StreamSettings,
    pub encoders: Vec<String>,
    pub ffmpeg_version: String,
    pub audio: AudioInfo,
    pub streams: Vec<crate::stream::StreamStats>,
    pub viewers: usize,
    pub max_viewers: usize,
    pub sessions: usize,
    pub uptime_secs: u64,
    pub max_bitrate_kbps: u32,
}

pub async fn info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<InfoResponse>, ApiError> {
    let session = require_session(&state, &headers, None).await?;
    let streams = state.streams.stats();
    Ok(Json(InfoResponse {
        hostname: state.hostname.clone(),
        os: crate::util::os_name(),
        version: env!("CARGO_PKG_VERSION"),
        tls: state.tls,
        auth_required: state.auth.requires_auth(),
        control: state.control.snapshot(),
        session: SessionView::from(&session),
        settings: state.streams.settings(),
        encoders: state.streams.encoders(),
        ffmpeg_version: state.ffmpeg.version.clone(),
        audio: AudioInfo {
            enabled: state.streams.audio_enabled(),
            device: state.streams.audio_device().map(str::to_string),
        },
        streams,
        viewers: state.streams.total_viewers(),
        max_viewers: state.cli.clamped_max_viewers(),
        sessions: state.auth.session_count().await,
        uptime_secs: state.started.elapsed().as_secs(),
        max_bitrate_kbps: 50_000,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcesResponse {
    pub monitors: Vec<capture::SourceInfo>,
    pub windows: Vec<capture::SourceInfo>,
    pub current: String,
}

pub async fn sources(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SourcesResponse>, ApiError> {
    require_session(&state, &headers, None).await?;
    let include_demo = state.streams.settings().source == "demo";
    let (monitors, windows) =
        tokio::task::spawn_blocking(move || capture::list_sources_with_demo(include_demo))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| ApiError::unavailable(e.to_string()))?;
    Ok(Json(SourcesResponse {
        monitors,
        windows,
        current: state.streams.settings().source,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ThumbQuery {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub w: Option<u32>,
}

pub async fn thumbnail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    require_session(&state, &headers, None).await?;
    if query.id.trim().is_empty() {
        return Err(ApiError::bad_request("missing source id"));
    }
    let width = query.w.unwrap_or(320).clamp(120, 960);
    let key = format!("{}:{}", query.id, width);

    {
        let cache = state.thumbnails.lock().unwrap();
        if let Some((at, bytes)) = cache.get(&key) {
            if at.elapsed() < Duration::from_secs(8) {
                return Ok(jpeg_response(bytes.clone()));
            }
        }
    }

    let id = query.id.clone();
    let (source, desktop) = tokio::task::spawn_blocking(move || {
        let source = capture::resolve_source(&id)?;
        let desktop = capture::primary_desktop_rect()?;
        anyhow::Ok((source, desktop))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let space = capture::desktop_space(&state.ffmpeg, desktop).await;
    let plan = capture::CapturePlan::build(source, space, 1.0)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let jpeg = capture::thumbnail(&state.ffmpeg, &plan, width)
        .await
        .map_err(|e| ApiError::unavailable(format!("thumbnail failed: {e}")))?;

    let bytes = Bytes::from(jpeg);
    {
        let mut cache = state.thumbnails.lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
        if cache.len() > 96 {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), bytes.clone()));
    }
    Ok(jpeg_response(bytes))
}

fn jpeg_response(bytes: Bytes) -> Response {
    let mut response = Response::new(axum::body::Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=8"),
    );
    response
}

pub async fn settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(patch): Json<crate::stream::SettingsPatch>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _session = require_session(&state, &headers, None).await?;
    if !state.control.mode().allows_control() {
        return Err(ApiError::forbidden(
            "Control is disabled on this host; stream settings are locked.",
        ));
    }
    let settings = state
        .streams
        .update_settings(patch)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({ "ok": true, "settings": settings })))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsResponse {
    pub uptime_secs: u64,
    pub viewers: usize,
    pub max_viewers: usize,
    pub control: ControlUpdate,
    pub streams: Vec<crate::stream::StreamStats>,
    pub bitrate_kbps: f64,
    pub fps: f64,
}

pub async fn stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<StatsResponse>, ApiError> {
    require_session(&state, &headers, None).await?;
    let streams = state.streams.stats();
    let bitrate = streams.iter().map(|s| s.bitrate_kbps).sum();
    let fps = streams.iter().map(|s| s.fps).fold(0.0_f64, f64::max);
    Ok(Json(StatsResponse {
        uptime_secs: state.started.elapsed().as_secs(),
        viewers: state.streams.total_viewers(),
        max_viewers: state.cli.clamped_max_viewers(),
        control: state.control.snapshot(),
        streams,
        bitrate_kbps: bitrate,
        fps,
    }))
}

pub async fn qr_svg(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let url = server_url(&state, &headers);
    let svg = crate::qr::svg(&url, 260).map_err(|e| ApiError::internal(e.to_string()))?;
    let mut response = Response::new(axum::body::Body::from(svg));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub async fn cert(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let Some(pem) = &state.cert_pem else {
        return Err(ApiError::not_found("TLS is not enabled"));
    };
    let mut response = Response::new(axum::body::Body::from(pem.clone()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-pem-file"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"rustscreen-cert.pem\""),
    );
    Ok(response)
}
