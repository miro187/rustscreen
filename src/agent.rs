//! Relay agent mode: capture locally, connect outbound to a RustScreen relay,
//! and serve viewers that know the 9-digit code + PIN.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use http::HeaderValue;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::capture;
use crate::cli::{Cli, ControlMode};
use crate::ffmpeg::Ffmpeg;
use crate::input::{InputEvent, InputHandle};
use crate::protocol::{encode_frame, pretty_code, relay_endpoints, AgentToRelay, RelayToAgent};
use crate::stream::{Mode, Payload, StreamManager, StreamSettings};
use crate::util::{self, Paint};

const OUTBOUND_CAPACITY: usize = 512;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READY_TIMEOUT: Duration = Duration::from_secs(25);
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Entry point for `rustscreen --relay <url>`.
pub async fn run(cli: Arc<Cli>) -> Result<()> {
    let relay_input = cli
        .relay
        .as_deref()
        .ok_or_else(|| anyhow!("missing --relay URL"))?;
    let (ws_url, viewer_base) = relay_endpoints(relay_input).ok_or_else(|| {
        anyhow!("invalid relay URL '{relay_input}' (expected https://host or http://host)")
    })?;
    let token = cli
        .relay_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "relay mode needs --relay-token (printed by rustscreen-relay on startup, \
                 or set RUSTSCREEN_RELAY_TOKEN)"
            )
        })?;

    let pin = match cli.password.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("none") => {
            eprintln!(
                "{} relay mode always requires a PIN; generating a random one",
                Paint::yellow("warning:")
            );
            util::random_pin()
        }
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => util::random_pin(),
    };
    if pin.len() < 4 {
        eprintln!(
            "{} a short PIN is easy to guess; consider a longer --password",
            Paint::yellow("warning:")
        );
    }
    if cli.audio.is_some() {
        eprintln!(
            "{} audio streaming is not supported in relay mode yet; continuing without audio",
            Paint::yellow("warning:")
        );
    }

    let ffmpeg = Arc::new(
        Ffmpeg::discover(cli.ffmpeg.clone())
            .await
            .context("ffmpeg is required to capture and encode the screen")?,
    );
    let encoders = ffmpeg.select_encoders(cli.encoder).await?;
    let source = capture::resolve_source(&cli.source).or_else(|err| {
        eprintln!(
            "{} source '{}': {err}. Falling back to the primary display.",
            Paint::yellow("warning:"),
            cli.source
        );
        capture::resolve_source("primary")
    })?;
    let settings = StreamSettings {
        source: source.id.clone(),
        fps: cli.clamped_fps(),
        bitrate_kbps: cli.clamped_bitrate(),
        scale: cli.clamped_scale(),
        encoder: None,
    }
    .sanitized();

    let streams = StreamManager::new(
        ffmpeg.clone(),
        encoders.clone(),
        settings.clone(),
        source.rect(),
        None,
    );
    let input = InputHandle::spawn();
    let hostname = util::hostname();
    let source_label = format!("{} ({}x{})", source.name, source.width, source.height);
    let encoder = encoders
        .first()
        .cloned()
        .unwrap_or_else(|| "libx264".into());
    let out_w = util::even((source.width as f32 * settings.scale).round().max(16.0) as u32).max(2);
    let out_h = util::even((source.height as f32 * settings.scale).round().max(16.0) as u32).max(2);
    let stream_line = format!(
        "{out_w}x{out_h} @ {} fps, {} kb/s, {}",
        settings.fps, settings.bitrate_kbps, encoder
    );

    print_header(
        &ws_url,
        &viewer_base,
        &pin,
        &hostname,
        &source_label,
        &stream_line,
    );

    let mut first = true;
    loop {
        if !first {
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
        first = false;
        let result = run_connection(AgentContext {
            cli: cli.clone(),
            ws_url: ws_url.clone(),
            viewer_base: viewer_base.clone(),
            token: token.clone(),
            pin: pin.clone(),
            hostname: hostname.clone(),
            source_label: source_label.clone(),
            streams: streams.clone(),
            input: input.clone(),
        })
        .await;

        match result {
            Ok(ConnectionEnd::Stopped) => break,
            Ok(ConnectionEnd::Disconnected) => {
                eprintln!(
                    "  {} {}",
                    Paint::dim("relay closed the connection;"),
                    Paint::dim("reconnecting in 3 seconds (Ctrl+C to stop)…")
                );
            }
            Err(err) => {
                eprintln!(
                    "
  {} {err}",
                    Paint::red("relay:")
                );
                eprintln!(
                    "  {}",
                    Paint::dim("reconnecting in 3 seconds (Ctrl+C to stop)…")
                );
            }
        }
    }
    input.release_all();
    Ok(())
}

enum ConnectionEnd {
    Stopped,
    Disconnected,
}

