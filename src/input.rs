//! Remote input injection (mouse, keyboard, text) via enigo.
//!
//! All events are serialized onto a dedicated OS thread: injection must be
//! ordered and must never block the async runtime.

use std::collections::HashSet;
use std::sync::mpsc::{self, Sender};
use std::thread;

#[cfg(not(windows))]
use enigo::Coordinate;
use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;

use crate::capture::Rect;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum MouseBtn {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl MouseBtn {
    fn to_enigo(self) -> Button {
        match self {
            MouseBtn::Left => Button::Left,
            MouseBtn::Middle => Button::Middle,
            MouseBtn::Right => Button::Right,
            MouseBtn::Back => Button::Back,
            MouseBtn::Forward => Button::Forward,
        }
    }
}

/// Messages accepted from browsers on the control WebSocket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum InputEvent {
    /// Pointer position, normalized to the captured rectangle (0.0..1.0).
    Move {
        x: f64,
        y: f64,
    },
    Down {
        button: MouseBtn,
        x: Option<f64>,
        y: Option<f64>,
    },
    Up {
        button: MouseBtn,
    },
    /// Wheel deltas in "lines"; positive y scrolls down, positive x scrolls right.
    Wheel {
        dx: f64,
        dy: f64,
    },
    /// Physical key by browser `KeyboardEvent.code`.
    Key {
        code: String,
        down: bool,
    },
    /// Unicode text (phone keyboards, clipboard paste, IME).
    Text {
        text: String,
    },
}

#[derive(Debug)]
enum Command {
    Event { event: InputEvent, rect: Rect },
    ReleaseAll,
}

#[derive(Clone)]
pub struct InputHandle {
    tx: Sender<Command>,
}

impl InputHandle {
    /// Spawn the input worker. Fails soft: if input injection is unavailable the
    /// server still runs and logs an error when events arrive.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        thread::Builder::new()
            .name("rustscreen-input".into())
            .spawn(move || {
                let mut worker = Worker::new();
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Command::Event { event, rect } => worker.handle(event, rect),
                        Command::ReleaseAll => worker.release_all(),
                    }
                }
            })
            .expect("spawn input thread");
        Self { tx }
    }

    pub fn send(&self, event: InputEvent, rect: Rect) {
        let _ = self.tx.send(Command::Event { event, rect });
    }

    pub fn release_all(&self) {
        let _ = self.tx.send(Command::ReleaseAll);
    }
}

struct Worker {
    enigo: Option<Enigo>,
    held_buttons: HashSet<MouseBtn>,
    held_keys: HashSet<String>,
    failed: bool,
}

impl Worker {
    fn new() -> Self {
        let enigo = match Enigo::new(&Settings {
            release_keys_when_dropped: true,
            ..Default::default()
        }) {
            Ok(enigo) => Some(enigo),
            Err(err) => {
                tracing::error!("input injection unavailable: {err}");
                None
            }
        };
        Self {
            enigo,
            held_buttons: HashSet::new(),
            held_keys: HashSet::new(),
            failed: false,
        }
    }

    fn enigo(&mut self) -> Option<&mut Enigo> {
        if self.enigo.is_none() && !self.failed {
            self.failed = true;
            tracing::warn!("remote control is unavailable on this host");
        }
        self.enigo.as_mut()
    }

