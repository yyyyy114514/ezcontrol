//! Mouse/keyboard injection via SendInput, and clipboard text access.

use std::collections::HashSet;

use serde::Deserialize;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture::Monitor;

#[derive(Deserialize)]
#[serde(tag = "t")]
pub enum Event {
    #[serde(rename = "mm")]
    Move { x: f64, y: f64 },
    #[serde(rename = "md")]
    Down { b: u8, x: f64, y: f64 },
    #[serde(rename = "mu")]
    Up { b: u8, x: f64, y: f64 },
    #[serde(rename = "wh")]
    Wheel { dx: f64, dy: f64 },
    #[serde(rename = "kd")]
    KeyDown { code: String },
    #[serde(rename = "ku")]
    KeyUp { code: String },
    #[serde(rename = "txt")]
    Text { s: String },
    #[serde(rename = "combo")]
    Combo { keys: Vec<String> },
    #[serde(rename = "rel")]
    ReleaseAll,
}

/// Keys and buttons this session holds down, so they can be released on disconnect/blur.
#[derive(Default)]
pub struct Pressed {
    keys: HashSet<String>,
    buttons: HashSet<u8>,
}

impl Pressed {
    pub fn release_all(&mut self) {
        for code in self.keys.drain() {
            key(&code, false);
        }
        for b in self.buttons.drain() {
            send(&[mouse_button(b, false)]);
        }
    }
}

