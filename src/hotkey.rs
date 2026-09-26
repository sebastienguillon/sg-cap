//! Hotkey parsing ("Ctrl+Shift+4"), registration and hotkey-control conversion.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::{HOTKEYF_ALT, HOTKEYF_CONTROL, HOTKEYF_EXT, HOTKEYF_SHIFT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, MapVirtualKeyW, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
    MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

use crate::util::from_wide;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotKey {
    /// MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN bits.
    pub mods: u32,
    pub vk: u32,
}

const NAMED_KEYS: &[(&str, u32)] = &[
    ("PrintScreen", 0x2C),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Space", 0x20),
    ("Tab", 0x09),
    ("Backspace", 0x08),
    ("Enter", 0x0D),
    ("Escape", 0x1B),
    ("Up", 0x26),
    ("Down", 0x28),
    ("Left", 0x25),
    ("Right", 0x27),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    ("NumLock", 0x90),
    ("CapsLock", 0x14),
    ("Apps", 0x5D),
    ("Multiply", 0x6A),
    ("Add", 0x6B),
    ("Subtract", 0x6D),
    ("Decimal", 0x6E),
    ("Divide", 0x6F),
    (";", 0xBA),
    ("=", 0xBB),
    (",", 0xBC),
    ("-", 0xBD),
    (".", 0xBE),
    ("/", 0xBF),
    ("`", 0xC0),
    ("[", 0xDB),
    ("\\", 0xDC),
    ("]", 0xDD),
    ("'", 0xDE),
];

fn name_to_vk(name: &str) -> Option<u32> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    let upper = n.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(num) = upper.strip_prefix('F') {
        if let Ok(v) = num.parse::<u32>() {
            if (1..=24).contains(&v) {
                return Some(0x70 + v - 1);
            }
        }
    }
    if let Some(num) = upper.strip_prefix("NUMPAD") {
        if let Ok(v) = num.parse::<u32>() {
            if v <= 9 {
                return Some(0x60 + v);
            }
        }
    }
    for (k, v) in NAMED_KEYS {
        if k.eq_ignore_ascii_case(n) {
            return Some(*v);
        }
    }
    if let Some(hex) = upper.strip_prefix("0X") {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return Some(v);
        }
    }
    None
}

fn vk_to_name(vk: u32) -> String {
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return (vk as u8 as char).to_string();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    if (0x60..=0x69).contains(&vk) {
        return format!("Numpad{}", vk - 0x60);
    }
    for (k, v) in NAMED_KEYS {
        if *v == vk {
            return k.to_string();
        }
    }
    // Layout-dependent keys: ask Windows for the name.
    unsafe {
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        if sc != 0 {
            let mut buf = [0u16; 64];
            let n = GetKeyNameTextW((sc << 16) as i32, &mut buf);
            if n > 0 {
                return from_wide(&buf);
            }
        }
    }
    format!("0x{vk:02X}")
}

impl HotKey {
    pub fn parse(s: &str) -> Option<HotKey> {
        let mut mods = 0u32;
        let mut vk = None;
        let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" | "ctl" if !last => mods |= MOD_CONTROL.0,
                "shift" if !last => mods |= MOD_SHIFT.0,
                "alt" if !last => mods |= MOD_ALT.0,
                "win" | "windows" | "super" | "meta" if !last => mods |= MOD_WIN.0,
                _ => {
                    if !last {
                        return None;
                    }
                    vk = name_to_vk(part);
                }
            }
        }
        // "Ctrl+Shift++" style input: an empty last part means the plus key.
        vk.map(|vk| HotKey { mods, vk })
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.mods & MOD_CONTROL.0 != 0 {
            s.push_str("Ctrl+");
        }
        if self.mods & MOD_SHIFT.0 != 0 {
            s.push_str("Shift+");
        }
        if self.mods & MOD_ALT.0 != 0 {
            s.push_str("Alt+");
        }
        if self.mods & MOD_WIN.0 != 0 {
            s.push_str("Win+");
        }
        s.push_str(&vk_to_name(self.vk));
        s
    }

    /// From the value returned by HKM_GETHOTKEY.
    pub fn from_control(v: u32) -> Option<HotKey> {
        let vk = v & 0xFF;
        if vk == 0 {
            return None;
        }
        let f = (v >> 8) & 0xFF;
        let mut mods = 0;
        if f & HOTKEYF_SHIFT != 0 {
            mods |= MOD_SHIFT.0;
        }
        if f & HOTKEYF_CONTROL != 0 {
            mods |= MOD_CONTROL.0;
        }
        if f & HOTKEYF_ALT != 0 {
            mods |= MOD_ALT.0;
        }
        Some(HotKey { mods, vk })
    }

    /// Value for HKM_SETHOTKEY.
    pub fn to_control(&self) -> u32 {
        let mut f = 0;
        if self.mods & MOD_SHIFT.0 != 0 {
            f |= HOTKEYF_SHIFT;
        }
        if self.mods & MOD_CONTROL.0 != 0 {
            f |= HOTKEYF_CONTROL;
        }
        if self.mods & MOD_ALT.0 != 0 {
            f |= HOTKEYF_ALT;
        }
        let _ = HOTKEYF_EXT;
        (f << 8) | (self.vk & 0xFF)
    }

    pub fn register(&self, hwnd: HWND, id: i32) -> windows::core::Result<()> {
        unsafe { RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(self.mods) | MOD_NOREPEAT, self.vk) }
    }

    pub fn unregister(hwnd: HWND, id: i32) {
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
    }
}