    fn handle(&mut self, event: InputEvent, rect: Rect) {
        match event {
            InputEvent::Move { x, y } => {
                let (px, py) = point_on_rect(x, y, rect);
                self.move_pointer(px, py);
            }
            InputEvent::Down { button, x, y } => {
                if let (Some(x), Some(y)) = (x, y) {
                    let (px, py) = point_on_rect(x, y, rect);
                    self.move_pointer(px, py);
                }
                if let Some(enigo) = self.enigo() {
                    if let Err(e) = enigo.button(button.to_enigo(), Direction::Press) {
                        tracing::debug!("mouse down failed: {e}");
                    } else {
                        self.held_buttons.insert(button);
                    }
                }
            }
            InputEvent::Up { button } => {
                if let Some(enigo) = self.enigo() {
                    if let Err(e) = enigo.button(button.to_enigo(), Direction::Release) {
                        tracing::debug!("mouse up failed: {e}");
                    }
                    self.held_buttons.remove(&button);
                }
            }
            InputEvent::Wheel { dx, dy } => {
                if let Some(enigo) = self.enigo() {
                    let v = dy.round() as i32;
                    if v != 0 {
                        let _ = enigo.scroll(v.clamp(-20, 20), Axis::Vertical);
                    }
                    let h = dx.round() as i32;
                    if h != 0 {
                        let _ = enigo.scroll(h.clamp(-20, 20), Axis::Horizontal);
                    }
                }
            }
            InputEvent::Key { code, down } => {
                let Some(key) = key_for_code(&code) else {
                    tracing::trace!("unmapped key code: {code}");
                    return;
                };
                if let Some(enigo) = self.enigo() {
                    let direction = if down {
                        Direction::Press
                    } else {
                        Direction::Release
                    };
                    if let Err(e) = enigo.key(key, direction) {
                        tracing::debug!("key event for {code} failed: {e}");
                    }
                    if down {
                        self.held_keys.insert(code);
                    } else {
                        self.held_keys.remove(&code);
                    }
                }
            }
            InputEvent::Text { text } => {
                let text = sanitize_text(&text);
                if text.is_empty() {
                    return;
                }
                if let Some(enigo) = self.enigo() {
                    if let Err(e) = enigo.text(&text) {
                        tracing::debug!("text injection failed: {e}");
                    }
                }
            }
        }
    }

    fn move_pointer(&mut self, x: i32, y: i32) {
        #[cfg(windows)]
        {
            let ok = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y) };
            if ok == 0 {
                tracing::debug!("SetCursorPos({x},{y}) failed");
            } else {
                let mut actual = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
                let got = unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut actual)
                };
                if got != 0 {
                    tracing::debug!(
                        "pointer -> ({x}, {y}) hardware reports ({}, {})",
                        actual.x,
                        actual.y
                    );
                }
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(enigo) = self.enigo() {
                let _ = enigo.move_mouse(x, y, Coordinate::Abs);
            }
        }
    }

    fn release_all(&mut self) {
        let buttons: Vec<MouseBtn> = self.held_buttons.drain().collect();
        if let Some(enigo) = self.enigo() {
            for button in buttons {
                let _ = enigo.button(button.to_enigo(), Direction::Release);
            }
        }
        let keys: Vec<String> = self.held_keys.drain().collect();
        for code in keys {
            if let Some(key) = key_for_code(&code) {
                if let Some(enigo) = self.enigo() {
                    let _ = enigo.key(key, Direction::Release);
                }
            }
        }
    }
}