struct AgentContext {
    cli: Arc<Cli>,
    ws_url: String,
    viewer_base: String,
    token: String,
    pin: String,
    hostname: String,
    source_label: String,
    streams: Arc<StreamManager>,
    input: InputHandle,
}

async fn run_connection(ctx: AgentContext) -> Result<ConnectionEnd> {
    let mut request = ctx
        .ws_url
        .clone()
        .into_client_request()
        .context("building relay request")?;
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", ctx.token)).context("invalid relay token")?,
    );
    request.headers_mut().insert(
        "User-Agent",
        HeaderValue::from_str(&format!("rustscreen-agent/{}", env!("CARGO_PKG_VERSION")))
            .unwrap_or_else(|_| HeaderValue::from_static("rustscreen-agent")),
    );

    let (socket, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
        .await
        .map_err(|_| anyhow!("timed out connecting to relay"))?
        .context("could not connect to relay (check the URL, token and tunnel)")?;

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    send_json(
        &tx,
        &AgentToRelay::Register {
            pin: ctx.pin.clone(),
            host: ctx.hostname.clone(),
            source: ctx.source_label.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .await?;

    let mut code: Option<String> = None;
    let mut frames: Option<JoinHandle<()>> = None;
    let mut viewers = 0usize;

    // Cloudflare (and many proxies) close idle WebSockets after ~100s. Keep the
    // agent connection warm even while no viewer is attached.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome: Result<ConnectionEnd> = loop {
        let message = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                break Ok(ConnectionEnd::Stopped);
            }
            _ = heartbeat.tick() => {
                if let Ok(text) = serde_json::to_string(&AgentToRelay::Pong { ts: 0 }) {
                    let _ = tx.try_send(Message::Text(text.into()));
                }
                continue;
            }
            message = stream.next() => message,
        };
        let Some(message) = message else {
            break Ok(ConnectionEnd::Disconnected);
        };
        match message {
            Ok(Message::Text(text)) => {
                let Ok(parsed) = serde_json::from_str::<RelayToAgent>(&text) else {
                    continue;
                };
                match parsed {
                    RelayToAgent::Registered { code: assigned } => {
                        code = Some(assigned.clone());
                        viewers = 0;
                        print_code_banner(
                            &ctx.viewer_base,
                            &assigned,
                            &ctx.pin,
                            &ctx.cli,
                            &ctx.source_label,
                        );
                    }
                    RelayToAgent::ViewerJoined { name } => {
                        viewers += 1;
                        println!("  {} {name}", Paint::green("viewer connected:"));
                        if let Some(task) = frames.take() {
                            task.abort();
                        }
                        frames = Some(spawn_frame_forwarder(ctx.streams.clone(), tx.clone()));
                    }
                    RelayToAgent::ViewerLeft => {
                        viewers = 0;
                        if let Some(task) = frames.take() {
                            task.abort();
                        }
                        ctx.input.release_all();
                        println!("  {}", Paint::dim("viewer disconnected"));
                    }
                    RelayToAgent::Input { event } => {
                        if ctx.cli.control == ControlMode::None {
                            continue;
                        }
                        if viewers == 0 {
                            continue;
                        }
                        match serde_json::from_value::<InputEvent>(event) {
                            Ok(event) => {
                                tracing::debug!(?event, "relay input");
                                ctx.input.send(event, ctx.streams.input_rect());
                            }
                            Err(err) => {
                                tracing::debug!("invalid input event from viewer: {err}");
                            }
                        }
                    }
                    RelayToAgent::Ping { ts } => {
                        let payload = AgentToRelay::Pong { ts };
                        if let Ok(text) = serde_json::to_string(&payload) {
                            if tx.try_send(Message::Text(text.into())).is_err() {
                                // The writer is busy; a missed pong is harmless.
                            }
                        }
                    }
                }
            }
            Ok(Message::Binary(_)) => {}
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => break Ok(ConnectionEnd::Disconnected),
            Ok(_) => {}
            Err(err) => break Err(anyhow!("relay connection error: {err}")),
        }
    };

    if let Some(task) = frames.take() {
        task.abort();
    }
    ctx.input.release_all();
    if let Some(code) = &code {
        println!(
            "  {} code {code} is no longer valid",
            Paint::dim("relay disconnected:")
        );
    }
    writer.abort();
    outcome
}

