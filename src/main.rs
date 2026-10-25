//! RustScreen — self-hosted screen sharing and remote control for your LAN.

mod auth;
mod capture;
mod cli;
mod ffmpeg;
mod h264;
mod input;
mod mp4;
mod net;
mod qr;
mod server;
mod stream;
mod tls;
mod util;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::auth::Auth;
use crate::cli::Cli;
use crate::ffmpeg::Ffmpeg;
use crate::input::InputHandle;
use crate::server::{AppState, ControlState};
use crate::stream::{StreamManager, StreamSettings};
use crate::util::Paint;

fn main() {
    // Console + DPI setup must happen before anything is printed or captured.
    util::prepare_console();
    util::protect_child_processes();
    capture::enable_dpi_awareness();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("fatal: could not start async runtime: {err}");
            std::process::exit(1);
        }
    };

    if let Err(err) = runtime.block_on(run()) {
        eprintln!("\n{} {err:#}", Paint::red("error:"));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Arc::new(Cli::parse());
    init_tracing();

    if cli.list_sources {
        print_sources()?;
        return Ok(());
    }

    let ffmpeg = Arc::new(
        Ffmpeg::discover(cli.ffmpeg.clone())
            .await
            .context("ffmpeg is required to capture and encode the screen")?,
    );

    if cli.list_encoders {
        print_encoders(&ffmpeg, cli.encoder).await?;
        return Ok(());
    }

    println!("{}", Paint::dim(format!("  ffmpeg: {}", ffmpeg.display())));

    let (password, generated_pin) = resolve_password(&cli);
    let encoders = ffmpeg.select_encoders(cli.encoder).await?;
    let audio_device = resolve_audio(&cli, &ffmpeg).await;

    let source = capture::resolve_source(&cli.source).or_else(|err| {
        eprintln!(
            "{} source '{}': {err}. Falling back to the primary display.",
            Paint::yellow("warning:"),
            cli.source
        );
        capture::resolve_source("primary")
    })?;

    let desktop = capture::primary_desktop_rect()?;
    let space = capture::desktop_space(&ffmpeg, desktop).await;
    let input_rect = source.rect();
    let settings = StreamSettings {
        source: source.id.clone(),
        fps: cli.clamped_fps(),
        bitrate_kbps: cli.clamped_bitrate(),
        scale: cli.clamped_scale(),
        encoder: None,
    }
    .sanitized();

    let hostname = util::hostname();
    let tls_enabled = cli.tls_enabled();
    let lan_ips = net::all_lan_ips();
    let tls_material = if tls_enabled {
        let custom = match (&cli.cert, &cli.key) {
            (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
            _ => None,
        };
        Some(
            tls::load_or_generate(custom, &lan_ips, &hostname)
                .context("preparing TLS certificate")?,
        )
    } else {
        None
    };

    let auth = Arc::new(Auth::new(password));
    let control = ControlState::new(cli.control);
    let input = InputHandle::spawn();
    let streams = StreamManager::new(
        ffmpeg.clone(),
        encoders.clone(),
        settings.clone(),
        input_rect,
        audio_device.clone(),
    );

    let state = Arc::new(AppState {
        cli: cli.clone(),
        auth,
        ffmpeg: ffmpeg.clone(),
        streams: streams.clone(),
        input,
        control,
        started: Instant::now(),
        hostname: hostname.clone(),
        tls: tls_enabled,
        cert_pem: tls_material.as_ref().map(|m| m.cert_pem.clone()),
        thumbnails: Mutex::new(HashMap::new()),
        next_client_id: AtomicU64::new(0),
    });

    print_banner(BannerInfo {
        hostname: &hostname,
        port: cli.port,
        bind: cli.bind,
        tls: tls_enabled,
        pin: generated_pin.as_deref(),
        auth_enabled: generated_pin.is_some() || cli.password.is_some(),
        control: cli.control,
        source: &source.name,
        source_rect: input_rect,
        scale: settings.scale,
        fps: settings.fps,
        bitrate: settings.bitrate_kbps,
        encoder: encoders.first().map(String::as_str).unwrap_or("?"),
        audio: audio_device.as_deref(),
        scaling: space,
        cert_path: tls_material
            .as_ref()
            .and_then(|m| m.path.as_ref())
            .map(|p| p.display().to_string()),
        cert_generated: tls_material.as_ref().is_some_and(|m| m.generated),
        custom_cert: cli.cert.is_some(),
        no_qr: cli.no_qr,
    });

    let addr = SocketAddr::new(cli.bind, cli.port);
    let app = server::router(state.clone());

    if cli.open {
        open_browser_later(preferred_local_url(cli.port, tls_enabled));
    }

    if tls_enabled {
        let material = tls_material.expect("tls material");
        serve_tls(addr, app, state.clone(), material).await?;
    } else {
        serve_plain(addr, app, state.clone()).await?;
    }

    state.streams.shutdown();
    println!("\n{}", Paint::dim("  RustScreen stopped."));
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ffmpeg=warn,xcap=off,windows=off"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .try_init();
}

fn resolve_password(cli: &Cli) -> (Option<String>, Option<String>) {
    match cli.password.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("none") => (None, None),
        Some(value) if !value.trim().is_empty() => (Some(value.trim().to_string()), None),
        _ => {
            let pin = util::random_pin();
            (Some(pin.clone()), Some(pin))
        }
    }
}

