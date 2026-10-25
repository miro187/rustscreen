//! WebSocket endpoints: WebCodecs video transport and the input/control channel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use super::{authenticate, origin_allowed, ApiError, AppState, ControlUpdate, WAIT_READY_TIMEOUT};
use crate::auth::Session;
use crate::cli::ControlMode;
use crate::input::InputEvent;
use crate::stream::{Mode, Payload};

const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const FRAME_HEADER_LEN: usize = 16;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn video_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> Response {
    let Some(session) = authenticate(&state, &headers, query.token.as_deref()).await else {
        return ApiError::unauthorized().into_response();
    };
    if !origin_allowed(&headers) {
        return ApiError::forbidden("Cross-origin WebSocket rejected").into_response();
    }
    ws.max_message_size(MAX_MESSAGE_SIZE)
        .max_frame_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| video_task(socket, state, session))
}

async fn video_task(socket: WebSocket, state: Arc<AppState>, _session: Session) {
    let (mut sender, mut receiver) = socket.split();

    if state.streams.total_viewers() >= state.cli.clamped_max_viewers() {
        let _ = send_json(
            &mut sender,
            json!({"type":"error","message":"Viewer limit reached on this host"}),
        )
        .await;
        let _ = sender.close().await;
        return;
    }

    let hub = match state.streams.ensure(Mode::WebCodecs) {
        Ok(hub) => hub,
        Err(err) => {
            let _ = send_json(
                &mut sender,
                json!({"type":"error","message":err.to_string()}),
            )
            .await;
            let _ = sender.close().await;
            return;
        }
    };

    let meta = match hub.wait_ready(WAIT_READY_TIMEOUT).await {
        Ok(meta) => meta,
        Err(err) => {
            let _ = send_json(
                &mut sender,
                json!({"type":"error","message":err.to_string()}),
            )
            .await;
            let _ = sender.close().await;
            return;
        }
    };

    let subscription = hub.subscribe();
    let codec = meta.codec.clone().unwrap_or_else(|| "avc1.42E01E".into());
    let config = json!({
        "type": "config",
        "transport": "webcodecs",
        "codec": codec,
        "width": meta.width,
        "height": meta.height,
        "fps": meta.fps,
        "serverTimeMs": now_ms(),
    });
    if send_json(&mut sender, config).await.is_err() {
        return;
    }

    let interval = 1_000_000u64 / meta.fps.max(1) as u64;
    let mut last_ts = 0u64;
    for frame in &subscription.initial {
        if send_binary(&mut sender, &frame.data, frame.keyframe, frame.ts_us)
            .await
            .is_err()
        {
            return;
        }
        last_ts = last_ts.max(frame.ts_us);
    }

    let mut rx = subscription.rx;

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
            frame = rx.recv() => {
                match frame {
                    Ok(frame) => {
                        if let Payload::Media(data) = &frame.payload {
                            // Timestamps must strictly increase for VideoDecoder.
                            let ts = frame.ts_us.max(last_ts + 1).max(last_ts + interval);
                            last_ts = ts;
                            if send_binary(&mut sender, data, frame.keyframe, ts).await.is_err() {
                                break;
                            }
                        } else {
                            let _ = send_json(&mut sender, json!({"type":"reset","reason":"stream restarted"})).await;
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        let _ = send_json(&mut sender, json!({
                            "type":"reset",
                            "reason": format!("viewer fell behind ({n} frames)")
                        })).await;
                        break;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
    let _ = sender.close().await;
}

async fn send_binary(
    sender: &mut SplitSink<WebSocket, Message>,
    data: &Bytes,
    keyframe: bool,
    ts_us: u64,
) -> Result<(), axum::Error> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + data.len());
    out.extend_from_slice(b"RS\x01");
    out.push(if keyframe { 1 } else { 0 });
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&ts_us.to_le_bytes());
    out.extend_from_slice(data);
    sender.send(Message::Binary(Bytes::from(out))).await
}

async fn send_json(
    sender: &mut SplitSink<WebSocket, Message>,
    value: Value,
) -> Result<(), axum::Error> {
    sender.send(Message::Text(value.to_string().into())).await
}

pub async fn control_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> Response {
    let Some(session) = authenticate(&state, &headers, query.token.as_deref()).await else {
        return ApiError::unauthorized().into_response();
    };
    if !origin_allowed(&headers) {
        return ApiError::forbidden("Cross-origin WebSocket rejected").into_response();
    }
    ws.max_message_size(MAX_MESSAGE_SIZE)
        .max_frame_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| control_task(socket, state, session))
}

