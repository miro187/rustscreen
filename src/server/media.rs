//! Long-lived media endpoints: fragmented MP4 over HTTP for MSE playback
//! (video and optional audio).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use super::{authenticate, ApiError, AppState, WAIT_READY_TIMEOUT};
use crate::stream::{Mode, Payload, Subscription};

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn video_mse(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let _session = authenticate(&state, &headers, query.token.as_deref())
        .await
        .ok_or_else(ApiError::unauthorized)?;
    stream_response(state, Mode::Mse).await
}

pub async fn audio(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let _session = authenticate(&state, &headers, query.token.as_deref())
        .await
        .ok_or_else(ApiError::unauthorized)?;
    if !state.streams.audio_enabled() {
        return Err(ApiError::not_found(
            "Audio is not enabled. Restart the host with --audio.",
        ));
    }
    stream_response(state, Mode::Audio).await
}

async fn stream_response(state: Arc<AppState>, mode: Mode) -> Result<Response, ApiError> {
    let hub = state
        .streams
        .ensure(mode)
        .map_err(|e| ApiError::unavailable(e.to_string()))?;

    let meta = hub
        .wait_ready(WAIT_READY_TIMEOUT)
        .await
        .map_err(|e| ApiError::unavailable(e.to_string()))?;

    if state.streams.total_viewers() >= state.cli.clamped_max_viewers() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "This host already has {} viewer(s). Try again later.",
                state.cli.clamped_max_viewers()
            ),
        ));
    }

    let subscription = hub.subscribe();
    let body = make_body(subscription);

    let codec = meta.codec.clone().unwrap_or_default();
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if mode == Mode::Audio {
            "audio/mp4"
        } else {
            "video/mp4"
        }),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-transform"),
    );
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers.insert("x-transport", HeaderValue::from_static("mse"));
    headers.insert(
        "x-video-codec",
        HeaderValue::from_str(&codec).unwrap_or_else(|_| HeaderValue::from_static("avc1.42E01E")),
    );
    headers.insert(
        "x-video-width",
        HeaderValue::from_str(&meta.width.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        "x-video-height",
        HeaderValue::from_str(&meta.height.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        "x-video-fps",
        HeaderValue::from_str(&meta.fps.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    if mode == Mode::Audio {
        headers.insert("x-audio-codec", HeaderValue::from_static("mp4a.40.2"));
    }
    Ok(response)
}

/// Turn a hub subscription into an HTTP body stream. Initialization data and
/// the latest keyframe are emitted first, then live fragments follow.
fn make_body(subscription: Subscription) -> Body {
    let Subscription {
        rx,
        initial,
        _guard,
        ..
    } = subscription;

    let initial_stream = stream::iter(
        initial
            .into_iter()
            .map(|frame| Ok::<Bytes, std::io::Error>(frame.data)),
    );
    let live = stream::unfold((rx, _guard), |(mut rx, guard)| async move {
        match rx.recv().await {
            Ok(frame) => {
                if let Payload::Media(bytes) = &frame.payload {
                    let bytes = bytes.clone();
                    return Some((Ok::<Bytes, std::io::Error>(bytes), (rx, guard)));
                }
                // Reset frames terminate the response so the browser
                // reconnects and fetches a fresh init segment.
                None
            }
            Err(RecvError::Lagged(_)) => Some((
                Err(std::io::Error::other(
                    "viewer fell behind; reconnect to resume",
                )),
                (rx, guard),
            )),
            Err(RecvError::Closed) => None,
        }
    });
    Body::from_stream(initial_stream.chain(live))
}
