//! Minimal H.264 (Annex-B) helpers.
//!
//! ffmpeg is instructed to insert an Access Unit Delimiter (AUD, NAL type 9)
//! before every frame. That gives us a trivial and robust way to split the
//! Annex-B byte stream into access units without parsing slice headers. Any
//! `00 00 01` sequence inside a NAL payload is prevented by the emulation
//! prevention bytes that H.264 requires, so searching for the AUD start code
//! is unambiguous.

use bytes::{Bytes, BytesMut};

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// One encoded video access unit (one displayable frame).
#[derive(Debug, Clone)]
pub struct AnnexBFrame {
    /// Full access unit in Annex-B format (start codes included).
    pub data: Bytes,
    /// True when the AU contains an IDR (key) frame.
    pub keyframe: bool,
    /// Set when an SPS was (re)discovered in this AU.
    pub codec: Option<String>,
}

#[derive(Debug, Default)]
pub struct AnnexBSplitter {
    buf: BytesMut,
    scan: usize,
    first_aud: Option<usize>,
    sps: Option<Bytes>,
    pps: Option<Bytes>,
    sps_pps: Option<Bytes>,
    codec: Option<String>,
}

impl AnnexBSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cached SPS/PPS NAL units (with start codes) that decoders need before
    /// the first key frame.
    #[cfg(test)]
    pub fn sps_pps(&self) -> Option<Bytes> {
        self.sps_pps.clone()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<AnnexBFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();

        loop {
            let Some((aud_start, resume)) = find_aud(&self.buf, self.scan) else {
                self.scan = self.buf.len().saturating_sub(3);
                break;
            };

            match self.first_aud {
                Some(first) if aud_start > first => {
                    let au = self.buf.split_to(aud_start).freeze();
                    if let Some(frame) = self.make_frame(au) {
                        frames.push(frame);
                    }
                    self.first_aud = Some(0);
                    self.scan = 0;
                }
                Some(_) => {
                    self.scan = resume;
                }
                None => {
                    self.first_aud = Some(aud_start);
                    self.scan = resume;
                }
            }

            // Drop already-consumed prefix before the first AUD (if any).
            if let Some(first) = self.first_aud {
                if first > 0 {
                    let _ = self.buf.split_to(first);
                    self.first_aud = Some(0);
                    self.scan = self.scan.saturating_sub(first);
                }
            }
        }

        frames
    }

    /// Emit whatever is buffered at end of stream (usually the last frame).
    pub fn flush(&mut self) -> Option<AnnexBFrame> {
        if self.buf.is_empty() {
            return None;
        }
        let au = std::mem::take(&mut self.buf).freeze();
        self.scan = 0;
        self.first_aud = None;
        self.make_frame(au)
    }

    fn make_frame(&mut self, au: Bytes) -> Option<AnnexBFrame> {
        let mut keyframe = false;
        let mut has_vcl = false;
        let mut sps_here = false;
        let mut pps_here = false;

        for nal in NalIter::new(&au) {
            let Some(header) = nal.first() else { continue };
            let typ = header & 0x1f;
            match typ {
                NAL_IDR => {
                    keyframe = true;
                    has_vcl = true;
                }
                NAL_SLICE => has_vcl = true,
                NAL_SPS => {
                    sps_here = true;
                    let nal_bytes = with_start_code(nal);
                    if self.sps.as_deref() != Some(nal_bytes.as_ref()) {
                        self.sps = Some(nal_bytes);
                        self.codec = codec_from_sps(nal);
                        self.refresh_sps_pps();
                    }
                }
                NAL_PPS => {
                    pps_here = true;
                    let nal_bytes = with_start_code(nal);
                    if self.pps.as_deref() != Some(nal_bytes.as_ref()) {
                        self.pps = Some(nal_bytes);
                        self.refresh_sps_pps();
                    }
                }
                _ => {}
            }
        }

        if !has_vcl {
            return None;
        }

        let mut data = au;
        if keyframe {
            if let Some(headers) = &self.sps_pps {
                let has_sps = sps_here;
                let has_pps = pps_here;
                if !(has_sps && has_pps) {
                    let mut combined = BytesMut::with_capacity(headers.len() + data.len());
                    combined.extend_from_slice(headers);
                    combined.extend_from_slice(&data);
                    data = combined.freeze();
                }
            }
        }

        let codec = if sps_here { self.codec.clone() } else { None };
        Some(AnnexBFrame {
            data,
            keyframe,
            codec,
        })
    }

    fn refresh_sps_pps(&mut self) {
        if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
            let mut out = BytesMut::with_capacity(sps.len() + pps.len());
            out.extend_from_slice(sps);
            out.extend_from_slice(pps);
            self.sps_pps = Some(out.freeze());
        }
    }
}

fn with_start_code(nal: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(nal.len() + 4);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
    out.freeze()
}

/// Find the next AUD start code at or after `from`.
/// Returns the start index of the AUD (handling 4-byte start codes) and the
/// offset to continue scanning from.
fn find_aud(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let last_start = buf.len().checked_sub(4)?;
    let mut i = from;
    if i > last_start {
        return None;
    }
    while i <= last_start {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && (buf[i + 3] & 0x1f) == NAL_AUD {
            let start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
            return Some((start, i + 4));
        }
        i += 1;
    }
    None
}

