//! Screen/window discovery, capture geometry and thumbnails.
//!
//! ffmpeg does the actual capturing (`gdigrab` on Windows, `x11grab` on Linux,
//! `avfoundation` on macOS). xcap is used only for enumerating displays and
//! windows, and for the "friendly" monitor names.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::ffmpeg::Ffmpeg;
use crate::util::even;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
    pub fn union(a: Rect, b: Rect) -> Rect {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        let right = a.right().max(b.right());
        let bottom = a.bottom().max(b.bottom());
        Rect {
            x,
            y,
            width: (right - x).max(0) as u32,
            height: (bottom - y).max(0) as u32,
        }
    }
    pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
        let x = a.x.max(b.x);
        let y = a.y.max(b.y);
        let right = a.right().min(b.right());
        let bottom = a.bottom().min(b.bottom());
        if right <= x || bottom <= y {
            return None;
        }
        Some(Rect {
            x,
            y,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        })
    }
    pub fn evenized(&self) -> Rect {
        Rect {
            width: even(self.width),
            height: even(self.height),
            ..*self
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub app: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub builtin: bool,
}

impl SourceInfo {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// Enable per-monitor DPI awareness (Windows). Must run before any screen API.
pub fn enable_dpi_awareness() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn monitor_sources() -> Result<Vec<SourceInfo>> {
    let monitors = xcap::Monitor::all().context("could not enumerate displays")?;
    let mut out = Vec::new();
    for m in monitors {
        let id = m.id().unwrap_or(0);
        let width = m.width().unwrap_or(0);
        let height = m.height().unwrap_or(0);
        if width < 16 || height < 16 {
            continue;
        }
        let name = m
            .friendly_name()
            .or_else(|_| m.name())
            .unwrap_or_else(|_| format!("Display {id}"));
        out.push(SourceInfo {
            id: format!("monitor:{id}"),
            name: if name.trim().is_empty() {
                format!("Display {id}")
            } else {
                name
            },
            kind: "monitor".into(),
            app: None,
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width,
            height,
            primary: m.is_primary().unwrap_or(false),
            builtin: m.is_builtin().unwrap_or(false),
        });
    }
    if out.is_empty() {
        return Err(anyhow!("no displays found"));
    }
    out.sort_by_key(|m| (!m.primary, m.x, m.y));
    Ok(out)
}

pub fn window_sources(monitors: &[SourceInfo]) -> Vec<SourceInfo> {
    let Ok(windows) = xcap::Window::all() else {
        return Vec::new();
    };
    let desktop = monitors
        .iter()
        .map(|m| m.rect())
        .reduce(Rect::union)
        .unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });

    let mut out = Vec::new();
    for w in windows {
        if w.is_minimized().unwrap_or(true) {
            continue;
        }
        let title = w.title().unwrap_or_default();
        let app = w.app_name().unwrap_or_default();
        let title = title.trim();
        if title.is_empty() || title == "Default IME" || title == "MSCTFIME UI" {
            continue;
        }
        let (x, y, width, height) = (
            w.x().unwrap_or(0),
            w.y().unwrap_or(0),
            w.width().unwrap_or(0),
            w.height().unwrap_or(0),
        );
        if width < 160 || height < 120 {
            continue;
        }
        // Only offer windows that are at least partially on screen.
        let Some(visible) = Rect::intersect(
            Rect {
                x,
                y,
                width,
                height,
            },
            desktop,
        ) else {
            continue;
        };
        if visible.area() < 160 * 120 {
            continue;
        }
        let app_label = if app.trim().is_empty() {
            None
        } else {
            Some(app)
        };
        let id = format!("window:{}", w.id().unwrap_or(0));
        let name = if title.len() > 70 {
            format!("{}…", truncate_chars(title, 70))
        } else {
            title.to_string()
        };
        out.push(SourceInfo {
            id,
            name,
            kind: "window".into(),
            app: app_label,
            x: visible.x,
            y: visible.y,
            width: visible.width,
            height: visible.height,
            primary: false,
            builtin: false,
        });
        if out.len() >= 40 {
            break;
        }
    }
    out
}

