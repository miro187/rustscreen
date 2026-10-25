use anyhow::{Context, Result};
use qrcode::render::{svg, unicode};
use qrcode::{EcLevel, QrCode};

/// Compact QR code using Unicode half-blocks, suitable for a terminal.
pub fn terminal(text: &str) -> Result<String> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)
        .context("QR code is too large")?;
    Ok(code
        .render::<unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .build())
}

/// QR code as a standalone SVG document.
pub fn svg(text: &str, size_px: u32) -> Result<String> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)
        .context("QR code is too large")?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(size_px, size_px)
        .quiet_zone(true)
        .dark_color(svg::Color("#0f172a"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_contains_rect() {
        let out = svg("https://192.168.1.2:8443", 220).unwrap();
        assert!(out.starts_with("<?xml"));
        assert!(out.contains("<svg"));
        let width_attr = out.split("width=\"").nth(1).unwrap_or("0");
        let width: u32 = width_attr
            .split('"')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        assert!(width >= 220, "svg width was {width}");
    }

    #[test]
    fn terminal_has_multiple_lines() {
        let out = terminal("https://192.168.1.2:8443").unwrap();
        assert!(out.lines().count() > 8);
    }
}
