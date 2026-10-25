use std::io::IsTerminal;
use std::path::PathBuf;

/// Keep ffmpeg children from outliving the server, even on a crash.
///
/// The server process is placed in a Windows job object with
/// `KILL_ON_JOB_CLOSE`. Every child inherits membership, so when the server
/// dies (gracefully or not) the OS terminates the capture processes.
pub fn protect_child_processes() {
    #[cfg(windows)]
    unsafe {
        use std::ffi::c_void;

        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
        // The handle is intentionally leaked: it has to stay open for the
        // lifetime of the process. Windows closes it on exit, which kills the
        // job members (ffmpeg).
    }
}

/// Enable UTF-8 output and ANSI escapes in classic Windows consoles.
pub fn prepare_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
        };
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) != 0 {
            let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
        let _ = SetConsoleOutputCP(65001);
    }
}

/// Whether ANSI colors should be written to stdout.
pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

pub struct Paint;

impl Paint {
    pub fn bold(s: impl AsRef<str>) -> String {
        wrap("\x1b[1m", s.as_ref(), "\x1b[0m")
    }
    pub fn dim(s: impl AsRef<str>) -> String {
        wrap("\x1b[2m", s.as_ref(), "\x1b[0m")
    }
    pub fn green(s: impl AsRef<str>) -> String {
        wrap("\x1b[32m", s.as_ref(), "\x1b[0m")
    }
    pub fn yellow(s: impl AsRef<str>) -> String {
        wrap("\x1b[33m", s.as_ref(), "\x1b[0m")
    }
    pub fn red(s: impl AsRef<str>) -> String {
        wrap("\x1b[31m", s.as_ref(), "\x1b[0m")
    }
    pub fn cyan(s: impl AsRef<str>) -> String {
        wrap("\x1b[36m", s.as_ref(), "\x1b[0m")
    }
}

fn wrap(prefix: &str, s: &str, suffix: &str) -> String {
    if color_enabled() {
        format!("{prefix}{s}{suffix}")
    } else {
        s.to_string()
    }
}

/// Directory used for the generated TLS certificate.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RUSTSCREEN_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("rustscreen");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("rustscreen");
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("rustscreen");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("rustscreen");
    }
    PathBuf::from(".rustscreen")
}

pub fn hostname() -> String {
    #[cfg(windows)]
    {
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.trim().is_empty() {
                return name;
            }
        }
    }
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    "rustscreen-host".to_string()
}

pub fn os_name() -> &'static str {
    #[cfg(windows)]
    {
        "Windows"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "Linux"
    }
}

/// Constant-time-ish byte comparison used for password checks.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn random_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

pub fn random_pin() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:06}")
}

/// Even dimension (many H.264 encoders reject odd sizes).
pub fn even(v: u32) -> u32 {
    (v / 2) * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_six_digits() {
        for _ in 0..50 {
            let pin = random_pin();
            assert_eq!(pin.len(), 6);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn even_rounds_down() {
        assert_eq!(even(1921), 1920);
        assert_eq!(even(1080), 1080);
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"secret", b"secre"));
    }
}
