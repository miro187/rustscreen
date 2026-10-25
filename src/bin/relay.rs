//! RustScreen Relay — pairs browser viewers with outbound agents using a
//! short one-time code, and forwards H.264 frames plus input events.
//!
//! This binary is intended to run behind a reverse proxy or a Cloudflare
//! Tunnel (it speaks plain HTTP/WS on localhost). Viewer authentication is
//! code + PIN; agents must present a shared relay token.

#[path = "../protocol.rs"]
mod protocol;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use clap::Parser;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use protocol::*;

const INDEX_HTML: &str = include_str!("../../relay-web/index.html");
const APP_JS: &str = include_str!("../../relay-web/app.js");
const RELAY_CSS: &str = include_str!("../../relay-web/relay.css");
const STYLE_CSS: &str = include_str!("../../assets/style.css");
const LOGO_SVG: &str = include_str!("../../assets/logo.svg");
const FAVICON_SVG: &str = include_str!("../../assets/favicon.svg");

const VIEWER_CHANNEL_CAPACITY: usize = 128;
const MAX_AGENT_MESSAGE: usize = 16 * 1024 * 1024;
const MAX_VIEWER_MESSAGE: usize = 256 * 1024;
const MAX_FAILURES: u32 = 12;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[command(
    name = "rustscreen-relay",
    version,
    about = "Public relay for RustScreen agents and browser viewers",
    long_about = None
)]
struct Args {
    /// Address to listen on. Keep 127.0.0.1 when running behind a tunnel.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// Port to listen on
    #[arg(long, default_value_t = 8090)]
    port: u16,

    /// Shared secret agents must present (auto-generated if omitted)
    #[arg(long, env = "RUSTSCREEN_RELAY_TOKEN")]
    token: Option<String>,

    /// Maximum number of simultaneously registered agents
    #[arg(long, default_value_t = 64)]
    max_agents: usize,
}

struct Inner {
    token: String,
    agents: Mutex<HashMap<String, AgentEntry>>,
    attempts: Mutex<HashMap<String, Attempts>>,
    next_viewer: AtomicU64,
    max_agents: usize,
}

struct AgentEntry {
    pin: String,
    host: String,
    source: String,
    version: String,
    to_agent: mpsc::UnboundedSender<String>,
    viewer: Option<ViewerEntry>,
    #[allow(dead_code)]
    connected: Instant,
}

struct ViewerEntry {
    id: u64,
    to_viewer: mpsc::Sender<ServerToViewer>,
}

#[derive(Debug, Clone)]
enum ServerToViewer {
    Text(String),
    Binary(Bytes),
}

#[derive(Debug, Default, Clone)]
struct Attempts {
    count: u32,
    first: Option<Instant>,
}

struct PairOutcome {
    host: String,
    source: String,
    version: String,
    to_agent: mpsc::UnboundedSender<String>,
    to_viewer: mpsc::Receiver<ServerToViewer>,
    viewer_id: u64,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    init_tracing();

    let token = args.token.unwrap_or_else(random_token);
    let state = Arc::new(Inner {
        token: token.clone(),
        agents: Mutex::new(HashMap::new()),
        attempts: Mutex::new(HashMap::new()),
        next_viewer: AtomicU64::new(1),
        max_agents: args.max_agents.clamp(1, 1024),
    });

    let app = router(state.clone());
    let addr = SocketAddr::new(args.bind, args.port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("error: could not listen on {addr}: {err}");
            std::process::exit(1);
        }
    };

    println!();
    println!("  RustScreen Relay");
    println!("  ──────────────────────────────────────────────────────────");
    println!("  Listening      http://{}", listener.local_addr().unwrap());
    println!("  Agent token    {token}");
    println!("  Max agents     {}", state.max_agents);
    println!("  ──────────────────────────────────────────────────────────");
    println!("  Start agents with:");
    println!("    rustscreen --relay <public-url> --relay-token <token>");
    println!("  Public URL must be a Cloudflare Tunnel or reverse proxy in front of");
    println!(
        "  this port. Without a public URL, use http://127.0.0.1:{} locally.",
        args.port
    );
    println!();

    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        result = server => {
            if let Err(err) = result {
                eprintln!("server error: {err}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n  relay stopped");
        }
    }
}

