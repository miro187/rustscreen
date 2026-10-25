# Security policy

RustScreen gives another device full mouse and keyboard control of a computer.
Treat it like a remote-access tool, not like a public web service.

## Supported versions

Only the latest release receives security fixes. This project is at an early
stage; review the code before using it in a sensitive environment.

## Threat model & scope

RustScreen is built for a **trusted local network** (home/office LAN, VPN).

What it does protect against:

- **Unauthenticated viewing/control** — a PIN/password is required, sessions use
  HTTP-only `SameSite=Strict` cookies, and failed logins are rate-limited per IP
  (8 attempts/minute, then a cooldown).
- **Cross-site request forgery** — state-changing requests require the session
  cookie, and cookies are not sent on cross-site requests. WebSocket upgrades
  check the `Origin` header against `Host`.
- **Cross-site scripting** — a strict Content-Security-Policy forbids inline and
  third-party scripts; the UI never inserts untrusted HTML.
- **Network sniffing** — TLS is enabled by default with a self-signed
  certificate generated locally. Traffic is encrypted; authenticity relies on
  the browser's one-time certificate warning (or your own certificate via
  `--cert/--key`).
- **Control races** — `--control single` implements an exclusive lease that is
  released when the controller disconnects. `--control none` blocks all input.
- **Orphaned capture processes** — on Windows the server runs in a job object
  with `KILL_ON_JOB_CLOSE`, so ffmpeg children die with the server, even after a
  crash.
- **Input flooding** — each control socket is limited to ~800 events per 2 s.

Out of scope / known limitations:

- **Do not expose RustScreen directly to the internet.** If you need remote
  access, put it behind a VPN (WireGuard/Tailscale) or an authenticating reverse
  proxy with a real TLS certificate.
- The self-signed certificate is not validated by a CA; users must verify and
  accept the warning once. The certificate is reused across restarts.
- A viewer who knows the PIN can change stream settings (resolution, bitrate,
  source) unless control is disabled. There is no per-viewer role system.
- The clipboard is not synchronized. Pasting sends text into the remote
  session; copying from the remote is not supported yet.
- Audio capture is optional and, for system sound on Windows, requires a
  third-party loopback device (e.g. VB-Cable) whose security is its own concern.
- Screen content protected by DRM, the UAC secure desktop, and the lock screen
  cannot be captured by design.
- Anyone with local access to the host can read the generated TLS private key
  from `%APPDATA%\rustscreen\tls\key.pem` (ACL-protected by the user profile) and
  can start their own server instance.

## Hardening checklist

- Keep the default PIN or set a strong `--password`; never use `--password none`
  on a shared network.
- Keep TLS enabled unless you are on a fully trusted wired network.
- Use `--control none` for presentations, or `--control single` for support
  sessions.
- Bind to a specific interface (`--bind 192.168.1.42`) if you have multiple
  networks.
- Close the app when you are done; it does not run as a background service.
- Review the ffmpeg build you install; it processes untrusted pixels on the
  host.

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Use GitHub's
[private vulnerability reporting](https://github.com/miro187/rustscreen/security/advisories/new)
(Security → Report a vulnerability). Include reproduction steps and the version
(`rustscreen --version`).

You can expect an acknowledgement within a few days. As an early-stage
volunteer project, fixes are best-effort.
