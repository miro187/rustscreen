# Changelog

All notable changes to RustScreen are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-10-25

First public release.

### Added

- Self-hosted host server with an embedded browser client (single binary).
- H.264 streaming over WebCodecs (Annex-B/WebSocket) and fragmented MP4
  (Media Source Extensions/HTTP).
- Mouse, wheel, keyboard, and Unicode/clipboard text injection with
  multi-monitor and DPI coordinate mapping.
- Display, window, all-displays, and privacy-safe demo capture sources with
  live thumbnails and on-the-fly source switching.
- Quality controls: FPS, bitrate, scale, and encoder (NVENC, Quick Sync, AMF,
  Media Foundation, libx264) with automatic probing and fallback.
- PIN authentication, session cookies, per-IP login rate limiting, WebSocket
  origin validation, CSP and security headers.
- Self-signed TLS generated on first run, custom certificate support, and an
  optional plain-HTTP mode for trusted networks.
- Control modes: all viewers, exclusive single controller, view-only.
- Optional audio streaming (fMP4/AAC) from a DirectShow device or system
  loopback device.
- Startup banner with LAN URLs, QR code, PIN, capture source, and encoder
  selection.
- Windows job object protection so ffmpeg cannot outlive the server.
- Unit tests for parsers, auth, coordinate mapping, TLS, and configuration.
- GitHub Actions CI and tagged release workflow.