/// Map normalized pointer coordinates onto the capture rectangle in physical
/// desktop pixels (the rectangle can have negative coordinates).
pub fn point_on_rect(nx: f64, ny: f64, rect: Rect) -> (i32, i32) {
    let nx = if nx.is_finite() {
        nx.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ny = if ny.is_finite() {
        ny.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let max_x = (rect.width as i32 - 1).max(0);
    let max_y = (rect.height as i32 - 1).max(0);
    let x = rect.x + (nx * rect.width as f64).round() as i32;
    let y = rect.y + (ny * rect.height as f64).round() as i32;
    (
        x.clamp(rect.x, rect.x + max_x),
        y.clamp(rect.y, rect.y + max_y),
    )
}

pub fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .take(8192)
        .collect()
}

/// Convert a `KeyboardEvent.code` into an enigo key.
pub fn key_for_code(code: &str) -> Option<Key> {
    #[cfg(windows)]
    {
        vk_for_code(code).map(Key::Other)
    }
    #[cfg(not(windows))]
    {
        portable_key_for_code(code)
    }
}

#[cfg(windows)]
fn vk_for_code(code: &str) -> Option<u32> {
    let vk = match code {
        // Letters (virtual-key codes match ASCII uppercase).
        "KeyA" => 0x41,
        "KeyB" => 0x42,
        "KeyC" => 0x43,
        "KeyD" => 0x44,
        "KeyE" => 0x45,
        "KeyF" => 0x46,
        "KeyG" => 0x47,
        "KeyH" => 0x48,
        "KeyI" => 0x49,
        "KeyJ" => 0x4A,
        "KeyK" => 0x4B,
        "KeyL" => 0x4C,
        "KeyM" => 0x4D,
        "KeyN" => 0x4E,
        "KeyO" => 0x4F,
        "KeyP" => 0x50,
        "KeyQ" => 0x51,
        "KeyR" => 0x52,
        "KeyS" => 0x53,
        "KeyT" => 0x54,
        "KeyU" => 0x55,
        "KeyV" => 0x56,
        "KeyW" => 0x57,
        "KeyX" => 0x58,
        "KeyY" => 0x59,
        "KeyZ" => 0x5A,
        // Digits row
        "Digit0" => 0x30,
        "Digit1" => 0x31,
        "Digit2" => 0x32,
        "Digit3" => 0x33,
        "Digit4" => 0x34,
        "Digit5" => 0x35,
        "Digit6" => 0x36,
        "Digit7" => 0x37,
        "Digit8" => 0x38,
        "Digit9" => 0x39,
        // Punctuation (OEM keys are layout-mapped by Windows).
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        "IntlBackslash" => 0xE2,
        "IntlRo" => 0xE1,
        "IntlYen" => 0xDC,
        // Control keys
        "Escape" => 0x1B,
        "Tab" => 0x09,
        "CapsLock" => 0x14,
        "Space" => 0x20,
        "Enter" => 0x0D,
        "Backspace" => 0x08,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "ScrollLock" => 0x91,
        "Pause" => 0x13,
        "NumLock" => 0x90,
        "ContextMenu" => 0x5D,
        // Modifiers (side-specific so combos behave)
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        // Numpad
        "Numpad0" => 0x60,
        "Numpad1" => 0x61,
        "Numpad2" => 0x62,
        "Numpad3" => 0x63,
        "Numpad4" => 0x64,
        "Numpad5" => 0x65,
        "Numpad6" => 0x66,
        "Numpad7" => 0x67,
        "Numpad8" => 0x68,
        "Numpad9" => 0x69,
        "NumpadMultiply" => 0x6A,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadDecimal" => 0x6E,
        "NumpadDivide" => 0x6F,
        "NumpadEnter" => 0x0D,
        "NumpadEqual" => 0x92,
        // Media & browser keys
        "MediaPlayPause" => 0xB3,
        "MediaStop" => 0xB2,
        "MediaTrackNext" => 0xB0,
        "MediaTrackPrevious" => 0xB1,
        "AudioVolumeUp" => 0xAF,
        "AudioVolumeDown" => 0xAE,
        "AudioVolumeMute" => 0xAD,
        "BrowserBack" => 0xA6,
        "BrowserForward" => 0xA7,
        "BrowserRefresh" => 0xA8,
        "BrowserStop" => 0xA9,
        "BrowserSearch" => 0xAA,
        "BrowserFavorites" => 0xAB,
        "BrowserHome" => 0xAC,
        "LaunchMail" => 0xB4,
        "LaunchMediaPlayer" => 0xB5,
        "LaunchApp1" => 0xB6,
        "LaunchApp2" => 0xB7,
        "Lang1" => 0x15,
        "Lang2" => 0x19,
        "KanaMode" => 0x15,
        "Convert" => 0x1C,
        "NonConvert" => 0x1D,
        _ => return function_key(code),
    };
    Some(vk)
}

#[cfg(windows)]
fn function_key(code: &str) -> Option<u32> {
    let n: u32 = code.strip_prefix('F')?.parse().ok()?;
    if (1..=24).contains(&n) {
        Some(0x70 + n - 1)
    } else {
        None
    }
}

#[cfg(not(windows))]
fn portable_key_for_code(code: &str) -> Option<Key> {
    let key = match code {
        "Escape" => Key::Escape,
        "Tab" => Key::Tab,
        "CapsLock" => Key::CapsLock,
        "Space" => Key::Space,
        "Enter" => Key::Return,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Insert" => Key::Insert,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowUp" => Key::UpArrow,
        "ArrowRight" => Key::RightArrow,
        "ArrowDown" => Key::DownArrow,
        "ShiftLeft" | "ShiftRight" => Key::Shift,
        "ControlLeft" | "ControlRight" => Key::Control,
        "AltLeft" | "AltRight" => Key::Alt,
        "MetaLeft" | "MetaRight" => Key::Meta,
        "Numpad0" => Key::Numpad0,
        "Numpad1" => Key::Numpad1,
        "Numpad2" => Key::Numpad2,
        "Numpad3" => Key::Numpad3,
        "Numpad4" => Key::Numpad4,
        "Numpad5" => Key::Numpad5,
        "Numpad6" => Key::Numpad6,
        "Numpad7" => Key::Numpad7,
        "Numpad8" => Key::Numpad8,
        "Numpad9" => Key::Numpad9,
        "NumpadEnter" => Key::Return,
        "NumpadAdd" => Key::Add,
        "NumpadSubtract" => Key::Subtract,
        "NumpadMultiply" => Key::Multiply,
        "NumpadDivide" => Key::Divide,
        "NumpadDecimal" => Key::Decimal,
        "MediaPlayPause" => Key::MediaPlayPause,
        "MediaStop" => Key::MediaStop,
        "MediaTrackNext" => Key::MediaNextTrack,
        "MediaTrackPrevious" => Key::MediaPrevTrack,
        "AudioVolumeUp" => Key::VolumeUp,
        "AudioVolumeDown" => Key::VolumeDown,
        "AudioVolumeMute" => Key::VolumeMute,
        other => {
            if let Some(letter) = other.strip_prefix("Key") {
                let c = letter.chars().next()?.to_ascii_lowercase();
                Key::Unicode(c)
            } else if let Some(digit) = other.strip_prefix("Digit") {
                Key::Unicode(digit.chars().next()?)
            } else if let Some(n) = other.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                match n {
                    1 => Key::F1,
                    2 => Key::F2,
                    3 => Key::F3,
                    4 => Key::F4,
                    5 => Key::F5,
                    6 => Key::F6,
                    7 => Key::F7,
                    8 => Key::F8,
                    9 => Key::F9,
                    10 => Key::F10,
                    11 => Key::F11,
                    12 => Key::F12,
                    _ => return None,
                }
            } else {
                return None;
            }
        }
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_maps_into_negative_monitor() {
        let rect = Rect {
            x: -1920,
            y: 3,
            width: 1920,
            height: 1080,
        };
        assert_eq!(point_on_rect(0.0, 0.0, rect), (-1920, 3));
        assert_eq!(point_on_rect(1.0, 1.0, rect), (-1, 1082));
        assert_eq!(point_on_rect(0.5, 0.5, rect), (-960, 543));
    }

    #[cfg(windows)]
    #[test]
    fn windows_keycodes() {
        assert_eq!(vk_for_code("KeyA"), Some(0x41));
        assert_eq!(vk_for_code("ArrowLeft"), Some(0x25));
        assert_eq!(vk_for_code("F12"), Some(0x7B));
        assert_eq!(vk_for_code("Numpad7"), Some(0x67));
        assert_eq!(vk_for_code("ControlRight"), Some(0xA3));
        assert_eq!(vk_for_code("Nope"), None);
    }

    #[test]
    fn sanitizes_control_characters() {
        assert_eq!(sanitize_text("a\u{0}b\nc"), "ab\nc");
    }

    #[test]
    fn parses_input_events() {
        let event: InputEvent = serde_json::from_str(r#"{"t":"move","x":0.1,"y":0.2}"#).unwrap();
        assert!(matches!(event, InputEvent::Move { .. }));
        let event: InputEvent =
            serde_json::from_str(r#"{"t":"down","button":"right","x":0.5,"y":0.5}"#).unwrap();
        assert!(matches!(event, InputEvent::Down { .. }));
        let event: InputEvent =
            serde_json::from_str(r#"{"t":"key","code":"KeyA","down":true}"#).unwrap();
        assert!(matches!(event, InputEvent::Key { down: true, .. }));
    }
}