/// A synthetic, privacy-safe source for demos and automated tests.
pub fn demo_source() -> SourceInfo {
    SourceInfo {
        id: "demo".into(),
        name: "Demo pattern".into(),
        kind: "demo".into(),
        app: None,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: false,
        builtin: false,
    }
}

/// All capturable sources: displays first, then visible windows.
pub fn list_sources() -> Result<(Vec<SourceInfo>, Vec<SourceInfo>)> {
    let monitors = monitor_sources()?;
    let windows = window_sources(&monitors);
    Ok((monitors, windows))
}

/// Like [`list_sources`], but with the demo pattern offered as a display.
pub fn list_sources_with_demo(include_demo: bool) -> Result<(Vec<SourceInfo>, Vec<SourceInfo>)> {
    let (mut monitors, windows) = list_sources()?;
    if include_demo {
        monitors.push(demo_source());
    }
    Ok((monitors, windows))
}

/// Resolve a user-provided source id to a concrete rectangle.
pub fn resolve_source(id: &str) -> Result<SourceInfo> {
    let wanted = id.trim();
    let lower = wanted.to_ascii_lowercase();

    if lower == "demo" || lower == "test" {
        return Ok(demo_source());
    }

    let monitors = monitor_sources()?;

    if lower.is_empty() || lower == "primary" {
        return monitors
            .iter()
            .find(|m| m.primary)
            .or_else(|| monitors.first())
            .cloned()
            .ok_or_else(|| anyhow!("no primary display found"));
    }

    if lower == "all" || lower == "desktop" {
        let union = monitors
            .iter()
            .map(|m| m.rect())
            .reduce(Rect::union)
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
        return Ok(SourceInfo {
            id: "all".into(),
            name: "All displays".into(),
            kind: "desktop".into(),
            app: None,
            x: union.x,
            y: union.y,
            width: union.width,
            height: union.height,
            primary: true,
            builtin: false,
        });
    }

    if let Some(found) = monitors.iter().find(|m| m.id == wanted) {
        return Ok(found.clone());
    }

    let windows = window_sources(&monitors);
    if let Some(found) = windows.iter().find(|w| w.id == wanted) {
        return Ok(found.clone());
    }

    // Allow "monitor:0" style fallback to the nth display.
    if let Some(n) = lower
        .strip_prefix("monitor:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        if let Some(found) = monitors.get(n) {
            return Ok(found.clone());
        }
    }

    Err(anyhow!(
        "source '{wanted}' not found (run with --list-sources to see available ids)"
    ))
}

/// A privacy-safe, on-brand demo pattern rendered entirely by ffmpeg.
fn demo_filter(width: u32, height: u32, fps: u32) -> String {
    let grid = (height / 11).max(24);
    let title_size = (height as f32 * 0.176).round() as u32;
    let subtitle_size = (height as f32 * 0.041).round() as u32;
    let offset = (height as f32 * 0.083).round() as i32;

    let mut chain = format!(
        "gradients=s={width}x{height}:c0=0x030712:c1=0x0e2a44:c2=0x312e81:speed=0.008:rate={fps},drawgrid=w={grid}:h={grid}:t=1:c=0x38bdf8@0.06"
    );
    if let (Some(bold), Some(regular)) = (find_font(true), find_font(false)) {
        chain.push_str(&format!(
            ",drawtext=fontfile='{bold}':text='RustScreen':fontcolor=0xe2e8f0:fontsize={title_size}:x=(w-text_w)/2:y=(h-text_h)/2-{offset}:shadowcolor=0x000000@0.4:shadowx=0:shadowy=4"
        ));
        chain.push_str(&format!(
            ",drawtext=fontfile='{regular}':text='DEMO SOURCE  ·  no screen is being captured':fontcolor=0x7dd3fc:fontsize={subtitle_size}:x=(w-text_w)/2:y=(h-text_h)/2+{offset}"
        ));
    }
    chain
}

