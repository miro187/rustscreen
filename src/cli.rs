use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ControlMode {
    /// Every authenticated viewer may control the mouse and keyboard.
    All,
    /// One viewer at a time may control; control is released on disconnect.
    Single,
    /// Nobody can control; viewers are read-only.
    None,
}

impl ControlMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlMode::All => "all",
            ControlMode::Single => "single",
            ControlMode::None => "none",
        }
    }
    pub fn allows_control(self) -> bool {
        !matches!(self, ControlMode::None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EncoderChoice {
    /// Probe the hardware encoders and fall back to libx264.
    Auto,
    /// NVIDIA NVENC (h264_nvenc)
    Nvenc,
    /// Intel Quick Sync (h264_qsv)
    Qsv,
    /// AMD AMF (h264_amf)
    Amf,
    /// Windows Media Foundation (h264_mf)
    Mf,
    /// Software x264 (libx264)
    X264,
}

impl EncoderChoice {
    /// Candidate ffmpeg encoder names, best first.
    pub fn candidates(self) -> Vec<&'static str> {
        match self {
            EncoderChoice::Auto => vec!["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"],
            EncoderChoice::Nvenc => vec!["h264_nvenc"],
            EncoderChoice::Qsv => vec!["h264_qsv"],
            EncoderChoice::Amf => vec!["h264_amf"],
            EncoderChoice::Mf => vec!["h264_mf"],
            EncoderChoice::X264 => vec!["libx264"],
        }
    }
}

/// Self-hosted screen sharing + remote control for your LAN.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "rustscreen",
    version,
    about = "Self-hosted screen sharing and remote control over your local network",
    long_about = None,
    after_help = "Examples:\n  rustscreen                       # random PIN, HTTPS, primary display\n  rustscreen --no-tls              # plain HTTP (port 8443)\n  rustscreen --password s3cret     # fixed password\n  rustscreen --source all --fps 60  # all displays, 60 fps\n  rustscreen --list-sources        # print capturable displays/windows\n\nDocs: https://github.com/miro187/rustscreen"
)]
pub struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8443, value_name = "PORT")]
    pub port: u16,

    /// Address to bind (use 127.0.0.1 to keep it local)
    #[arg(long, default_value = "0.0.0.0", value_name = "ADDR")]
    pub bind: IpAddr,

    /// Password / PIN required to connect. Use "none" to disable auth.
    /// Defaults to a random 6-digit PIN shown at startup.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,

    /// Who is allowed to control mouse & keyboard
    #[arg(long, value_enum, default_value_t = ControlMode::All)]
    pub control: ControlMode,

    /// Capture source: "primary", "all", "monitor:<id>" or "window:<id>"
    #[arg(long, default_value = "primary", value_name = "SOURCE")]
    pub source: String,

    /// Captured frames per second (5-60)
    #[arg(long, default_value_t = 30, value_name = "FPS")]
    pub fps: u32,

    /// Video bitrate in kilobits per second (500-50000)
    #[arg(long, default_value_t = 6000, value_name = "KBPS")]
    pub bitrate: u32,

    /// Output scale factor between 0.25 and 1.0
    #[arg(long, default_value_t = 1.0, value_name = "FACTOR")]
    pub scale: f32,

    /// H.264 encoder to use
    #[arg(long, value_enum, default_value_t = EncoderChoice::Auto)]
    pub encoder: EncoderChoice,

    /// Disable HTTPS (plain HTTP; traffic is NOT encrypted)
    #[arg(long)]
    pub no_tls: bool,

    /// Custom TLS certificate (PEM). Requires --key.
    #[arg(long, value_name = "FILE", requires = "key")]
    pub cert: Option<PathBuf>,

    /// Custom TLS private key (PEM). Requires --cert.
    #[arg(long, value_name = "FILE", requires = "cert")]
    pub key: Option<PathBuf>,

    /// Path to the ffmpeg executable (default: "ffmpeg" from PATH)
    #[arg(long, value_name = "FILE")]
    pub ffmpeg: Option<PathBuf>,

    /// Also stream audio from a DirectShow device. Pass "auto" to autodetect,
    /// or a device name (e.g. "Stereo Mix (Realtek Audio)").
    #[arg(long, num_args = 0..=1, default_missing_value = "auto", value_name = "DEVICE")]
    pub audio: Option<String>,

    /// Maximum number of simultaneous viewers (1-64)
    #[arg(long, default_value_t = 8, value_name = "N")]
    pub max_viewers: usize,

    /// Do not print a QR code in the terminal
    #[arg(long)]
    pub no_qr: bool,

    /// Open the browser on this machine after starting
    #[arg(long)]
    pub open: bool,

    /// Agent mode: connect outbound to a RustScreen relay instead of listening.
    /// Example: --relay https://abc.trycloudflare.com
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,

    /// Shared secret for registering with the relay (printed by rustscreen-relay)
    #[arg(long, value_name = "TOKEN", env = "RUSTSCREEN_RELAY_TOKEN")]
    pub relay_token: Option<String>,

    /// List capturable displays and windows, then exit
    #[arg(long)]
    pub list_sources: bool,

    /// Probe the available H.264 encoders, then exit
    #[arg(long)]
    pub list_encoders: bool,
}

impl Cli {
    pub fn tls_enabled(&self) -> bool {
        !self.no_tls || (self.cert.is_some() && self.key.is_some())
    }

    pub fn clamped_fps(&self) -> u32 {
        self.fps.clamp(5, 60)
    }

    pub fn clamped_bitrate(&self) -> u32 {
        self.bitrate.clamp(500, 50_000)
    }

    pub fn clamped_scale(&self) -> f32 {
        let s = if self.scale.is_finite() {
            self.scale
        } else {
            1.0
        };
        s.clamp(0.25, 1.0)
    }

    pub fn clamped_max_viewers(&self) -> usize {
        self.max_viewers.clamp(1, 64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn clamps_out_of_range_values() {
        let cli = Cli::try_parse_from([
            "rustscreen",
            "--fps",
            "1000",
            "--bitrate",
            "1",
            "--scale",
            "4",
            "--max-viewers",
            "999",
        ])
        .unwrap();
        assert_eq!(cli.clamped_fps(), 60);
        assert_eq!(cli.clamped_bitrate(), 500);
        assert_eq!(cli.clamped_scale(), 1.0);
        assert_eq!(cli.clamped_max_viewers(), 64);
    }

    #[test]
    fn tls_defaults_on() {
        let cli = Cli::try_parse_from(["rustscreen"]).unwrap();
        assert!(cli.tls_enabled());
        let cli = Cli::try_parse_from(["rustscreen", "--no-tls"]).unwrap();
        assert!(!cli.tls_enabled());
    }
}
