//! ffmpeg discovery, encoder probing and command construction.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

use crate::capture::{input_args, CapturePlan};
use crate::cli::EncoderChoice;

#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Fragmented MP4 on stdout (MSE compatible).
    Mp4,
    /// Raw Annex-B H.264 on stdout (WebCodecs compatible).
    AnnexB,
}

impl Ffmpeg {
    pub async fn discover(custom: Option<PathBuf>) -> Result<Self> {
        let path = custom.unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let mut cmd = Command::new(&path);
        cmd.arg("-version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        hide_console(&mut cmd);
        let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
            .await
            .map_err(|_| anyhow!("ffmpeg did not respond"))?
            .with_context(|| {
                format!(
                    "could not run '{}'. Install ffmpeg and make sure it is on PATH \
                     (winget install Gyan.FFmpeg / brew install ffmpeg / apt install ffmpeg), \
                     or pass --ffmpeg <path>.",
                    path.display()
                )
            })?;
        if !out.status.success() {
            bail!(
                "'{} -version' failed: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first_line = stdout.lines().next().unwrap_or("ffmpeg").trim();
        let version = first_line
            .strip_prefix("ffmpeg version ")
            .unwrap_or(first_line)
            .split_whitespace()
            .next()
            .unwrap_or("unknown")
            .to_string();
        Ok(Self { path, version })
    }

    pub fn display(&self) -> String {
        format!("{} ({})", self.path.display(), self.version)
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.path);
        cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
        hide_console(&mut cmd);
        cmd
    }

    /// Run ffmpeg and return raw stdout. Used for thumbnails and probes.
    pub async fn run_output(&self, args: &[String]) -> Result<Vec<u8>> {
        let child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not start {}", self.path.display()))?;
        let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .map_err(|_| anyhow!("ffmpeg timed out"))?
            .context("ffmpeg failed to run")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("ffmpeg failed: {}", tail_lines(&stderr, 6));
        }
        if out.stdout.len() > 32 * 1024 * 1024 {
            bail!("ffmpeg produced an unexpectedly large output");
        }
        Ok(out.stdout)
    }

    /// Run ffmpeg and return stderr even if the process exits non-zero.
    pub async fn capture_stderr(&self, args: &[String]) -> Result<String> {
        let child = self
            .command(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not start {}", self.path.display()))?;
        let out =
            match tokio::time::timeout(Duration::from_secs(25), child.wait_with_output()).await {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => return Err(e).context("ffmpeg failed to run"),
                Err(_) => bail!("ffmpeg timed out"),
            };
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Ok(stderr)
    }

    pub async fn available_encoders(&self) -> Vec<String> {
        let out = self
            .run_output(&[
                "-hide_banner".into(),
                "-loglevel".into(),
                "info".into(),
                "-encoders".into(),
            ])
            .await;
        let Ok(bytes) = out else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&bytes);
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let _flags = parts.next()?;
                let name = parts.next()?;
                if name.starts_with("h264") {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Verify that an encoder actually works with our exact settings.
    pub async fn probe_encoder(&self, encoder: &str) -> Result<()> {
        let args = probe_args(encoder);
        let child = self
            .command(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not start {}", self.path.display()))?;
        let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .map_err(|_| anyhow!("encoder probe timed out"))?
            .context("encoder probe failed")?;
        if out.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(anyhow!("{}", tail_lines(&stderr, 4)))
        }
    }

    /// Returns the working encoders in preference order.
    pub async fn select_encoders(&self, choice: EncoderChoice) -> Result<Vec<String>> {
        let mut working = Vec::new();
        let mut failures = Vec::new();
        for candidate in choice.candidates() {
            match self.probe_encoder(candidate).await {
                Ok(()) => working.push(candidate.to_string()),
                Err(e) => failures.push(format!("{candidate}: {e}")),
            }
        }
        if working.is_empty() {
            bail!(
                "no usable H.264 encoder found. Tried: {}. \
                 Install ffmpeg with libx264 support or choose another --encoder.",
                failures.join("; ")
            );
        }
        tracing::debug!(?working, ?failures, "encoder probe finished");
        Ok(working)
    }
}

fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// ffmpeg arguments that encode 6 synthetic frames with the given encoder.
fn probe_args(encoder: &str) -> Vec<String> {
    let mut args = base_args();
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc2=size=640x360:rate=30".into(),
        "-frames:v".into(),
        "8".into(),
    ]);
    args.extend(encoder_args(encoder, 2000, 30));
    args.extend(["-f".into(), "null".into(), "-".into()]);
    args
}

fn base_args() -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-nostats".into(),
    ]
}