pub fn apply(ev: &Event, mon: &Monitor, p: &mut Pressed) {
    match ev {
        Event::Move { x, y } => send(&[mouse_move(mon, *x, *y)]),
        Event::Down { b, x, y } => {
            p.buttons.insert(*b);
            send(&[mouse_move(mon, *x, *y), mouse_button(*b, true)]);
        }
        Event::Up { b, x, y } => {
            p.buttons.remove(b);
            send(&[mouse_move(mon, *x, *y), mouse_button(*b, false)]);
        }
        Event::Wheel { dx, dy } => {
            let mut v = Vec::new();
            if *dy != 0.0 {
                v.push(mouse_raw(MOUSEEVENTF_WHEEL, 0, 0, (-dy).round() as i32));
            }
            if *dx != 0.0 {
                v.push(mouse_raw(MOUSEEVENTF_HWHEEL, 0, 0, dx.round() as i32));
            }
            send(&v);
        }
        Event::KeyDown { code } => {
            if key(code, true) {
                p.keys.insert(code.clone());
            }
        }
        Event::KeyUp { code } => {
            p.keys.remove(code);
            key(code, false);
        }
        Event::Text { s } => text(s),
        Event::Combo { keys } => {
            for k in keys {
                key(k, true);
            }
            for k in keys.iter().rev() {
                key(k, false);
            }
        }
        Event::ReleaseAll => p.release_all(),
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_raw(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// x, y are 0..1 relative to the monitor; mapped to the 0..65535 virtual-desktop space.
fn mouse_move(m: &Monitor, x: f64, y: f64) -> INPUT {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    let px = m.left as f64 + x.clamp(0.0, 1.0) * (m.w - 1).max(0) as f64;
    let py = m.top as f64 + y.clamp(0.0, 1.0) * (m.h - 1).max(0) as f64;
    let nx = ((px - vx as f64) * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
    let ny = ((py - vy as f64) * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
    mouse_raw(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        nx,
        ny,
        0,
    )
}

fn mouse_button(b: u8, down: bool) -> INPUT {
    let (flags, data) = match (b, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (1, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (3, true) => (MOUSEEVENTF_XDOWN, 1),
        (3, false) => (MOUSEEVENTF_XUP, 1),
        (4, true) => (MOUSEEVENTF_XDOWN, 2),
        (_, _) => (MOUSEEVENTF_XUP, 2),
    };
    mouse_raw(flags, 0, 0, data)
}

enum KeyDef {
    /// set-1 scancode, extended (E0) flag
    Scan(u16, bool),
    Vk(u16),
}

/// KeyboardEvent.code -> key. Scancodes keep it independent of the remote keyboard layout.
fn lookup(code: &str) -> Option<KeyDef> {
    use KeyDef::*;
    let s = |c| Some(Scan(c, false));
    let e = |c| Some(Scan(c, true));
    if let Some(ch) = code.strip_prefix("Key") {
        let c = ch.chars().next()?;
        const ROW: &[(char, u16)] = &[
            ('Q', 0x10), ('W', 0x11), ('E', 0x12), ('R', 0x13), ('T', 0x14), ('Y', 0x15),
            ('U', 0x16), ('I', 0x17), ('O', 0x18), ('P', 0x19), ('A', 0x1E), ('S', 0x1F),
            ('D', 0x20), ('F', 0x21), ('G', 0x22), ('H', 0x23), ('J', 0x24), ('K', 0x25),
            ('L', 0x26), ('Z', 0x2C), ('X', 0x2D), ('C', 0x2E), ('V', 0x2F), ('B', 0x30),
            ('N', 0x31), ('M', 0x32),
        ];
        return ROW.iter().find(|(k, _)| *k == c).map(|(_, v)| Scan(*v, false));
    }
    if let Some(d) = code.strip_prefix("Digit") {
        let n: u16 = d.parse().ok()?;
        return s(if n == 0 { 0x0B } else { 0x01 + n });
    }
    if let Some(f) = code.strip_prefix('F') {
        if let Ok(n) = f.parse::<u16>() {
            return match n {
                1..=10 => s(0x3A + n),
                11 => s(0x57),
                12 => s(0x58),
                13..=24 => Some(Vk(0x7C + n - 13)),
                _ => None,
            };
        }
    }
    match code {
        "Escape" => s(0x01),
        "Minus" => s(0x0C),
        "Equal" => s(0x0D),
        "Backspace" => s(0x0E),
        "Tab" => s(0x0F),
        "BracketLeft" => s(0x1A),
        "BracketRight" => s(0x1B),
        "Enter" => s(0x1C),
        "ControlLeft" => s(0x1D),
        "Semicolon" => s(0x27),
        "Quote" => s(0x28),
        "Backquote" => s(0x29),
        "ShiftLeft" => s(0x2A),
        "Backslash" => s(0x2B),
        "Comma" => s(0x33),
        "Period" => s(0x34),
        "Slash" => s(0x35),
        "ShiftRight" => s(0x36),
        "NumpadMultiply" => s(0x37),
        "AltLeft" => s(0x38),
        "Space" => s(0x39),
        "CapsLock" => s(0x3A),
        "ScrollLock" => s(0x46),
        "Numpad7" => s(0x47),
        "Numpad8" => s(0x48),
        "Numpad9" => s(0x49),
        "NumpadSubtract" => s(0x4A),
        "Numpad4" => s(0x4B),
        "Numpad5" => s(0x4C),
        "Numpad6" => s(0x4D),
        "NumpadAdd" => s(0x4E),
        "Numpad1" => s(0x4F),
        "Numpad2" => s(0x50),
        "Numpad3" => s(0x51),
        "Numpad0" => s(0x52),
        "NumpadDecimal" => s(0x53),
        "IntlBackslash" => s(0x56),
        "NumpadEnter" => e(0x1C),
        "ControlRight" => e(0x1D),
        "NumpadDivide" => e(0x35),
        "PrintScreen" => e(0x37),
        "AltRight" => e(0x38),
        "Home" => e(0x47),
        "ArrowUp" => e(0x48),
        "PageUp" => e(0x49),
        "ArrowLeft" => e(0x4B),
        "ArrowRight" => e(0x4D),
        "End" => e(0x4F),
        "ArrowDown" => e(0x50),
        "PageDown" => e(0x51),
        "Insert" => e(0x52),
        "Delete" => e(0x53),
        "MetaLeft" | "OSLeft" => e(0x5B),
        "MetaRight" | "OSRight" => e(0x5C),
        "ContextMenu" => e(0x5D),
        "NumLock" => Some(Vk(0x90)),
        "Pause" => Some(Vk(0x13)),
        "AudioVolumeMute" => Some(Vk(0xAD)),
        "AudioVolumeDown" => Some(Vk(0xAE)),
        "AudioVolumeUp" => Some(Vk(0xAF)),
        "MediaTrackNext" => Some(Vk(0xB0)),
        "MediaTrackPrevious" => Some(Vk(0xB1)),
        "MediaStop" => Some(Vk(0xB2)),
        "MediaPlayPause" => Some(Vk(0xB3)),
        "Lang1" => Some(Vk(0x15)), // Hangul / Kana
        "Lang2" => Some(Vk(0x19)), // Hanja
        _ => None,
    }
}

fn kbd(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Returns false for unknown codes.
fn key(code: &str, down: bool) -> bool {
    let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    match lookup(code) {
        Some(KeyDef::Scan(sc, ext)) => {
            let mut f = KEYEVENTF_SCANCODE | up;
            if ext {
                f |= KEYEVENTF_EXTENDEDKEY;
            }
            send(&[kbd(0, sc, f)]);
            true
        }
        Some(KeyDef::Vk(vk)) => {
            let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
            let mut f = up;
            if matches!(vk, 0x90 | 0xAD..=0xB3) {
                f |= KEYEVENTF_EXTENDEDKEY;
            }
            send(&[kbd(vk, sc, f)]);
            true
        }
        None => false,
    }
}

/// Types arbitrary Unicode text (works for CJK, emoji) regardless of the remote IME/layout.
fn text(s: &str) {
    let mut v = Vec::new();
    for ch in s.chars() {
        match ch {
            '\r' => continue,
            '\n' => {
                v.push(kbd(0, 0x1C, KEYEVENTF_SCANCODE));
                v.push(kbd(0, 0x1C, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
            }
            '\t' => {
                v.push(kbd(0, 0x0F, KEYEVENTF_SCANCODE));
                v.push(kbd(0, 0x0F, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
            }
            _ => {
                let mut buf = [0u16; 2];
                for u in ch.encode_utf16(&mut buf) {
                    v.push(kbd(0, *u, KEYEVENTF_UNICODE));
                    v.push(kbd(0, *u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
            }
        }
    }
    for chunk in v.chunks(200) {
        send(chunk);
    }
}

// ---------------- clipboard ----------------

fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

pub fn clipboard_get() -> Option<String> {
    if !open_clipboard() {
        return None;
    }
    let r = unsafe {
        (|| {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(g);
            Some(s)
        })()
    };
    unsafe {
        let _ = CloseClipboard();
    }
    r
}

/// Changes whenever any program writes the clipboard.
pub fn clipboard_seq() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Returns the clipboard sequence number after the write.
pub fn clipboard_set(s: &str) -> Option<u32> {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    if !open_clipboard() {
        return None;
    }
    let ok = unsafe {
        (|| {
            EmptyClipboard().ok()?;
            let g = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let p = GlobalLock(g) as *mut u16;
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            let _ = GlobalUnlock(g);
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(g.0))).ok()?;
            Some(())
        })()
        .is_some()
    };
    unsafe {
        let _ = CloseClipboard();
    }
    ok.then(clipboard_seq)
}
