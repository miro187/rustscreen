use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;

use crate::net::SESSION_IDLE_TIMEOUT;
use crate::util::{ct_eq, random_token};

/// Maximum failed password attempts per IP inside `FAILURE_WINDOW`.
const MAX_FAILURES: u32 = 8;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_SESSIONS: usize = 64;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(skip)]
    pub token: String,
    pub client_name: String,
    #[serde(skip)]
    pub last_seen: Instant,
}

#[derive(Debug)]
pub enum LoginError {
    InvalidPassword,
    TooManyAttempts { retry_after_secs: u64 },
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::InvalidPassword => write!(f, "Wrong password"),
            LoginError::TooManyAttempts { retry_after_secs } => write!(
                f,
                "Too many failed attempts. Try again in {}s.",
                retry_after_secs
            ),
        }
    }
}

#[derive(Debug, Default)]
struct FailureWindow {
    count: u32,
    first: Option<Instant>,
}

pub struct Auth {
    /// `None` means authentication is disabled.
    password: Option<String>,
    sessions: RwLock<HashMap<String, Session>>,
    failures: Mutex<HashMap<IpAddr, FailureWindow>>,
}

impl Auth {
    pub fn new(password: Option<String>) -> Self {
        Self {
            password,
            sessions: RwLock::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn requires_auth(&self) -> bool {
        self.password.is_some()
    }

    fn verify(&self, supplied: &str) -> bool {
        match &self.password {
            None => true,
            Some(expected) => ct_eq(expected.as_bytes(), supplied.trim().as_bytes()),
        }
    }

    fn check_rate_limit(&self, ip: IpAddr) -> Result<(), LoginError> {
        let mut failures = self.failures.lock().unwrap();
        let window = failures.entry(ip).or_default();
        match window.first {
            Some(first) if first.elapsed() > FAILURE_WINDOW => {
                window.first = Some(Instant::now());
                window.count = 0;
            }
            None => {
                window.first = Some(Instant::now());
                window.count = 0;
            }
            _ => {}
        }
        if window.count >= MAX_FAILURES {
            let elapsed = window.first.unwrap_or_else(Instant::now).elapsed();
            let retry = FAILURE_WINDOW.saturating_sub(elapsed).as_secs().max(1);
            return Err(LoginError::TooManyAttempts {
                retry_after_secs: retry,
            });
        }
        Ok(())
    }

    fn note_failure(&self, ip: IpAddr) {
        let mut failures = self.failures.lock().unwrap();
        let window = failures.entry(ip).or_default();
        if window.first.is_none() {
            window.first = Some(Instant::now());
        }
        window.count = window.count.saturating_add(1);
    }

    /// Authenticate and create a session. A fresh token is returned on success.
    pub async fn login(
        &self,
        ip: IpAddr,
        password: &str,
        client_name: String,
    ) -> Result<Session, LoginError> {
        self.check_rate_limit(ip)?;
        if !self.verify(password) {
            self.note_failure(ip);
            return Err(LoginError::InvalidPassword);
        }
        {
            let mut failures = self.failures.lock().unwrap();
            failures.remove(&ip);
        }

        let token = random_token(24);
        let session = Session {
            token: token.clone(),
            client_name,
            last_seen: Instant::now(),
        };

        let mut sessions = self.sessions.write().await;
        // Expire old sessions and bound memory usage.
        sessions.retain(|_, s| s.last_seen.elapsed() < SESSION_IDLE_TIMEOUT);
        if sessions.len() >= MAX_SESSIONS {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(token, session.clone());
        Ok(session)
    }

    /// Returns the session for `token` and refreshes its idle timer.
    pub async fn validate(&self, token: &str) -> Option<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(token)?;
        if session.last_seen.elapsed() > SESSION_IDLE_TIMEOUT {
            sessions.remove(token);
            return None;
        }
        session.last_seen = Instant::now();
        Some(session.clone())
    }

    pub async fn logout(&self, token: &str) {
        self.sessions.write().await.remove(token);
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// Turn a User-Agent header into something short and human readable.
pub fn friendly_client_name(user_agent: &str) -> String {
    let ua = user_agent.to_ascii_lowercase();
    let browser = if ua.contains("edg/") {
        "Edge"
    } else if ua.contains("opr/") || ua.contains("opera") {
        "Opera"
    } else if ua.contains("firefox") {
        "Firefox"
    } else if ua.contains("chrome") || ua.contains("crios") {
        "Chrome"
    } else if ua.contains("safari") {
        "Safari"
    } else if ua.contains("rustscreen") {
        "RustScreen"
    } else {
        "Browser"
    };
    let os = if ua.contains("android") {
        "Android"
    } else if ua.contains("iphone") || ua.contains("ipad") || ua.contains("ipod") {
        "iOS"
    } else if ua.contains("windows") {
        "Windows"
    } else if ua.contains("mac os") || ua.contains("macintosh") {
        "macOS"
    } else if ua.contains("linux") {
        "Linux"
    } else {
        "Unknown"
    };
    format!("{browser} on {os}")
}

/// The browser sends its platform + language; this is only used for display.
pub fn sanitize_name(name: &str) -> String {
    let filtered: String = name.chars().filter(|c| !c.is_control()).take(48).collect();
    let filtered = filtered.trim();
    if filtered.is_empty() {
        return "Viewer".to_string();
    }
    // Pull a readable label out of a long UA string if needed.
    if filtered.len() > 40 && filtered.contains(' ') {
        friendly_client_name(filtered)
    } else {
        filtered.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "192.168.1.10".parse().unwrap()
    }

    #[tokio::test]
    async fn rejects_wrong_password_and_accepts_right_one() {
        let auth = Auth::new(Some("123456".into()));
        assert!(matches!(
            auth.login(ip(), "000000", "test".into()).await,
            Err(LoginError::InvalidPassword)
        ));
        let session = auth.login(ip(), " 123456 ", "test".into()).await.unwrap();
        assert!(auth.validate(&session.token).await.is_some());
    }

    #[tokio::test]
    async fn rate_limits_after_repeated_failures() {
        let auth = Auth::new(Some("123456".into()));
        for _ in 0..MAX_FAILURES {
            let _ = auth.login(ip(), "nope", "test".into()).await;
        }
        assert!(matches!(
            auth.login(ip(), "123456", "test".into()).await,
            Err(LoginError::TooManyAttempts { .. })
        ));
    }

    #[tokio::test]
    async fn logout_invalidates_token() {
        let auth = Auth::new(Some("pw".into()));
        let s = auth.login(ip(), "pw", "t".into()).await.unwrap();
        auth.logout(&s.token).await;
        assert!(auth.validate(&s.token).await.is_none());
    }

    #[tokio::test]
    async fn no_password_disables_auth() {
        let auth = Auth::new(None);
        assert!(!auth.requires_auth());
        let s = auth.login(ip(), "", "t".into()).await.unwrap();
        assert!(auth.validate(&s.token).await.is_some());
    }

    #[test]
    fn ua_names() {
        assert_eq!(
            friendly_client_name("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120"),
            "Chrome on Windows"
        );
        assert_eq!(
            friendly_client_name("Mozilla/5.0 (iPad; CPU OS 17_0) Version/17.0 Safari/604.1"),
            "Safari on iOS"
        );
    }
}