pub fn encoder_args(encoder: &str, bitrate_kbps: u32, fps: u32) -> Vec<String> {
    let gop = fps.clamp(5, 60);
    let br = format!("{bitrate_kbps}k");
    let mut args: Vec<String> = match encoder {
        "libx264" => vec![
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "ultrafast".into(),
            "-tune".into(),
            "zerolatency".into(),
            "-profile:v".into(),
            "baseline".into(),
            "-bf".into(),
            "0".into(),
            "-g".into(),
            gop.to_string(),
            "-keyint_min".into(),
            gop.to_string(),
            "-sc_threshold".into(),
            "0".into(),
            "-b:v".into(),
            br.clone(),
            "-maxrate".into(),
            br.clone(),
            "-bufsize".into(),
            br.clone(),
            "-x264-params".into(),
            "repeat-headers=0:aud=0:force-cfr=1".into(),
        ],
        "h264_nvenc" => vec![
            "-c:v".into(),
            "h264_nvenc".into(),
            "-preset".into(),
            "p1".into(),
            "-tune".into(),
            "ull".into(),
            "-rc".into(),
            "cbr".into(),
            "-b:v".into(),
            br.clone(),
            "-maxrate".into(),
            br.clone(),
            "-bufsize".into(),
            br.clone(),
            "-g".into(),
            gop.to_string(),
            "-bf".into(),
            "0".into(),
        ],
        "h264_qsv" => vec![
            "-c:v".into(),
            "h264_qsv".into(),
            "-preset".into(),
            "veryfast".into(),
            "-b:v".into(),
            br.clone(),
            "-maxrate".into(),
            br.clone(),
            "-g".into(),
            gop.to_string(),
            "-bf".into(),
            "0".into(),
            "-look_ahead".into(),
            "0".into(),
        ],
        "h264_amf" => vec![
            "-c:v".into(),
            "h264_amf".into(),
            "-usage".into(),
            "lowlatency".into(),
            "-quality".into(),
            "speed".into(),
            "-rc".into(),
            "cbr".into(),
            "-b:v".into(),
            br.clone(),
            "-maxrate".into(),
            br.clone(),
            "-g".into(),
            gop.to_string(),
            "-bf".into(),
            "0".into(),
        ],
        "h264_mf" => vec![
            "-c:v".into(),
            "h264_mf".into(),
            "-scenario".into(),
            "display_remoting".into(),
            "-rate_control".into(),
            "cbr".into(),
            "-b:v".into(),
            br.clone(),
            "-maxrate".into(),
            br.clone(),
            "-g".into(),
            gop.to_string(),
        ],
        other => vec!["-c:v".into(), other.into()],
    };
    args.extend(["-pix_fmt".into(), "yuv420p".into()]);
    args
}

/// Full ffmpeg command line for a live stream pipeline.
pub fn stream_args(
    plan: &CapturePlan,
    encoder: &str,
    fps: u32,
    bitrate_kbps: u32,
    mode: OutputMode,
) -> Vec<String> {
    let mut args = base_args();
    args.extend(input_args(plan, fps));
    let filter =
        if plan.output_width != plan.capture.width || plan.output_height != plan.capture.height {
            format!(
                "scale={}:{}:flags=fast_bilinear,format=yuv420p",
                plan.output_width, plan.output_height
            )
        } else {
            "format=yuv420p".to_string()
        };
    args.extend(["-vf".into(), filter]);
    args.extend(encoder_args(encoder, bitrate_kbps, fps));
    args.extend(["-an".into(), "-sn".into(), "-dn".into()]);
    match mode {
        OutputMode::Mp4 => {
            args.extend([
                "-f".into(),
                "mp4".into(),
                "-movflags".into(),
                "+frag_keyframe+empty_moov+default_base_moof+omit_tfhd_offset".into(),
                "-flush_packets".into(),
                "1".into(),
                "pipe:1".into(),
            ]);
        }
        OutputMode::AnnexB => {
            args.extend([
                "-f".into(),
                "h264".into(),
                "-bsf:v".into(),
                "h264_metadata=aud=insert".into(),
                "-flush_packets".into(),
                "1".into(),
                "pipe:1".into(),
            ]);
        }
    }
    args
}

/// Audio-only fMP4 pipeline (optional feature).
pub fn audio_args(device: &str, bitrate_kbps: u32, frag_ms: u32) -> Vec<String> {
    let mut args = base_args();
    #[cfg(windows)]
    {
        args.extend([
            "-f".into(),
            "dshow".into(),
            "-audio_buffer_size".into(),
            "50".into(),
            "-i".into(),
            format!("audio={device}"),
        ]);
    }
    #[cfg(target_os = "linux")]
    {
        args.extend(["-f".into(), "pulse".into(), "-i".into(), device.to_string()]);
    }
    #[cfg(target_os = "macos")]
    {
        args.extend([
            "-f".into(),
            "avfoundation".into(),
            "-i".into(),
            format!(":{device}"),
        ]);
    }
    args.extend([
        "-vn".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        format!("{bitrate_kbps}k"),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "2".into(),
        "-f".into(),
        "mp4".into(),
        "-movflags".into(),
        "+frag_keyframe+empty_moov+default_base_moof".into(),
        "-frag_duration".into(),
        (frag_ms.max(50) * 1000).to_string(),
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]);
    args
}

pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_contain_low_latency_flags() {
        let args = encoder_args("libx264", 4000, 30);
        let joined = args.join(" ");
        assert!(joined.contains("zerolatency"));
        assert!(joined.contains("-b:v 4000k"));
        assert!(joined.contains("-g 30"));
        assert!(joined.contains("-pix_fmt yuv420p"));
    }

    #[test]
    fn annexb_has_aud_filter() {
        let plan = crate::capture::CapturePlan {
            source: crate::capture::SourceInfo {
                id: "monitor:0".into(),
                name: "Display".into(),
                kind: "monitor".into(),
                app: None,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                primary: true,
                builtin: false,
            },
            input: crate::capture::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            capture: crate::capture::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            output_width: 1920,
            output_height: 1080,
        };
        let args = stream_args(&plan, "libx264", 30, 6000, OutputMode::AnnexB);
        assert!(args.join(" ").contains("h264_metadata=aud=insert"));
        let args = stream_args(&plan, "libx264", 30, 6000, OutputMode::Mp4);
        let joined = args.join(" ");
        assert!(joined.contains("empty_moov"));
        assert!(joined.contains("frag_keyframe"));
    }

    #[test]
    fn tail_lines_trims() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b | c");
    }
}