fn router(state: Arc<Inner>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/relay.css", get(relay_css))
        .route("/logo.svg", get(logo))
        .route("/favicon.svg", get(favicon))
        .route("/healthz", get(health))
        .route("/agent", get(agent_ws))
        .route("/viewer", get(viewer_ws))
        .fallback(not_found)
        .with_state(state)
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .try_init();
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut response = Response::new(axum::body::Body::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    security_headers(response)
}

fn security_headers(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; media-src 'self' blob:; \
             script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    response
}

async fn index() -> Response {
    asset(INDEX_HTML, "text/html; charset=utf-8")
}
async fn app_js() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}
async fn style_css() -> Response {
    asset(STYLE_CSS, "text/css; charset=utf-8")
}
async fn relay_css() -> Response {
    asset(RELAY_CSS, "text/css; charset=utf-8")
}
async fn logo() -> Response {
    asset(LOGO_SVG, "image/svg+xml")
}
async fn favicon() -> Response {
    asset(FAVICON_SVG, "image/svg+xml")
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "service": "rustscreen-relay", "version": env!("CARGO_PKG_VERSION") }))
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not found").into_response()
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(supplied) = value.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(supplied.trim().as_bytes(), token.as_bytes())
}

fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
    if let Some(value) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(ip) = value.trim().parse() {
            return ip;
        }
    }
    if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = value.split(',').next() {
            if let Ok(ip) = first.trim().parse() {
                return ip;
            }
        }
    }
    addr.ip()
}

async fn agent_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Inner>>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "invalid relay token").into_response();
    }
    ws.max_message_size(MAX_AGENT_MESSAGE)
        .max_frame_size(MAX_AGENT_MESSAGE)
        .on_upgrade(move |socket| agent_task(socket, state))
}

async fn agent_task(socket: WebSocket, state: Arc<Inner>) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut code: Option<String> = None;
    let mut registered = false;

    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let parsed: AgentToRelay = match serde_json::from_str(&text) {
                    Ok(parsed) => parsed,
                    Err(_) => continue,
                };
                match parsed {
                    AgentToRelay::Register {
                        pin,
                        host,
                        source,
                        version,
                    } => {
                        if registered {
                            continue;
                        }
                        let Some(pin) = validate_pin(&pin) else {
                            let _ = tx
                                .send(json!({"type":"error","message":"Invalid PIN"}).to_string());
                            break;
                        };
                        let host = clean_label(&host, 64);
                        let source = clean_label(&source, 120);
                        let version = clean_label(&version, 32);

                        let assigned = {
                            let mut agents = state.agents.lock().unwrap();
                            if agents.len() >= state.max_agents {
                                None
                            } else {
                                let code = generate_code(&agents);
                                agents.insert(
                                    code.clone(),
                                    AgentEntry {
                                        pin,
                                        host,
                                        source,
                                        version,
                                        to_agent: tx.clone(),
                                        viewer: None,
                                        connected: Instant::now(),
                                    },
                                );
                                Some(code)
                            }
                        };
                        let Some(assigned_code) = assigned else {
                            let _ = tx.send(
                                json!({"type":"error","message":"Relay is full"}).to_string(),
                            );
                            break;
                        };
                        registered = true;
                        let message = serde_json::to_string(&RelayToAgent::Registered {
                            code: assigned_code.clone(),
                        })
                        .unwrap_or_default();
                        let _ = tx.send(message);
                        tracing::info!(code = %assigned_code, "agent registered");
                        code = Some(assigned_code);
                    }
                    other => {
                        if !registered {
                            continue;
                        }
                        let Some(viewer_tx) = current_viewer(&state, code.as_deref()) else {
                            continue;
                        };
                        if let Some(viewer_message) = other.to_viewer() {
                            let payload = match serde_json::to_string(&viewer_message) {
                                Ok(payload) => payload,
                                Err(_) => continue,
                            };
                            if viewer_tx.try_send(ServerToViewer::Text(payload)).is_err() {
                                drop_viewer(&state, code.as_deref(), "viewer is too slow");
                            }
                        }
                    }
                }
            }
            Ok(Message::Binary(data)) => {
                if !registered {
                    continue;
                }
                let Some(viewer_tx) = current_viewer(&state, code.as_deref()) else {
                    continue;
                };
                if let Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) =
                    viewer_tx.try_send(ServerToViewer::Binary(data))
                {
                    drop_viewer(&state, code.as_deref(), "viewer is too slow");
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    writer.abort();
    if let Some(code) = code {
        let removed = state.agents.lock().unwrap().remove(&code);
        if let Some(entry) = removed {
            if let Some(viewer) = entry.viewer {
                let _ = viewer.to_viewer.try_send(ServerToViewer::Text(
                    json!({"type":"error","message":"The host went offline"}).to_string(),
                ));
                // Dropping `viewer` closes the channel, which closes the viewer socket.
            }
        }
        tracing::info!(code, "agent disconnected");
    }
}