/// Iterate over NAL units (start codes stripped).
pub struct NalIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> NalIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.data.len() {
                return None;
            }
            let offset = find_start_code(&self.data[self.pos..])?;
            let pattern = self.pos + offset;
            let nal_start = pattern + 3;
            if nal_start >= self.data.len() {
                self.pos = self.data.len();
                return None;
            }
            let next_pattern = find_start_code(&self.data[nal_start..])
                .map(|o| nal_start + o)
                .unwrap_or(self.data.len());
            // Trim the extra zero byte of a 4-byte start code.
            let end = if next_pattern > nal_start && self.data[next_pattern - 1] == 0 {
                next_pattern - 1
            } else {
                next_pattern
            };
            self.pos = next_pattern.max(nal_start);
            if end > nal_start {
                return Some(&self.data[nal_start..end]);
            }
        }
    }
}

fn find_start_code(data: &[u8]) -> Option<usize> {
    if data.len() < 3 {
        return None;
    }
    (0..data.len().saturating_sub(2))
        .find(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
}

/// Build the WebCodecs/MSE `avc1.PPCCLL` codec string from an SPS NAL.
pub fn codec_from_sps(nal: &[u8]) -> Option<String> {
    // nal[0] is the NAL header; the profile follows in the RBSP.
    let rbsp = unescape_rbsp(nal.get(1..)?);
    if rbsp.len() < 3 {
        return None;
    }
    Some(format!(
        "avc1.{:02x}{:02x}{:02x}",
        rbsp[0], rbsp[1], rbsp[2]
    ))
}

fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Baseline profile, level 3.0 SPS (from a real x264 stream).
    const SPS: &[u8] = &[
        0x67, 0x42, 0xc0, 0x1e, 0xda, 0x05, 0x07, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40,
        0x00, 0x00, 0x0f, 0x03, 0xc5, 0x0a, 0xa8,
    ];
    const PPS: &[u8] = &[0x68, 0xce, 0x0f, 0xc8];

    fn nal(n: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(n);
        v
    }

    fn aud() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x09, 0x10]
    }

    fn idr(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x65];
        v.extend_from_slice(payload);
        v
    }

    fn slice(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x41];
        v.extend_from_slice(payload);
        v
    }

    fn stream() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(aud());
        v.extend(nal(SPS));
        v.extend(nal(PPS));
        v.extend(nal(&[0x06, 0x05, 0x01, 0x02])); // SEI
        v.extend(idr(&[0x88, 0x84, 0x21, 0x99]));
        v.extend(aud());
        v.extend(slice(&[0x9a, 0x22, 0x11]));
        v.extend(aud());
        v.extend(slice(&[0x9a, 0x22, 0x33]));
        v
    }

    #[test]
    fn splits_access_units() {
        let mut splitter = AnnexBSplitter::new();
        let data = stream();
        // Feed in tiny chunks to exercise the incremental scanner.
        let mut frames = Vec::new();
        for chunk in data.chunks(3) {
            frames.extend(splitter.push(chunk));
        }
        if let Some(f) = splitter.flush() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 3, "expected 1 IDR + 2 slices");
        assert!(frames[0].keyframe);
        assert_eq!(frames[0].codec.as_deref(), Some("avc1.42c01e"));
        assert!(!frames[1].keyframe);
        assert!(!frames[2].keyframe);
        // Late joiners need SPS/PPS in-band with key frames.
        assert!(frames[0].data.windows(4).any(|w| w == [0, 0, 1, 0x67]));
    }

    #[test]
    fn prepends_headers_for_late_joiners() {
        let mut splitter = AnnexBSplitter::new();
        let mut frames = splitter.push(&stream());
        // Force the buffer out so the 3rd AU is emitted too.
        frames.extend(splitter.push(&aud()));
        assert!(!frames.is_empty());
        assert!(frames[0].keyframe);
        let sps = splitter.sps_pps().expect("cached SPS/PPS");
        assert!(sps.starts_with(&[0, 0, 0, 1, 0x67]));
    }

    #[test]
    fn codec_string_from_sps() {
        assert_eq!(codec_from_sps(SPS).as_deref(), Some("avc1.42c01e"));
    }

    #[test]
    fn handles_three_byte_start_codes() {
        let mut splitter = AnnexBSplitter::new();
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 1, 9, 0x10]);
        data.extend_from_slice(&[0, 0, 1, 0x67]);
        data.extend_from_slice(&SPS[1..]);
        data.extend_from_slice(&[0, 0, 1, 0x68]);
        data.extend_from_slice(&PPS[1..]);
        data.extend_from_slice(&[0, 0, 1, 0x65, 0x11, 0x22]);
        data.extend_from_slice(&[0, 0, 1, 9, 0x10]);
        data.extend_from_slice(&[0, 0, 1, 0x41, 0x33]);
        let frames = splitter.push(&data);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].keyframe);
    }
}