async fn resolve_audio(cli: &Cli, ffmpeg: &Ffmpeg) -> Option<String> {
    let requested = cli.audio.as_deref()?;
    if !requested.eq_ignore_ascii_case("auto") {
        return Some(requested.to_string());
    }
    #[cfg(windows)]
    {
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "info",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Ok(stderr) = ffmpeg.capture_stderr(&args).await {
            let devices: Vec<String> = stderr
                .lines()
                .filter(|line| line.contains("(audio)"))
                .filter_map(|line| {
                    let start = line.find('"')? + 1;
                    let rest = &line[start..];
                    let end = rest.find('"')?;
                    Some(rest[..end].to_string())
                })
                .collect();
            if devices.is_empty() {
                eprintln!(
                    "{} no DirectShow audio devices found; continuing without audio",
                    Paint::yellow("warning:")
                );
                return None;
            }
            let loopback = devices.iter().find(|d| {
                let lower = d.to_ascii_lowercase();
                lower.contains("stereo mix")
                    || lower.contains("loopback")
                    || lower.contains("what u hear")
                    || lower.contains("virtual-audio")
                    || lower.contains("vb-audio")
                    || lower.contains("cable output")
            });
            let chosen = loopback.cloned().unwrap_or_else(|| devices[0].clone());
            if loopback.is_none() {
                eprintln!(
                    "{} no system-audio loopback device found; capturing '{}'. \
                     Install a virtual loopback device for system audio.",
                    Paint::yellow("warning:"),
                    chosen
                );
            }
            Some(chosen)
        } else {
            None
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = ffmpeg;
        Some("default".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = ffmpeg;
        Some("0".to_string())
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = ffmpeg;
        None
    }
}

fn print_sources() -> Result<()> {
    let (monitors, windows) = capture::list_sources()?;
    println!("\n{}", Paint::bold("Capturable displays"));
    for m in &monitors {
        println!(
            "  {:<18} {:<28} {}x{} at {},{}{}  {}",
            m.id,
            m.name,
            m.width,
            m.height,
            m.x,
            m.y,
            if m.primary { "  primary" } else { "" },
            Paint::dim(if m.builtin { "(built-in)" } else { "" }),
        );
    }
    if !windows.is_empty() {
        println!("\n{}", Paint::bold("Capturable windows"));
        for w in &windows {
            println!(
                "  {:<18} {:<40} {}x{} at {},{}  {}",
                w.id,
                format!(
                    "{} {}",
                    w.app
                        .as_deref()
                        .map(|a| format!("[{a}]"))
                        .unwrap_or_default(),
                    w.name
                ),
                w.width,
                w.height,
                w.x,
                w.y,
                Paint::dim("(captures the visible screen area)"),
            );
        }
    }
    println!(
        "\nUse it with: {} --source {}\n",
        Paint::cyan("rustscreen"),
        monitors.first().map(|m| m.id.as_str()).unwrap_or("primary")
    );
    Ok(())
}

async fn print_encoders(ffmpeg: &Ffmpeg, choice: cli::EncoderChoice) -> Result<()> {
    let available = ffmpeg.available_encoders().await;
    println!("\n{}", Paint::bold("H.264 encoders reported by ffmpeg"));
    if available.is_empty() {
        println!("  (none)");
    }
    for enc in &available {
        println!("  - {enc}");
    }
    println!("\n{}", Paint::bold("Probing with real 8-frame encodes"));
    for candidate in choice.candidates() {
        match ffmpeg.probe_encoder(candidate).await {
            Ok(()) => println!("  {} {candidate}", Paint::green("OK         ")),
            Err(err) => println!(
                "  {} {candidate}  {}",
                Paint::red("unavailable"),
                Paint::dim(err.to_string())
            ),
        }
    }
    println!();
    Ok(())
}

struct BannerInfo<'a> {
    hostname: &'a str,
    port: u16,
    bind: std::net::IpAddr,
    tls: bool,
    pin: Option<&'a str>,
    auth_enabled: bool,
    control: cli::ControlMode,
    source: &'a str,
    source_rect: capture::Rect,
    scale: f32,
    fps: u32,
    bitrate: u32,
    encoder: &'a str,
    audio: Option<&'a str>,
    scaling: capture::DesktopSpace,
    cert_path: Option<String>,
    cert_generated: bool,
    custom_cert: bool,
    no_qr: bool,
}

fn print_banner(info: BannerInfo<'_>) {
    let urls = net::connect_addresses(info.tls, info.port, info.bind);
    let preferred = urls
        .iter()
        .find(|(label, _)| label != "Local")
        .or_else(|| urls.first())
        .map(|(_, url)| url.clone())
        .unwrap_or_else(|| preferred_local_url(info.port, info.tls));

    let line = "─".repeat(58);
    println!();
    println!("  {}", Paint::bold(Paint::cyan("RustScreen")));
    println!("  {}", Paint::dim(&line));
    println!(
        "  {:<14} {} ({})",
        Paint::dim("Host"),
        info.hostname,
        util::os_name()
    );
    for (label, url) in &urls {
        println!("  {:<14} {}", Paint::dim(label), Paint::bold(url));
    }
    if let Some(pin) = info.pin {
        let spaced: Vec<String> = pin.chars().map(|c| c.to_string()).collect();
        println!(
            "  {:<14} {}",
            Paint::dim("PIN"),
            Paint::bold(Paint::green(spaced.join(" ")))
        );
    } else if !info.auth_enabled {
        println!(
            "  {:<14} {}",
            Paint::dim("PIN"),
            Paint::red("authentication DISABLED")
        );
    }
    println!(
        "  {:<14} {}",
        Paint::dim("Control"),
        match info.control {
            cli::ControlMode::All => "enabled for all viewers".to_string(),
            cli::ControlMode::Single => "one viewer at a time".to_string(),
            cli::ControlMode::None => "view only".to_string(),
        }
    );
    println!(
        "  {:<14} {} ({}x{})",
        Paint::dim("Source"),
        info.source,
        info.source_rect.width,
        info.source_rect.height
    );
    let out_w = if (info.scale - 1.0).abs() < f32::EPSILON {
        info.source_rect.width
    } else {
        (info.source_rect.width as f32 * info.scale).round() as u32
    };
    let out_h = if (info.scale - 1.0).abs() < f32::EPSILON {
        info.source_rect.height
    } else {
        (info.source_rect.height as f32 * info.scale).round() as u32
    };
    println!(
        "  {:<14} {}x{} @ {} fps, {} kb/s, {}",
        Paint::dim("Stream"),
        util::even(out_w),
        util::even(out_h),
        info.fps,
        info.bitrate,
        info.encoder
    );
    if let Some(device) = info.audio {
        println!("  {:<14} {}", Paint::dim("Audio"), device);
    }
    if (info.scaling.scale_x() - 1.0).abs() > 0.01 {
        println!(
            "  {:<14} {:.0}% (coordinates are remapped automatically)",
            Paint::dim("Display scale"),
            info.scaling.scale_x() * 100.0
        );
    }
    if info.tls {
        println!(
            "  {:<14} {}",
            Paint::dim("TLS"),
            if info.custom_cert {
                "custom certificate"
            } else {
                "self-signed certificate (accept the browser warning once)"
            }
        );
        if let Some(path) = &info.cert_path {
            println!(
                "  {:<14} {}{}",
                Paint::dim("Certificate"),
                Paint::dim(path),
                if info.cert_generated {
                    Paint::dim(" (new)")
                } else {
                    String::new()
                }
            );
        }
    } else {
        println!(
            "  {:<14} {}",
            Paint::dim("TLS"),
            Paint::red("disabled — traffic is NOT encrypted")
        );
    }
    println!("  {}", Paint::dim(&line));

    if !info.no_qr && std::io::stdout().is_terminal() {
        if let Ok(code) = qr::terminal(&preferred) {
            println!();
            for line in code.lines() {
                println!("   {line}");
            }
            println!("   {}", Paint::dim(&preferred));
        }
    }
    println!();
    println!(
        "  {}",
        Paint::dim("Open the address on any device on your network, then enter the PIN.")
    );
    println!("  {}", Paint::dim("Press Ctrl+C to stop."));
    println!();
}

fn preferred_local_url(port: u16, tls: bool) -> String {
    net::base_url(
        tls,
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port,
    )
}

fn open_browser_later(url: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = open_url(&url);
    });
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