fn current_viewer(state: &Inner, code: Option<&str>) -> Option<mpsc::Sender<ServerToViewer>> {
    let code = code?;
    let agents = state.agents.lock().unwrap();
    agents
        .get(code)
        .and_then(|agent| agent.viewer.as_ref())
        .map(|viewer| viewer.to_viewer.clone())
}

fn drop_viewer(state: &Inner, code: Option<&str>, reason: &str) {
    let Some(code) = code else { return };
    let mut agents = state.agents.lock().unwrap();
    let Some(agent) = agents.get_mut(code) else {
        return;
    };
    if agent.viewer.take().is_some() {
        let _ = agent
            .to_agent
            .send(serde_json::to_string(&RelayToAgent::ViewerLeft).unwrap_or_default());
        tracing::info!(code, reason, "viewer dropped");
    }
}

fn clear_viewer(state: &Inner, code: &str, viewer_id: u64) {
    let mut agents = state.agents.lock().unwrap();
    let Some(agent) = agents.get_mut(code) else {
        return;
    };
    let matches = agent
        .viewer
        .as_ref()
        .is_some_and(|viewer| viewer.id == viewer_id);
    if matches {
        agent.viewer = None;
        let _ = agent
            .to_agent
            .send(serde_json::to_string(&RelayToAgent::ViewerLeft).unwrap_or_default());
    }
}

async fn viewer_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Inner>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&headers, addr);
    ws.max_message_size(MAX_VIEWER_MESSAGE)
        .max_frame_size(MAX_VIEWER_MESSAGE)
        .on_upgrade(move |socket| viewer_task(socket, state, ip))
}

