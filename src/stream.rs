//! Live streaming engine: manages ffmpeg pipelines, fragment parsing and
//! fan-out to viewers.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::capture::{self, CapturePlan, Rect};
use crate::ffmpeg::{self, Ffmpeg, OutputMode};
use crate::h264::{AnnexBFrame, AnnexBSplitter};
use crate::mp4::{Mp4Event, Mp4Splitter};

const BROADCAST_CAPACITY: usize = 256;
const READ_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Raw Annex-B H.264 over a WebSocket, decoded with WebCodecs.
    WebCodecs,
    /// Fragmented MP4 over HTTP, played through Media Source Extensions.
    Mse,
    /// Optional audio-only fMP4 stream.
    Audio,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::WebCodecs => "webcodecs",
            Mode::Mse => "mse",
            Mode::Audio => "audio",
        }
    }
    fn output_mode(self) -> OutputMode {
        match self {
            Mode::WebCodecs => OutputMode::AnnexB,
            Mode::Mse => OutputMode::Mp4,
            Mode::Audio => OutputMode::Mp4,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamMeta {
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSettings {
    pub source: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub scale: f32,
    pub encoder: Option<String>,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            source: "primary".into(),
            fps: 30,
            bitrate_kbps: 6000,
            scale: 1.0,
            encoder: None,
        }
    }
}

impl StreamSettings {
    pub fn sanitized(mut self) -> Self {
        self.fps = self.fps.clamp(5, 60);
        self.bitrate_kbps = self.bitrate_kbps.clamp(500, 50_000);
        if !self.scale.is_finite() {
            self.scale = 1.0;
        }
        self.scale = self.scale.clamp(0.25, 1.0);
        if self.source.trim().is_empty() {
            self.source = "primary".into();
        }
        self
    }

    fn key(&self, encoder: &str) -> String {
        format!(
            "{}|{}|{}|{:.3}|{}",
            self.source, self.fps, self.bitrate_kbps, self.scale, encoder
        )
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SettingsPatch {
    pub source: Option<String>,
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub scale: Option<f32>,
    pub encoder: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Payload {
    Media(Bytes),
    Reset,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub keyframe: bool,
    pub ts_us: u64,
    pub payload: Payload,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStats {
    pub mode: String,
    pub viewers: usize,
    pub frames: u64,
    pub bitrate_kbps: f64,
    pub fps: f64,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps_target: u32,
    pub error: Option<String>,
}

struct RateMeter {
    last_bytes: u64,
    last_frames: u64,
    last_at: Instant,
    bitrate_kbps: f64,
    fps: f64,
}

impl RateMeter {
    fn new() -> Self {
        Self {
            last_bytes: 0,
            last_frames: 0,
            last_at: Instant::now(),
            bitrate_kbps: 0.0,
            fps: 0.0,
        }
    }
    fn sample(&mut self, bytes: u64, frames: u64) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_at).as_secs_f64();
        if dt >= 0.2 {
            let db = bytes.saturating_sub(self.last_bytes) as f64;
            let df = frames.saturating_sub(self.last_frames) as f64;
            self.bitrate_kbps = db * 8.0 / 1000.0 / dt;
            self.fps = df / dt;
            self.last_bytes = bytes;
            self.last_frames = frames;
            self.last_at = now;
        }
    }
}

struct HubState {
    meta: StreamMeta,
    init: Option<Bytes>,
    last_frag: Option<Bytes>,
    last_key: Option<Bytes>,
    last_key_ts_us: u64,
    current_frag: Option<Bytes>,
    error: Option<String>,
}

pub struct Hub {
    mode: Mode,
    tx: broadcast::Sender<Arc<Frame>>,
    state: Mutex<HubState>,
    bytes: AtomicU64,
    frames: AtomicU64,
    viewers: AtomicU64,
    meter: Mutex<RateMeter>,
}

pub struct SubscriberGuard {
    hub: Arc<Hub>,
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.hub.viewers.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct InitialFrame {
    pub data: Bytes,
    pub ts_us: u64,
    pub keyframe: bool,
}

pub struct Subscription {
    pub rx: broadcast::Receiver<Arc<Frame>>,
    /// Frames that must be sent before live data (init segment + latest keyframe).
    pub initial: Vec<InitialFrame>,
    pub _guard: SubscriberGuard,
}

impl Hub {
    fn new(mode: Mode, fps: u32) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            mode,
            tx,
            state: Mutex::new(HubState {
                meta: StreamMeta {
                    codec: None,
                    width: 0,
                    height: 0,
                    fps,
                },
                init: None,
                last_frag: None,
                last_key: None,
                last_key_ts_us: 0,
                current_frag: None,
                error: None,
            }),
            bytes: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            viewers: AtomicU64::new(0),
            meter: Mutex::new(RateMeter::new()),
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn viewers(&self) -> usize {
        self.viewers.load(Ordering::Relaxed) as usize
    }

    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let st = self.state.lock().unwrap();
        let rx = self.tx.subscribe();
        let mut initial = Vec::new();
        match self.mode {
            Mode::Mse | Mode::Audio => {
                if let Some(init) = &st.init {
                    initial.push(InitialFrame {
                        data: init.clone(),
                        ts_us: 0,
                        keyframe: false,
                    });
                }
                if let Some(frag) = &st.last_frag {
                    initial.push(InitialFrame {
                        data: frag.clone(),
                        ts_us: 0,
                        keyframe: true,
                    });
                }
            }
            Mode::WebCodecs => {
                if let Some(key) = &st.last_key {
                    initial.push(InitialFrame {
                        data: key.clone(),
                        ts_us: st.last_key_ts_us,
                        keyframe: true,
                    });
                }
            }
        }
        self.viewers.fetch_add(1, Ordering::Relaxed);
        drop(st);
        let guard = SubscriberGuard { hub: self.clone() };
        Subscription {
            rx,
            initial,
            _guard: guard,
        }
    }

    pub fn meta(&self) -> StreamMeta {
        self.state.lock().unwrap().meta.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// Prepare the hub for a (re)starting pipeline.
    fn begin(&self, width: u32, height: u32, fps: u32) {
        let mut st = self.state.lock().unwrap();
        st.error = None;
        st.init = None;
        st.last_frag = None;
        st.last_key = None;
        st.last_key_ts_us = 0;
        st.current_frag = None;
        st.meta.width = width;
        st.meta.height = height;
        st.meta.fps = fps;
        if self.mode == Mode::Audio {
            st.meta.codec = Some("mp4a.40.2".into());
        } else {
            st.meta.codec = None;
        }
    }

    fn reset(&self, error: Option<String>) {
        let mut st = self.state.lock().unwrap();
        st.init = None;
        st.last_frag = None;
        st.last_key = None;
        st.last_key_ts_us = 0;
        st.current_frag = None;
        st.error = error;
        let frame = Arc::new(Frame {
            keyframe: false,
            ts_us: 0,
            payload: Payload::Reset,
        });
        drop(st);
        let _ = self.tx.send(frame);
    }

    fn publish(&self, payload: Payload, keyframe: bool, ts_us: u64) {
        let st = self.state.lock().unwrap();
        let frame = Arc::new(Frame {
            keyframe,
            ts_us,
            payload,
        });
        let len = match &frame.payload {
            Payload::Media(bytes) => bytes.len() as u64,
            Payload::Reset => 0,
        };
        self.bytes.fetch_add(len, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
        drop(st);
        let _ = self.tx.send(frame);
    }

    /// Publish a parsed access unit (WebCodecs path).
    fn publish_au(&self, au: AnnexBFrame, ts_us: u64) {
        let keyframe = au.keyframe;
        {
            let mut st = self.state.lock().unwrap();
            if let Some(codec) = &au.codec {
                st.meta.codec = Some(codec.clone());
            }
            if keyframe {
                st.last_key = Some(au.data.clone());
                st.last_key_ts_us = ts_us;
            }
        }
        self.publish(Payload::Media(au.data), keyframe, ts_us);
    }

    /// Publish one complete top-level MP4 box (MSE path).
    fn publish_box(&self, typ: [u8; 4], data: Bytes) {
        let mut st = self.state.lock().unwrap();
        match &typ {
            b"moof" => {
                st.last_frag = st.current_frag.take();
                st.current_frag = Some(data.clone());
            }
            b"mdat" => {
                if let Some(current) = &mut st.current_frag {
                    let mut buf =
                        bytes::BytesMut::with_capacity(current.len().saturating_add(data.len()));
                    buf.extend_from_slice(current);
                    buf.extend_from_slice(&data);
                    *current = buf.freeze();
                    // The fragment is complete once its mdat has been parsed.
                    st.last_frag = Some(current.clone());
                }
            }
            b"styp" => {
                st.current_frag = Some(data.clone());
            }
            _ => {}
        }
        let keyframe = &typ == b"moof";
        drop(st);
        let frame = Arc::new(Frame {
            keyframe,
            ts_us: 0,
            payload: Payload::Media(data),
        });
        self.frames.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(frame);
    }

    fn set_init(&self, init: Bytes) {
        if let Some(codec) = crate::mp4::codec_from_init(&init) {
            self.set_codec(&codec);
        }
        let mut st = self.state.lock().unwrap();
        st.init = Some(init);
    }

    fn set_codec(&self, codec: &str) {
        let mut st = self.state.lock().unwrap();
        st.meta.codec = Some(codec.to_string());
    }

    /// Wait until the stream is ready to be consumed by a browser.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<StreamMeta> {
        let started = Instant::now();
        loop {
            {
                let st = self.state.lock().unwrap();
                let ready = match self.mode {
                    Mode::Mse => st.init.is_some() && st.meta.codec.is_some(),
                    Mode::WebCodecs => st.meta.codec.is_some() && st.last_key.is_some(),
                    Mode::Audio => st.init.is_some(),
                };
                if ready {
                    return Ok(st.meta.clone());
                }
                if let Some(err) = &st.error {
                    return Err(anyhow!(err.clone()));
                }
            }
            if started.elapsed() > timeout {
                let err = self
                    .error()
                    .unwrap_or_else(|| "timed out waiting for the video encoder".to_string());
                return Err(anyhow!(err));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn stats(&self) -> StreamStats {
        let meta = self.meta();
        let error = self.error();
        let bytes = self.bytes.load(Ordering::Relaxed);
        let frames = self.frames.load(Ordering::Relaxed);
        let mut meter = self.meter.lock().unwrap();
        meter.sample(bytes, frames);
        StreamStats {
            mode: self.mode.as_str().to_string(),
            viewers: self.viewers(),
            frames,
            bitrate_kbps: meter.bitrate_kbps,
            fps: meter.fps,
            codec: meta.codec,
            width: meta.width,
            height: meta.height,
            fps_target: meta.fps,
            error,
        }
    }
}

struct Slot {
    hub: Arc<Hub>,
    cancel: CancellationToken,
    key: String,
    last_used: Instant,
}

pub struct StreamManager {
    ffmpeg: Arc<Ffmpeg>,
    encoders: Vec<String>,
    settings: RwLock<StreamSettings>,
    slots: Mutex<HashMap<Mode, Slot>>,
    input_rect: RwLock<Rect>,
    audio_device: Option<String>,
}

impl StreamManager {
    pub fn new(
        ffmpeg: Arc<Ffmpeg>,
        encoders: Vec<String>,
        settings: StreamSettings,
        input_rect: Rect,
        audio_device: Option<String>,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            ffmpeg,
            encoders,
            settings: RwLock::new(settings),
            slots: Mutex::new(HashMap::new()),
            input_rect: RwLock::new(input_rect),
            audio_device,
        });
        spawn_idle_reaper(manager.clone());
        manager
    }

    pub fn settings(&self) -> StreamSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn encoders(&self) -> Vec<String> {
        self.encoders.clone()
    }

    pub fn input_rect(&self) -> Rect {
        *self.input_rect.read().unwrap()
    }

    pub fn audio_enabled(&self) -> bool {
        self.audio_device.is_some()
    }

    pub fn audio_device(&self) -> Option<&str> {
        self.audio_device.as_deref()
    }

    pub fn update_settings(&self, patch: SettingsPatch) -> Result<StreamSettings> {
        let mut current = self.settings.read().unwrap().clone();
        if let Some(source) = patch.source {
            let trimmed = source.trim().to_string();
            if trimmed.is_empty() {
                return Err(anyhow!("source must not be empty"));
            }
            // Validate now so users get a useful error instead of a broken stream.
            capture::resolve_source(&trimmed)?;
            current.source = trimmed;
        }
        if let Some(fps) = patch.fps {
            current.fps = fps;
        }
        if let Some(bitrate) = patch.bitrate_kbps {
            current.bitrate_kbps = bitrate;
        }
        if let Some(scale) = patch.scale {
            current.scale = scale;
        }
        if let Some(encoder) = patch.encoder {
            let encoder = encoder.trim().to_string();
            if encoder.is_empty() {
                current.encoder = None;
            } else if self.encoders.iter().any(|e| e == &encoder) {
                current.encoder = Some(encoder);
            } else {
                return Err(anyhow!(
                    "encoder '{encoder}' is not available (available: {})",
                    self.encoders.join(", ")
                ));
            }
        }
        let sanitized = current.sanitized();
        let changed = {
            let old = self.settings.read().unwrap();
            old.source != sanitized.source
                || old.fps != sanitized.fps
                || old.bitrate_kbps != sanitized.bitrate_kbps
                || (old.scale - sanitized.scale).abs() > f32::EPSILON
                || old.encoder != sanitized.encoder
        };
        if changed {
            *self.settings.write().unwrap() = sanitized.clone();
            self.stop_all();
            tracing::info!(
                source = %sanitized.source,
                fps = sanitized.fps,
                bitrate = sanitized.bitrate_kbps,
                scale = sanitized.scale,
                "stream settings changed; restarting pipeline"
            );
        }
        Ok(sanitized)
    }

    fn encoder_for(&self, settings: &StreamSettings) -> String {
        settings
            .encoder
            .clone()
            .filter(|e| self.encoders.iter().any(|x| x == e))
            .unwrap_or_else(|| {
                self.encoders
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "libx264".to_string())
            })
    }

    /// Make sure a pipeline is running and return its hub.
    pub fn ensure(self: &Arc<Self>, mode: Mode) -> Result<Arc<Hub>> {
        if mode == Mode::Audio && !self.audio_enabled() {
            return Err(anyhow!("audio streaming is disabled (start with --audio)"));
        }
        let settings = self.settings();
        let encoder = self.encoder_for(&settings);
        let key = if mode == Mode::Audio {
            format!("audio|{}", self.audio_device.as_deref().unwrap_or(""))
        } else {
            settings.key(&encoder)
        };

        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&mode) {
            if slot.key == key {
                slot.last_used = Instant::now();
                return Ok(slot.hub.clone());
            }
        }
        if let Some(old) = slots.remove(&mode) {
            old.cancel.cancel();
        }

        let hub = Hub::new(mode, settings.fps);
        let cancel = CancellationToken::new();
        slots.insert(
            mode,
            Slot {
                hub: hub.clone(),
                cancel: cancel.clone(),
                key,
                last_used: Instant::now(),
            },
        );
        drop(slots);

        match mode {
            Mode::Audio => {
                let manager = self.clone();
                let hub2 = hub.clone();
                tokio::spawn(async move {
                    manager.supervise_audio(hub2, cancel).await;
                });
            }
            _ => {
                let manager = self.clone();
                let hub2 = hub.clone();
                tokio::spawn(async move {
                    manager
                        .supervise_video(hub2, settings, encoder, cancel)
                        .await;
                });
            }
        }
        Ok(hub)
    }

    pub fn stop_all(&self) {
        let mut slots = self.slots.lock().unwrap();
        for (_, slot) in slots.drain() {
            slot.cancel.cancel();
        }
    }

    pub fn shutdown(&self) {
        self.stop_all();
    }

    pub fn stats(&self) -> Vec<StreamStats> {
        let slots = self.slots.lock().unwrap();
        slots.values().map(|s| s.hub.stats()).collect()
    }

    pub fn total_viewers(&self) -> usize {
        let slots = self.slots.lock().unwrap();
        slots.values().map(|s| s.hub.viewers()).sum()
    }

    async fn supervise_video(
        self: Arc<Self>,
        hub: Arc<Hub>,
        settings: StreamSettings,
        initial_encoder: String,
        cancel: CancellationToken,
    ) {
        let mut encoder_index = self
            .encoders
            .iter()
            .position(|e| e == &initial_encoder)
            .unwrap_or(0);
        let mut consecutive_failures = 0u32;
        let mut backoff = Duration::from_millis(500);

        loop {
            if cancel.is_cancelled() {
                break;
            }
            let encoder = self
                .encoders
                .get(encoder_index)
                .cloned()
                .unwrap_or_else(|| "libx264".to_string());

            // Resolve the capture geometry (monitors can come and go).
            let plan = match self.resolve_plan(&settings).await {
                Ok(plan) => plan,
                Err(err) => {
                    tracing::warn!("cannot resolve capture source: {err}");
                    hub.reset(Some(format!("cannot capture screen: {err}")));
                    if sleep_or_cancel(&cancel, Duration::from_secs(2)).await {
                        break;
                    }
                    continue;
                }
            };
            *self.input_rect.write().unwrap() = plan.input;
            hub.begin(plan.output_width, plan.output_height, settings.fps);

            let args = ffmpeg::stream_args(
                &plan,
                &encoder,
                settings.fps,
                settings.bitrate_kbps,
                hub.mode().output_mode(),
            );
            tracing::info!(
                encoder = %encoder,
                source = %plan.source.name,
                capture = ?plan.capture,
                output = format!("{}x{}", plan.output_width, plan.output_height),
                mode = hub.mode().as_str(),
                "starting capture pipeline"
            );

            let outcome = self.run_pipeline(&hub, &args, &cancel).await;

            if cancel.is_cancelled() {
                break;
            }

            let reason = outcome.unwrap_or_else(|e| e.to_string());
            consecutive_failures += 1;
            if consecutive_failures >= 3 && self.encoders.len() > 1 {
                consecutive_failures = 0;
                encoder_index = (encoder_index + 1) % self.encoders.len();
                tracing::warn!(
                    "encoder '{encoder}' keeps failing; switching to '{}'",
                    self.encoders[encoder_index]
                );
            } else if consecutive_failures >= 3 {
                consecutive_failures = 0;
            }
            hub.reset(Some(reason.clone()));
            tracing::warn!("capture pipeline stopped: {reason}");
            if sleep_or_cancel(&cancel, backoff).await {
                break;
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
        hub.reset(None);
    }

    async fn resolve_plan(&self, settings: &StreamSettings) -> Result<CapturePlan> {
        let desktop = capture::primary_desktop_rect()?;
        let space = capture::desktop_space(&self.ffmpeg, desktop).await;
        let source = capture::resolve_source(&settings.source)?;
        CapturePlan::build(source, space, settings.scale)
    }

    async fn supervise_audio(self: Arc<Self>, hub: Arc<Hub>, cancel: CancellationToken) {
        let Some(device) = self.audio_device.clone() else {
            hub.reset(Some("audio is disabled".into()));
            return;
        };
        let mut backoff = Duration::from_secs(1);
        loop {
            if cancel.is_cancelled() {
                break;
            }
            hub.begin(0, 0, 0);
            let args = ffmpeg::audio_args(&device, 128, 200);
            tracing::info!(device = %device, "starting audio pipeline");
            let outcome = self.run_pipeline(&hub, &args, &cancel).await;
            if cancel.is_cancelled() {
                break;
            }
            let reason = outcome.unwrap_or_else(|e| e.to_string());
            hub.reset(Some(format!("audio capture failed: {reason}")));
            tracing::warn!("audio pipeline stopped: {reason}");
            if sleep_or_cancel(&cancel, backoff).await {
                break;
            }
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
    }

    /// Spawn ffmpeg, feed stdout into the right parser, and forward frames.
    /// Returns the reason the pipeline ended (Err on process failure).
    async fn run_pipeline(
        &self,
        hub: &Arc<Hub>,
        args: &[String],
        cancel: &CancellationToken,
    ) -> Result<String> {
        let mut cmd = Command::new(&self.ffmpeg.path);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        hide_child_window(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow!(
                "could not start ffmpeg ({}): {e}",
                self.ffmpeg.path.display()
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg stdout unavailable"))?;
        let stderr = child.stderr.take();
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

        if let Some(stderr) = stderr {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    tracing::debug!(target: "ffmpeg", "{line}");
                    let mut tail = tail.lock().unwrap();
                    tail.push_back(line);
                    while tail.len() > 24 {
                        tail.pop_front();
                    }
                }
            });
        }

        let mut reader = stdout;
        let mut buf = vec![0u8; 64 * 1024];
        let mut splitter = AnnexBSplitter::new();
        let mut mp4 = Mp4Splitter::new();
        let mut ts_us: u64 = 0;
        let mut frame_interval = 1_000_000u64 / hub.meta().fps.max(1) as u64;
        let mut got_data = false;

        let result: Result<String> = async {
            loop {
                let read = tokio::select! {
                    _ = cancel.cancelled() => {
                        kill_child(&mut child).await;
                        return Ok("stopped".into());
                    }
                    res = tokio::time::timeout(READ_TIMEOUT, reader.read(&mut buf)) => {
                        match res {
                            Err(_) => {
                                kill_child(&mut child).await;
                                return Err(anyhow!("capture stalled (no frames for {}s)", READ_TIMEOUT.as_secs()));
                            }
                            Ok(Ok(0)) => break,
                            Ok(Ok(n)) => n,
                            Ok(Err(e)) => {
                                return Err(anyhow!("reading ffmpeg output failed: {e}"));
                            }
                        }
                    }
                };
                let chunk = &buf[..read];
                got_data = true;
                match hub.mode() {
                    Mode::WebCodecs => {
                        for au in splitter.push(chunk) {
                            if let Some(codec) = au.codec.clone() {
                                hub.set_codec(&codec);
                            }
                            ts_us = ts_us.saturating_add(frame_interval);
                            hub.publish_au(au, ts_us);
                        }
                        // Keep frame pacing in sync with the configured fps.
                        frame_interval = 1_000_000u64 / hub.meta().fps.max(1) as u64;
                    }
                    Mode::Mse | Mode::Audio => {
                        for event in mp4.push(chunk) {
                            match event {
                                Mp4Event::Init(init) => hub.set_init(init),
                                Mp4Event::Media { typ, data } => hub.publish_box(typ, data),
                            }
                        }
                    }
                }
            }
            if hub.mode() == Mode::WebCodecs {
                if let Some(au) = splitter.flush() {
                    ts_us = ts_us.saturating_add(frame_interval);
                    hub.publish_au(au, ts_us);
                }
            }
            kill_child(&mut child).await;
            let status = child.wait().await.ok();
            let code = status.map(|s| s.to_string()).unwrap_or_default();
            let detail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join(" | ");
            if got_data {
                Ok(format!("ffmpeg exited ({code}) {detail}"))
            } else {
                Err(anyhow!("ffmpeg failed to produce video: {}", if detail.is_empty() {
                    code
                } else {
                    detail
                }))
            }
        }
        .await;

        result
    }
}

async fn kill_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

fn hide_child_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Sleep, returning true if cancellation happened.
async fn sleep_or_cancel(cancel: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

fn spawn_idle_reaper(manager: Arc<StreamManager>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let stale: Vec<Mode> = {
                let slots = manager.slots.lock().unwrap();
                slots
                    .iter()
                    .filter(|(_, slot)| {
                        slot.hub.viewers() == 0
                            && slot.last_used.elapsed() > Duration::from_secs(30)
                    })
                    .map(|(mode, _)| *mode)
                    .collect()
            };
            if stale.is_empty() {
                continue;
            }
            let mut slots = manager.slots.lock().unwrap();
            for mode in stale {
                if let Some(slot) = slots.get(&mode) {
                    if slot.hub.viewers() == 0 {
                        tracing::debug!(mode = mode.as_str(), "stopping idle pipeline");
                        slot.cancel.cancel();
                        slots.remove(&mode);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_clamped_and_keyed() {
        let s = StreamSettings {
            fps: 500,
            bitrate_kbps: 10,
            scale: 3.0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.fps, 60);
        assert_eq!(s.bitrate_kbps, 500);
        assert_eq!(s.scale, 1.0);
        assert!(s.key("libx264").contains("primary"));
    }

    #[test]
    fn mse_fragment_cache() {
        let hub = Hub::new(Mode::Mse, 30);
        let init = Bytes::from_static(b"init");
        hub.set_init(init.clone());
        hub.set_codec("avc1.42c01e");
        hub.publish_box(*b"moof", Bytes::from_static(b"moofdata"));
        hub.publish_box(*b"mdat", Bytes::from_static(b"mdatdata"));
        let sub = hub.subscribe();
        assert_eq!(sub.initial.len(), 2);
        assert_eq!(&sub.initial[0].data[..], b"init");
        assert_eq!(&sub.initial[1].data[..], b"moofdatamdatdata");
        assert_eq!(hub.viewers(), 1);
        drop(sub);
        assert_eq!(hub.viewers(), 0);
    }

    #[test]
    fn next_fragment_replaces_previous() {
        let hub = Hub::new(Mode::Mse, 30);
        hub.set_init(Bytes::from_static(b"init"));
        hub.publish_box(*b"moof", Bytes::from_static(b"A"));
        hub.publish_box(*b"mdat", Bytes::from_static(b"a"));
        hub.publish_box(*b"moof", Bytes::from_static(b"B"));
        hub.publish_box(*b"mdat", Bytes::from_static(b"b"));
        let sub = hub.subscribe();
        assert_eq!(&sub.initial[1].data[..], b"Bb");
    }
}
