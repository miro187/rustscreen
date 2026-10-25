//! Self-signed TLS material management.
//!
//! A self-signed certificate is generated once and reused, so the browser's
//! "accept this certificate" exception survives restarts. Passing `--cert` and
//! `--key` uses user-provided files instead.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::util::config_dir;

pub struct TlsMaterial {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// Where the generated certificate lives (for display / deletion).
    pub path: Option<PathBuf>,
    pub generated: bool,
}

/// Load user-provided certificates or generate/reuse a self-signed pair.
pub fn load_or_generate(
    custom: Option<(PathBuf, PathBuf)>,
    lan_ips: &[IpAddr],
    hostname: &str,
) -> Result<TlsMaterial> {
    if let Some((cert_path, key_path)) = custom {
        let cert_pem = fs::read(&cert_path)
            .with_context(|| format!("reading certificate {}", cert_path.display()))?;
        let key_pem =
            fs::read(&key_path).with_context(|| format!("reading key {}", key_path.display()))?;
        return Ok(TlsMaterial {
            cert_pem,
            key_pem,
            path: Some(cert_path),
            generated: false,
        });
    }

    let dir = config_dir().join("tls");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if cert_path.is_file() && key_path.is_file() {
        if let (Ok(cert_pem), Ok(key_pem)) = (fs::read(&cert_path), fs::read(&key_path)) {
            if !cert_pem.is_empty() && !key_pem.is_empty() {
                return Ok(TlsMaterial {
                    cert_pem,
                    key_pem,
                    path: Some(cert_path),
                    generated: false,
                });
            }
        }
    }

    let material = generate(lan_ips, hostname)?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&cert_path, &material.cert_pem)
        .with_context(|| format!("writing {}", cert_path.display()))?;
    fs::write(&key_path, &material.key_pem)
        .with_context(|| format!("writing {}", key_path.display()))?;
    restrict_permissions(&key_path);
    Ok(TlsMaterial {
        path: Some(cert_path),
        generated: true,
        ..material
    })
}

fn generate(lan_ips: &[IpAddr], hostname: &str) -> Result<TlsMaterial> {
    let mut sans: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if !hostname.trim().is_empty() && !hostname.eq_ignore_ascii_case("localhost") {
        sans.push(hostname.trim().to_string());
    }
    for ip in lan_ips {
        sans.push(ip.to_string());
    }
    sans.sort();
    sans.dedup();

    let mut params = rcgen::CertificateParams::new(sans)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "RustScreen");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "RustScreen (self-signed)");
    params.is_ca = rcgen::IsCa::NoCa;
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(TlsMaterial {
        cert_pem: cert.pem().into_bytes(),
        key_pem: key.serialize_pem().into_bytes(),
        path: None,
        generated: true,
    })
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn generates_certificate_with_ips() {
        let material = generate(
            &[
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ],
            "test-host",
        )
        .unwrap();
        let pem = String::from_utf8(material.cert_pem).unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE"));
        let key = String::from_utf8(material.key_pem).unwrap();
        assert!(key.contains("BEGIN PRIVATE KEY"));
    }
}
