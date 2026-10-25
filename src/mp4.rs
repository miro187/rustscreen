//! Incremental parser for fragmented MP4 (used by ffmpeg's `-f mp4` output
//! with `+frag_keyframe+empty_moov`). The stream consists of an init segment
//! (`ftyp` + `moov`) followed by one `moof`/`mdat` pair per keyframe interval.

use bytes::{Bytes, BytesMut};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mp4Event {
    /// Assembled initialization segment; browsers must receive this before
    /// any media fragment.
    Init(Bytes),
    /// A complete top-level box (`moof`, `mdat`, `styp`, ...).
    Media { typ: [u8; 4], data: Bytes },
}

#[derive(Debug, Default)]
pub struct Mp4Splitter {
    buf: BytesMut,
    init: BytesMut,
    got_moov: bool,
}

impl Mp4Splitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Mp4Event> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some((header_len, size, typ)) = peek_box(&self.buf) {
            if self.buf.len() < size {
                break;
            }
            let box_bytes = self.buf.split_to(size).freeze();

            if !self.got_moov {
                // Everything up to and including moov forms the init segment.
                match &typ {
                    b"ftyp" | b"free" | b"wide" => self.init.extend_from_slice(&box_bytes),
                    b"moov" => {
                        self.init.extend_from_slice(&box_bytes);
                        self.got_moov = true;
                        let init = std::mem::take(&mut self.init).freeze();
                        events.push(Mp4Event::Init(init));
                    }
                    _ => {
                        // Unknown pre-init box: keep it, MSE ignores unknowns.
                        self.init.extend_from_slice(&box_bytes);
                    }
                }
                continue;
            }

            // Media phase. sidx would reference fragment offsets we do not
            // rewrite, so it is safer to drop it.
            if &typ == b"sidx" || &typ == b"mfra" {
                continue;
            }
            debug_assert_eq!(header_len + (size - header_len), size);
            events.push(Mp4Event::Media {
                typ,
                data: box_bytes,
            });
        }

        events
    }
}

/// Parse the box header at the front of `buf`.
/// Returns (header length, total box size, box type).
fn peek_box(buf: &[u8]) -> Option<(usize, usize, [u8; 4])> {
    if buf.len() < 8 {
        return None;
    }
    let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let mut typ = [0u8; 4];
    typ.copy_from_slice(&buf[4..8]);
    if size32 == 1 {
        if buf.len() < 16 {
            return None;
        }
        let size64 = u64::from_be_bytes(buf[8..16].try_into().ok()?);
        if !(16..=64 * 1024 * 1024).contains(&size64) {
            return None;
        }
        Some((16, size64 as usize, typ))
    } else if size32 == 0 {
        // Extends to end of stream; cannot be framed incrementally.
        None
    } else if !(8..=64 * 1024 * 1024).contains(&size32) {
        None
    } else {
        Some((8, size32 as usize, typ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_bytes(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (payload.len() + 8) as u32;
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn splits_init_and_fragments() {
        let mut data = Vec::new();
        data.extend(box_bytes(b"ftyp", b"isom0000"));
        data.extend(box_bytes(b"moov", b"MOOVDATA"));
        data.extend(box_bytes(b"moof", b"MOOF"));
        data.extend(box_bytes(b"mdat", b"PAYLOAD"));

        let mut splitter = Mp4Splitter::new();
        let mut events = Vec::new();
        for chunk in data.chunks(5) {
            events.extend(splitter.push(chunk));
        }
        assert_eq!(events.len(), 3);
        match &events[0] {
            Mp4Event::Init(init) => {
                assert_eq!(&init[4..8], b"ftyp");
                assert!(init.windows(4).any(|w| w == b"moov"));
            }
            _ => panic!("first event must be init"),
        }
        match &events[1] {
            Mp4Event::Media { typ, .. } => assert_eq!(typ, b"moof"),
            _ => panic!("expected moof"),
        }
        match &events[2] {
            Mp4Event::Media { typ, .. } => assert_eq!(typ, b"mdat"),
            _ => panic!("expected mdat"),
        }
    }

    #[test]
    fn drops_sidx() {
        let mut data = Vec::new();
        data.extend(box_bytes(b"ftyp", b"x"));
        data.extend(box_bytes(b"moov", b"y"));
        data.extend(box_bytes(b"sidx", b"z"));
        data.extend(box_bytes(b"moof", b"m"));
        let mut splitter = Mp4Splitter::new();
        let events = splitter.push(&data);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn handles_large_size() {
        let mut data = Vec::new();
        data.extend(box_bytes(b"ftyp", b"x"));
        data.extend(box_bytes(b"moov", b"y"));
        // largesize mdat
        let payload = b"ABCD";
        let mut mdat = Vec::new();
        mdat.extend_from_slice(&1u32.to_be_bytes());
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&(16u64 + payload.len() as u64).to_be_bytes());
        mdat.extend_from_slice(payload);
        data.extend(mdat);
        let mut splitter = Mp4Splitter::new();
        let events = splitter.push(&data);
        assert_eq!(events.len(), 2);
        match &events[1] {
            Mp4Event::Media { typ, data } => {
                assert_eq!(typ, b"mdat");
                assert_eq!(data.len(), 20);
            }
            _ => panic!(),
        }
    }
}

/// Extract the `avc1.PPCCLL` codec string from an fMP4 init segment by
/// locating the `avcC` box inside moov.
pub fn codec_from_init(init: &[u8]) -> Option<String> {
    let needle = b"avcC";
    let mut i = 0;
    while i + 8 <= init.len() {
        if &init[i..i + 4] == needle {
            let v = &init[i + 4..];
            if v.len() >= 4 {
                return Some(format!("avc1.{:02x}{:02x}{:02x}", v[1], v[2], v[3]));
            }
            return None;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod codec_tests {
    use super::codec_from_init;

    #[test]
    fn finds_avcc_profile() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 8]);
        data.extend_from_slice(b"avcC");
        data.extend_from_slice(&[0x01, 0x42, 0xc0, 0x1e, 0xff]);
        assert_eq!(codec_from_init(&data).as_deref(), Some("avc1.42c01e"));
    }
}