fn find_font(bold: bool) -> Option<String> {
    const BOLD: &[&str] = &[
        "C:/Windows/Fonts/segoeuib.ttf",
        "C:/Windows/Fonts/arialbd.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    ];
    const REGULAR: &[&str] = &[
        "C:/Windows/Fonts/segoeui.ttf",
        "C:/Windows/Fonts/arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    let candidates = if bold { BOLD } else { REGULAR };
    candidates
        .iter()
        .find(|path| std::path::Path::new(path).is_file())
        .map(|path| path.replace('\\', "/").replace(':', "\\:"))
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

pub fn primary_desktop_rect() -> Result<Rect> {
    let monitors = monitor_sources()?;
    Ok(monitors
        .iter()
        .map(|m| m.rect())
        .reduce(Rect::union)
        .unwrap_or_else(|| monitors[0].rect()))
}

/// The coordinate space ffmpeg's capture backend sees. On a 125% scaled
/// Windows desktop, a DPI-unaware ffmpeg sees a smaller virtual desktop than
/// our DPI-aware process, so we translate rectangles into its coordinate space.
#[derive(Debug, Clone, Copy)]
pub struct DesktopSpace {
    /// Virtual desktop rectangle in ffmpeg's coordinate space.
    pub ffmpeg: Rect,
    /// Virtual desktop rectangle in physical pixels (our process's space).
    pub physical: Rect,
}

impl DesktopSpace {
    fn identity(physical: Rect) -> Self {
        Self {
            ffmpeg: physical,
            physical,
        }
    }

    pub fn scale_x(&self) -> f64 {
        if self.physical.width == 0 {
            1.0
        } else {
            self.ffmpeg.width as f64 / self.physical.width as f64
        }
    }

    pub fn scale_y(&self) -> f64 {
        if self.physical.height == 0 {
            1.0
        } else {
            self.ffmpeg.height as f64 / self.physical.height as f64
        }
    }

    /// Translate a physical rectangle into ffmpeg's coordinate space.
    pub fn to_ffmpeg(self, rect: Rect) -> Rect {
        let sx = self.scale_x();
        let sy = self.scale_y();
        let x = self.ffmpeg.x + ((rect.x - self.physical.x) as f64 * sx).round() as i32;
        let y = self.ffmpeg.y + ((rect.y - self.physical.y) as f64 * sy).round() as i32;
        Rect {
            x,
            y,
            width: (rect.width as f64 * sx).round().max(2.0) as u32,
            height: (rect.height as f64 * sy).round().max(2.0) as u32,
        }
    }
}

static DESKTOP_SPACE: OnceLock<DesktopSpace> = OnceLock::new();

pub async fn desktop_space(ffmpeg: &Ffmpeg, physical: Rect) -> DesktopSpace {
    if let Some(space) = DESKTOP_SPACE.get() {
        return *space;
    }
    let probed = detect_ffmpeg_desktop(ffmpeg).await;
    let space = match probed {
        Some(rect) if rect.width >= 16 && rect.height >= 16 => DesktopSpace {
            ffmpeg: rect,
            physical,
        },
        _ => DesktopSpace::identity(physical),
    };
    let _ = DESKTOP_SPACE.set(space);
    space
}

/// Run a one-frame gdigrab capture and parse gdigrab's own desktop metrics.
#[cfg(windows)]
async fn detect_ffmpeg_desktop(ffmpeg: &Ffmpeg) -> Option<Rect> {
    let args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "info".to_string(),
        "-f".to_string(),
        "gdigrab".to_string(),
        "-framerate".to_string(),
        "1".to_string(),
        "-i".to_string(),
        "desktop".to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let out = tokio::time::timeout(Duration::from_secs(20), ffmpeg.capture_stderr(&args))
        .await
        .ok()?
        .ok()?;
    parse_gdigrab_desktop(&out)
}

#[cfg(not(windows))]
async fn detect_ffmpeg_desktop(_ffmpeg: &Ffmpeg) -> Option<Rect> {
    None
}

pub fn parse_gdigrab_desktop(stderr: &str) -> Option<Rect> {
    // "Capturing whole desktop as 3840x1083x32 at (-1920,0)"
    for line in stderr.lines() {
        let Some(idx) = line.find("Capturing whole desktop as ") else {
            continue;
        };
        let rest = &line[idx + "Capturing whole desktop as ".len()..];
        let mut parts = rest.split_whitespace();
        let size = parts.next()?;
        let at = parts.next()?;
        let coords = parts.next()?;
        let mut size_parts = size.split('x');
        let width: u32 = size_parts.next()?.parse().ok()?;
        let height: u32 = size_parts.next()?.parse().ok()?;
        if at != "at" {
            continue;
        }
        let coords = coords.trim_start_matches('(').trim_end_matches(')');
        let (xs, ys) = coords.split_once(',')?;
        let x: i32 = xs.parse().ok()?;
        let y: i32 = ys.parse().ok()?;
        return Some(Rect {
            x,
            y,
            width,
            height,
        });
    }
    None
}

/// Everything needed to spawn a capture pipeline.
#[derive(Debug, Clone)]
pub struct CapturePlan {
    pub source: SourceInfo,
    /// Physical rectangle (used to map pointer coordinates).
    pub input: Rect,
    /// Rectangle in ffmpeg's coordinate space.
    pub capture: Rect,
    pub output_width: u32,
    pub output_height: u32,
}

impl CapturePlan {
    pub fn build(source: SourceInfo, space: DesktopSpace, scale: f32) -> Result<Self> {
        let physical = source.rect();
        let is_demo = source.kind == "demo";

        if is_demo {
            let scale = scale.clamp(0.25, 1.0);
            let capture = physical.evenized();
            return Ok(Self {
                input: physical,
                capture,
                output_width: even((physical.width as f32 * scale).round().max(16.0) as u32).max(2),
                output_height: even((physical.height as f32 * scale).round().max(16.0) as u32)
                    .max(2),
                source,
            });
        }

        let mut capture = space.to_ffmpeg(physical);
        // Clamp to the virtual desktop ffmpeg can actually grab.
        let desktop = space.ffmpeg;
        capture.x = capture.x.max(desktop.x);
        capture.y = capture.y.max(desktop.y);
        let max_right = desktop.right();
        let max_bottom = desktop.bottom();
        if capture.right() > max_right {
            capture.width = (max_right - capture.x).max(0) as u32;
        }
        if capture.bottom() > max_bottom {
            capture.height = (max_bottom - capture.y).max(0) as u32;
        }
        capture = capture.evenized();
        if capture.width < 16 || capture.height < 16 {
            return Err(anyhow!(
                "capture region is too small or off-screen ({:?})",
                capture
            ));
        }
        let scale = scale.clamp(0.25, 1.0);
        let output_width = even((physical.width as f32 * scale).round().max(16.0) as u32).max(2);
        let output_height = even((physical.height as f32 * scale).round().max(16.0) as u32).max(2);
        Ok(Self {
            source,
            input: physical,
            capture,
            output_width,
            output_height,
        })
    }
}

/// ffmpeg input arguments for the capture backend.
pub fn input_args(plan: &CapturePlan, fps: u32) -> Vec<String> {
    if plan.source.kind == "demo" {
        return vec![
            // `-re` paces the synthetic source at wall-clock speed, exactly like
            // a real capture device. Without it ffmpeg would encode as fast as
            // possible and flood clients with a huge backlog.
            "-re".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            demo_filter(plan.capture.width, plan.capture.height, fps),
        ];
    }
    let capture = plan.capture;
    #[cfg(windows)]
    {
        vec![
            "-f".into(),
            "gdigrab".into(),
            "-framerate".into(),
            fps.to_string(),
            "-draw_mouse".into(),
            "1".into(),
            "-offset_x".into(),
            capture.x.to_string(),
            "-offset_y".into(),
            capture.y.to_string(),
            "-video_size".into(),
            format!("{}x{}", capture.width, capture.height),
            "-i".into(),
            "desktop".into(),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        vec![
            "-f".into(),
            "x11grab".into(),
            "-framerate".into(),
            fps.to_string(),
            "-video_size".into(),
            format!("{}x{}", capture.width, capture.height),
            "-i".into(),
            format!("{display}+{},{}", capture.x, capture.y),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            "-f".into(),
            "avfoundation".into(),
            "-framerate".into(),
            fps.to_string(),
            "-capture_cursor".into(),
            "1".into(),
            "-i".into(),
            "1:none".into(),
        ]
    }
}

/// Grab a single JPEG frame of any source (used by the source picker).
pub async fn thumbnail(ffmpeg: &Ffmpeg, plan: &CapturePlan, width: u32) -> Result<Vec<u8>> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-nostdin".to_string(),
    ];
    args.extend(input_args(plan, 1));
    args.extend([
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        format!("scale={}:-2:flags=fast_bilinear", width.max(64)),
        "-q:v".into(),
        "5".into(),
        "-f".into(),
        "mjpeg".into(),
        "pipe:1".into(),
    ]);
    let out = tokio::time::timeout(Duration::from_secs(15), ffmpeg.run_output(&args))
        .await
        .map_err(|_| anyhow!("thumbnail capture timed out"))??;
    if out.len() < 512 {
        return Err(anyhow!("thumbnail capture produced no image"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_union_and_intersect() {
        let a = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let b = Rect {
            x: 0,
            y: 3,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            Rect::union(a, b),
            Rect {
                x: -1920,
                y: 0,
                width: 3840,
                height: 1083
            }
        );
        // These two monitors only touch at x=0, so they do not overlap.
        assert_eq!(Rect::intersect(a, b), None);
        let c = Rect {
            x: -100,
            y: 10,
            width: 500,
            height: 400,
        };
        assert_eq!(
            Rect::intersect(a, c).unwrap(),
            Rect {
                x: -100,
                y: 10,
                width: 100,
                height: 400
            }
        );
    }

    #[test]
    fn demo_filter_is_well_formed() {
        let filter = demo_filter(1920, 1080, 30);
        assert!(filter.starts_with("gradients=s=1920x1080"), "{filter}");
        assert!(filter.contains("drawgrid=w=98:h=98"), "{filter}");
        if find_font(true).is_some() {
            assert!(filter.contains(",drawtext=fontfile="), "{filter}");
            assert!(filter.contains("fontcolor=0xe2e8f0"), "{filter}");
        }
    }

    #[test]
    fn parses_gdigrab_metrics() {
        let log = "[in#0 @ 0000] Capturing whole desktop as 3840x1083x32 at (-1920,0)\n";
        assert_eq!(
            parse_gdigrab_desktop(log),
            Some(Rect {
                x: -1920,
                y: 0,
                width: 3840,
                height: 1083
            })
        );
    }

    #[test]
    fn dpi_translation() {
        let physical = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1083,
        };
        // A 125% scaled secondary display at x=-1920 maps to x=-1536.
        let space = DesktopSpace {
            ffmpeg: Rect {
                x: -1536,
                y: 0,
                width: 3072,
                height: 866,
            },
            physical,
        };
        let translated = space.to_ffmpeg(Rect {
            x: -1920,
            y: 3,
            width: 1920,
            height: 1080,
        });
        assert_eq!(
            translated,
            Rect {
                x: -1536,
                y: 2,
                width: 1536,
                height: 864
            }
        );
    }

    #[test]
    fn plan_scales_output_evenly() {
        let source = SourceInfo {
            id: "monitor:0".into(),
            name: "Display".into(),
            kind: "monitor".into(),
            app: None,
            x: 0,
            y: 0,
            width: 1921,
            height: 1081,
            primary: true,
            builtin: false,
        };
        let space = DesktopSpace::identity(source.rect());
        let plan = CapturePlan::build(source, space, 0.75).unwrap();
        assert_eq!(plan.output_width % 2, 0);
        assert_eq!(plan.output_height % 2, 0);
        assert_eq!(plan.input.width, 1921);
        assert!(plan.capture.width <= plan.input.width);
    }
}