fn spawn_frame_forwarder(streams: Arc<StreamManager>, tx: mpsc::Sender<Message>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let hub = match streams.ensure(Mode::WebCodecs) {
            Ok(hub) => hub,
            Err(err) => {
                send_error(&tx, &format!("could not start capture: {err}")).await;
                return;
            }
        };
        let meta = match hub.wait_ready(READY_TIMEOUT).await {
            Ok(meta) => meta,
            Err(err) => {
                send_error(&tx, &format!("capture is not ready: {err}")).await;
                return;
            }
        };
        let subscription = hub.subscribe();
        let codec = meta.codec.clone().unwrap_or_else(|| "avc1.42E01E".into());
        send_json_owned(
            &tx,
            &AgentToRelay::Config {
                codec,
                width: meta.width,
                height: meta.height,
                fps: meta.fps,
            },
        )
        .await;

        for frame in &subscription.initial {
            if tx
                .send(Message::Binary(encode_frame(
                    &frame.data,
                    frame.keyframe,
                    frame.ts_us,
                )))
                .await
                .is_err()
            {
                return;
            }
        }

        let mut rx = subscription.rx;
        let mut dropped = 0u64;
        loop {
            match rx.recv().await {
                Ok(frame) => match &frame.payload {
                    Payload::Media(data) => {
                        let bytes = encode_frame(data, frame.keyframe, frame.ts_us);
                        // If the relay is briefly slow we drop frames and let the
                        // next keyframe resynchronise the decoder.
                        if tx.try_send(Message::Binary(bytes)).is_err() {
                            dropped += 1;
                            if dropped == 1 {
                                tracing::warn!("relay backpressure: dropping video frames");
                            }
                        }
                    }
                    Payload::Reset => {
                        let message = AgentToRelay::Reset {
                            reason: "capture pipeline restarted".into(),
                        };
                        if let Ok(text) = serde_json::to_string(&message) {
                            let _ = tx.try_send(Message::Text(text.into()));
                        }
                        return;
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let message = AgentToRelay::Reset {
                        reason: "viewer fell behind".into(),
                    };
                    if let Ok(text) = serde_json::to_string(&message) {
                        let _ = tx.try_send(Message::Text(text.into()));
                    }
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

async fn send_json(tx: &mpsc::Sender<Message>, message: &AgentToRelay) -> Result<()> {
    let text = serde_json::to_string(message)?;
    tx.send(Message::Text(text.into()))
        .await
        .map_err(|_| anyhow!("relay connection closed"))
}

async fn send_json_owned(tx: &mpsc::Sender<Message>, message: &AgentToRelay) {
    if let Ok(text) = serde_json::to_string(message) {
        let _ = tx.send(Message::Text(text.into())).await;
    }
}

async fn send_error(tx: &mpsc::Sender<Message>, message: &str) {
    send_json_owned(
        tx,
        &AgentToRelay::Error {
            message: message.to_string(),
        },
    )
    .await;
}

fn print_header(
    ws_url: &str,
    viewer_base: &str,
    pin: &str,
    hostname: &str,
    source: &str,
    stream_line: &str,
) {
    println!();
    println!("  {}", Paint::bold(Paint::cyan("RustScreen Relay Agent")));
    println!("  {}", Paint::dim("─".repeat(58)));
    println!(
        "  {:<13} {}",
        Paint::dim("Relay"),
        ws_url.trim_end_matches("/agent")
    );
    println!("  {:<13} {}", Paint::dim("Viewer base"), viewer_base);
    println!("  {:<13} {}", Paint::dim("Host"), hostname);
    println!("  {:<13} {}", Paint::dim("Source"), source);
    println!("  {:<13} {}", Paint::dim("Stream"), stream_line);
    println!(
        "  {:<13} {}",
        Paint::dim("PIN"),
        Paint::bold(Paint::green(pin))
    );
    println!("  {}", Paint::dim("─".repeat(58)));
    println!("  Connecting to the relay…");
    println!();
}

fn print_code_banner(viewer_base: &str, code: &str, pin: &str, cli: &Cli, source: &str) {
    let viewer_url = format!("{}?code={}", viewer_base, code);
    println!();
    println!("  {}", Paint::bold(Paint::green("Ready for a viewer")));
    println!("  {}", Paint::dim("─".repeat(58)));
    println!(
        "  {:<13} {}",
        Paint::dim("Code"),
        Paint::bold(pretty_code(code))
    );
    println!(
        "  {:<13} {}",
        Paint::dim("PIN"),
        Paint::bold(Paint::green(pin))
    );
    println!(
        "  {:<13} {}",
        Paint::dim("Viewer"),
        Paint::bold(&viewer_url)
    );
    println!("  {:<13} {}", Paint::dim("Source"), source);
    println!("  {}", Paint::dim("─".repeat(58)));
    println!(
        "  {}",
        Paint::dim("Share the code and PIN. They stay valid until this agent stops.")
    );
    if !cli.no_qr && std::io::stdout().is_terminal() {
        if let Ok(qr) = crate::qr::terminal(&viewer_url) {
            println!();
            for line in qr.lines() {
                println!("   {line}");
            }
        }
    }
    println!();
    println!("  {}", Paint::dim("Waiting for a viewer…"));
    println!();
}