async fn serve_plain(addr: SocketAddr, app: axum::Router, state: Arc<AppState>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!("could not listen on {addr} — is another program using this port?")
    })?;
    println!(
        "  {} {}\n",
        Paint::green("ready on"),
        listener.local_addr()?
    );
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    let mut server_task = tokio::spawn(async move { server.await });

    tokio::select! {
        result = &mut server_task => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err).context("server error"),
                Err(err) => return Err(err).context("server task failed"),
            }
        }
        _ = shutdown_signal() => {
            println!("
    {}", Paint::dim("  shutting down…"));
            // End media streams first so graceful shutdown cannot hang on them.
            state.streams.shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(3), server_task).await;
        }
    }
    Ok(())
}

async fn serve_tls(
    addr: SocketAddr,
    app: axum::Router,
    state: Arc<AppState>,
    material: tls::TlsMaterial,
) -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let config =
        axum_server::tls_rustls::RustlsConfig::from_pem(material.cert_pem, material.key_pem)
            .await
            .context("invalid TLS certificate or key")?;

    let listener = std::net::TcpListener::bind(addr).with_context(|| {
        format!("could not listen on {addr} — is another program using this port?")
    })?;
    listener.set_nonblocking(true)?;
    println!(
        "  {} {}\n",
        Paint::green("ready on"),
        listener.local_addr()?
    );

    let handle = axum_server::Handle::new();
    let server = axum_server::from_tcp_rustls(listener, config)
        .context("setting up TLS listener")?
        .handle(handle.clone())
        .serve(app.into_make_service_with_connect_info::<SocketAddr>());

    let mut server_task = tokio::spawn(server);
    tokio::select! {
        result = &mut server_task => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err).context("server error"),
                Err(err) => return Err(err).context("server task failed"),
            }
        }
        _ = shutdown_signal() => {
            println!("\n{}", Paint::dim("  shutting down…"));
            handle.graceful_shutdown(Some(Duration::from_secs(2)));
            state.streams.shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(4), server_task).await;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