async fn viewer_task(socket: WebSocket, state: Arc<Inner>, ip: IpAddr) {
    let (mut sender, mut receiver) = socket.split();

    let first = tokio::time::timeout(Duration::from_secs(15), receiver.next()).await;
    let connect = match first {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<ViewerToRelay>(&text).ok(),
        _ => None,
    };
    let Some(ViewerToRelay::Connect { code, pin, name }) = connect else {
        let _ = deny(&mut sender, "Send a connection request first").await;
        return;
    };

    let code = normalize_code(&code);
    if code.len() != 9 {
        let _ = deny(&mut sender, "Enter the 9-digit code shown on the host").await;
        return;
    }
    let name = clean_label(
        if name.trim().is_empty() {
            "Browser viewer"
        } else {
            &name
        },
        48,
    );

    if let Err(reason) = check_rate_limit(&state, ip) {
        let _ = deny(&mut sender, &reason).await;
        return;
    }

    let outcome = match pair_viewer(&state, &code, &pin) {
        Ok(outcome) => outcome,
        Err(reason) => {
            note_failure(&state, ip);
            let _ = deny(&mut sender, &reason).await;
            return;
        }
    };
    clear_failures(&state, ip);

    let paired = serde_json::to_string(&RelayToViewer::Paired {
        host: outcome.host.clone(),
        source: outcome.source.clone(),
        version: outcome.version.clone(),
    })
    .unwrap_or_default();
    if sender.send(Message::Text(paired.into())).await.is_err() {
        clear_viewer(&state, &code, outcome.viewer_id);
        return;
    }

    let viewer_id = outcome.viewer_id;
    let to_agent = outcome.to_agent.clone();
    let vrx = outcome.to_viewer;

    let writer = tokio::spawn(async move {
        let mut vrx = vrx;
        while let Some(message) = vrx.recv().await {
            let result = match message {
                ServerToViewer::Text(text) => sender.send(Message::Text(text.into())).await,
                ServerToViewer::Binary(data) => sender.send(Message::Binary(data)).await,
            };
            if result.is_err() {
                break;
            }
        }
        let _ = sender.close().await;
    });

    let _ = to_agent.send(
        serde_json::to_string(&RelayToAgent::ViewerJoined { name: name.clone() })
            .unwrap_or_default(),
    );
    tracing::info!(code = %code, viewer = %name, ip = %ip, "viewer paired");

    let guard = ViewerGuard {
        state: state.clone(),
        code: code.clone(),
        viewer_id,
    };

    while let Some(message) = receiver.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let Ok(parsed) = serde_json::from_str::<ViewerToRelay>(&text) else {
                    continue;
                };
                let forwarded = match parsed {
                    ViewerToRelay::Input { event } => {
                        serde_json::to_string(&RelayToAgent::Input { event })
                    }
                    ViewerToRelay::Ping { ts } => serde_json::to_string(&RelayToAgent::Ping { ts }),
                    ViewerToRelay::Connect { .. } => Ok(String::new()),
                };
                if let Ok(payload) = forwarded {
                    if !payload.is_empty() {
                        let _ = to_agent.send(payload);
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    drop(guard);
    writer.abort();
    tracing::info!(code = %code, viewer = %name, "viewer disconnected");
}

struct ViewerGuard {
    state: Arc<Inner>,
    code: String,
    viewer_id: u64,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        clear_viewer(&self.state, &self.code, self.viewer_id);
    }
}

async fn deny(sender: &mut SplitSink<WebSocket, Message>, reason: &str) -> Result<(), axum::Error> {
    let payload = serde_json::to_string(&RelayToViewer::Denied {
        reason: reason.to_string(),
    })
    .unwrap_or_else(|_| "{\"type\":\"denied\",\"reason\":\"Connection rejected\"}".to_string());
    let _ = sender.send(Message::Text(payload.into())).await;
    let _ = sender.close().await;
    Ok(())
}

fn pair_viewer(state: &Inner, code: &str, pin: &str) -> Result<PairOutcome, String> {
    let mut agents = state.agents.lock().unwrap();
    let agent = agents
        .get_mut(code)
        .ok_or_else(|| "No computer is online with that code".to_string())?;
    if agent.viewer.is_some() {
        return Err("Someone is already connected to this computer".to_string());
    }
    if !ct_eq(agent.pin.trim().as_bytes(), pin.trim().as_bytes()) {
        return Err("Wrong PIN".to_string());
    }
    let (to_viewer, from_relay) = mpsc::channel(VIEWER_CHANNEL_CAPACITY);
    let viewer_id = state.next_viewer.fetch_add(1, Ordering::Relaxed);
    agent.viewer = Some(ViewerEntry {
        id: viewer_id,
        to_viewer,
    });
    Ok(PairOutcome {
        host: agent.host.clone(),
        source: agent.source.clone(),
        version: agent.version.clone(),
        to_agent: agent.to_agent.clone(),
        to_viewer: from_relay,
        viewer_id,
    })
}

fn check_rate_limit(state: &Inner, ip: IpAddr) -> Result<(), String> {
    let mut attempts = state.attempts.lock().unwrap();
    let entry = attempts.entry(ip.to_string()).or_default();
    if let Some(first) = entry.first {
        if first.elapsed() > FAILURE_WINDOW {
            entry.count = 0;
            entry.first = None;
        }
    }
    if entry.count >= MAX_FAILURES {
        let remaining = entry
            .first
            .map(|first| FAILURE_WINDOW.saturating_sub(first.elapsed()).as_secs())
            .unwrap_or(60)
            .max(1);
        return Err(format!("Too many attempts. Try again in {remaining}s."));
    }
    Ok(())
}

fn note_failure(state: &Inner, ip: IpAddr) {
    let mut attempts = state.attempts.lock().unwrap();
    let entry = attempts.entry(ip.to_string()).or_default();
    if entry.first.is_none() {
        entry.first = Some(Instant::now());
    }
    entry.count = entry.count.saturating_add(1);
}

fn clear_failures(state: &Inner, ip: IpAddr) {
    state.attempts.lock().unwrap().remove(&ip.to_string());
}

fn generate_code(agents: &HashMap<String, AgentEntry>) -> String {
    let mut rng = rand::thread_rng();
    loop {
        let value = rng.next_u32() % 1_000_000_000;
        let code = format!("{value:09}");
        if !agents.contains_key(&code) {
            return code;
        }
    }
}

fn random_token() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 24];
    rng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn validate_pin(pin: &str) -> Option<String> {
    let pin = pin.trim();
    if pin.is_empty() || pin.len() > 64 || pin.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(pin.to_string())
}

fn clean_label(label: &str, max: usize) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_fit_nine_digits() {
        let agents = HashMap::new();
        for _ in 0..500 {
            let code = generate_code(&agents);
            assert_eq!(code.len(), 9);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn labels_are_sanitized() {
        assert_eq!(clean_label("  hello\u{0}world  ", 24), "helloworld");
        assert_eq!(clean_label("", 8), "Unknown");
        assert!(validate_pin("123456").is_some());
        assert!(validate_pin("").is_none());
    }

    #[test]
    fn rate_limit_blocks() {
        let state = Inner {
            token: "t".into(),
            agents: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            next_viewer: AtomicU64::new(1),
            max_agents: 1,
        };
        let ip: IpAddr = "192.168.1.2".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            note_failure(&state, ip);
        }
        assert!(check_rate_limit(&state, ip).is_err());
        clear_failures(&state, ip);
        assert!(check_rate_limit(&state, ip).is_ok());
    }
}