async fn control_task(socket: WebSocket, state: Arc<AppState>, session: Session) {
    let (mut sender, mut receiver) = socket.split();
    let client_id = state.next_client_id();
    let name = session.client_name.clone();
    let mut updates = state.control.subscribe();

    let hello = json!({
        "type": "hello",
        "clientId": client_id,
        "name": name,
        "controlMode": state.control.mode().as_str(),
        "holder": state.control.holder_name(),
        "settings": state.streams.settings(),
        "hostname": state.hostname,
        "version": env!("CARGO_PKG_VERSION"),
        "audio": state.streams.audio_enabled(),
        "viewers": state.streams.total_viewers(),
        "controlAllowed": state.control.allowed(client_id),
    });
    if send_json(&mut sender, hello).await.is_err() {
        return;
    }

    let mut idle = Box::pin(tokio::time::sleep(IDLE_TIMEOUT));
    let mut sent_input = false;
    let mut window_started = Instant::now();
    let mut events_in_window = 0u32;
    let mut last_denied: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = &mut idle => {
                tracing::debug!(client = %name, "control websocket idle timeout");
                break;
            }
            update = updates.recv() => {
                match update {
                    Ok(update) => {
                        let you = state.control.holder_client_id() == Some(client_id);
                        if send_control_update(&mut sender, &update, you).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
            message = receiver.next() => {
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
                let Some(message) = message else { break };
                let text = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    let _ = send_error(&mut sender, "Malformed JSON message").await;
                    continue;
                };
                let kind = value.get("t").and_then(Value::as_str).unwrap_or_default().to_string();
                match kind.as_str() {
                    "ping" => {
                        let ts = value.get("ts").and_then(Value::as_i64).unwrap_or(0);
                        let _ = send_json(&mut sender, json!({
                            "type": "pong",
                            "ts": ts,
                            "serverTime": now_ms(),
                        })).await;
                    }
                    "controlRequest" => {
                        match state.control.claim(client_id, &name) {
                            Ok(()) => {
                                let _ = send_json(&mut sender, json!({
                                    "type": "controlAck",
                                    "granted": true,
                                    "mode": state.control.mode().as_str(),
                                })).await;
                                if !state.control.allowed(client_id) {
                                    // Nothing else to do: All mode grants implicitly.
                                }
                            }
                            Err(reason) => {
                                let _ = send_json(&mut sender, json!({
                                    "type": "controlDenied",
                                    "reason": reason,
                                })).await;
                            }
                        }
                    }
                    "controlRelease" => {
                        state.control.release(client_id);
                        if sent_input {
                            state.input.release_all();
                            sent_input = false;
                        }
                        let _ = send_json(&mut sender, json!({
                            "type": "controlAck",
                            "granted": false,
                            "mode": state.control.mode().as_str(),
                        })).await;
                    }
                    "releaseAll" => {
                        state.input.release_all();
                        sent_input = false;
                    }
                    "move" | "down" | "up" | "wheel" | "key" | "text" => {
                        if window_started.elapsed() > Duration::from_secs(2) {
                            window_started = Instant::now();
                            events_in_window = 0;
                        }
                        events_in_window += 1;
                        if events_in_window > 800 {
                            if last_denied.map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(true) {
                                last_denied = Some(Instant::now());
                                let _ = send_error(&mut sender, "Input rate limit exceeded").await;
                            }
                            continue;
                        }
                        if !state.control.allowed(client_id) {
                            let claimed = state.control.mode() == ControlMode::Single
                                && state.control.claim(client_id, &name).is_ok();
                            if !claimed {
                                if last_denied.map(|t| t.elapsed() > Duration::from_secs(2)).unwrap_or(true) {
                                    last_denied = Some(Instant::now());
                                    let reason = state.control.holder_name().map(|h| format!("{h} is controlling this screen"));
                                    let _ = send_json(&mut sender, json!({
                                        "type": "controlDenied",
                                        "reason": reason.unwrap_or_else(|| "Remote control is disabled on this host".into()),
                                    })).await;
                                }
                                continue;
                            }
                        }
                        match serde_json::from_value::<InputEvent>(value) {
                            Ok(event) => {
                                sent_input = true;
                                state.input.send(event, state.streams.input_rect());
                            }
                            Err(err) => {
                                let _ = send_error(&mut sender, &format!("Invalid input event: {err}")).await;
                            }
                        }
                    }
                    other => {
                        let _ = send_error(&mut sender, &format!("Unknown message type '{other}'")).await;
                    }
                }
            }
        }
    }

    state.control.release(client_id);
    if sent_input {
        state.input.release_all();
    }
    let _ = sender.close().await;
}

async fn send_control_update(
    sender: &mut SplitSink<WebSocket, Message>,
    update: &ControlUpdate,
    you: bool,
) -> Result<(), axum::Error> {
    send_json(
        sender,
        json!({
            "type": "control",
            "mode": update.mode,
            "holder": update.holder,
            "you": you,
        }),
    )
    .await
}

async fn send_error(
    sender: &mut SplitSink<WebSocket, Message>,
    message: &str,
) -> Result<(), axum::Error> {
    send_json(sender, json!({ "type": "error", "message": message })).await
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_layout() {
        let data = Bytes::from_static(b"abc");
        let mut out = Vec::new();
        out.extend_from_slice(b"RS\x01");
        out.push(1);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&123u64.to_le_bytes());
        out.extend_from_slice(&data);
        assert_eq!(out.len(), FRAME_HEADER_LEN + 3);
        assert_eq!(u64::from_le_bytes(out[8..16].try_into().unwrap()), 123);
    }
}
