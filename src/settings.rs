//! Settings persisted as a flat JSON file in %APPDATA%\SgCap\settings.json.

use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Desktop, KF_FLAG_DEFAULT};

use crate::util::{app_data_dir, log};

#[derive(Clone, Debug)]
pub struct Settings {
    pub region_hotkey: String,
    pub screen_hotkey: String,
    pub thumbnail_seconds: u32,
    /// Empty means "Desktop".
    pub save_folder: String,
    pub copy_to_clipboard: bool,
    pub start_with_windows: bool,
    /// Last annotation tool used in the editor ("line", "arrow", "rect", "ellipse").
    pub annotate_tool: String,
    /// Last annotation colour, 0xRRGGBB.
    pub annotate_color: u32,
    /// Last annotation stroke width, in points (scaled by the capture DPI).
    pub annotate_width: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            region_hotkey: "Ctrl+Shift+4".to_string(),
            screen_hotkey: "Ctrl+Shift+3".to_string(),
            thumbnail_seconds: 12,
            save_folder: String::new(),
            copy_to_clipboard: false,
            start_with_windows: false,
            annotate_tool: "arrow".to_string(),
            annotate_color: 0xFF3B30,
            annotate_width: 4,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        app_data_dir().join("settings.json")
    }

    pub fn load() -> Settings {
        Self::try_load().unwrap_or_else(|e| {
            log(&format!("settings: {e}, using defaults"));
            Settings::default()
        })
    }

    /// Defaults when there is no file yet; an error when it exists but cannot
    /// be read or parsed (so callers do not overwrite it with defaults).
    pub fn try_load() -> Result<Settings, String> {
        let mut s = Settings::default();
        let text = match std::fs::read_to_string(Self::path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(s),
            Err(e) => return Err(format!("cannot read {}: {e}", Self::path().display())),
        };
        match parse_flat_json(&text) {
            Ok(pairs) => {
                for (k, v) in pairs {
                    match (k.as_str(), v) {
                        ("region_hotkey", JsonVal::Str(v)) => s.region_hotkey = v,
                        ("screen_hotkey", JsonVal::Str(v)) => s.screen_hotkey = v,
                        ("thumbnail_seconds", JsonVal::Num(v)) => {
                            s.thumbnail_seconds = (v.round() as i64).clamp(1, 120) as u32
                        }
                        ("save_folder", JsonVal::Str(v)) => s.save_folder = v,
                        ("copy_to_clipboard", JsonVal::Bool(v)) => s.copy_to_clipboard = v,
                        ("start_with_windows", JsonVal::Bool(v)) => s.start_with_windows = v,
                        ("annotate_tool", JsonVal::Str(v)) => s.annotate_tool = v,
                        ("annotate_color", JsonVal::Str(v)) => {
                            if let Some(c) = parse_hex_color(&v) {
                                s.annotate_color = c;
                            }
                        }
                        ("annotate_width", JsonVal::Num(v)) => {
                            s.annotate_width = (v.round() as i64).clamp(1, 50) as u32
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => return Err(format!("parse error: {e}")),
        }
        Ok(s)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let _ = std::fs::create_dir_all(app_data_dir());
        let text = format!(
            "{{\n  \"region_hotkey\": \"{}\",\n  \"screen_hotkey\": \"{}\",\n  \"thumbnail_seconds\": {},\n  \"save_folder\": \"{}\",\n  \"copy_to_clipboard\": {},\n  \"start_with_windows\": {},\n  \"annotate_tool\": \"{}\",\n  \"annotate_color\": \"#{:06X}\",\n  \"annotate_width\": {}\n}}\n",
            json_escape(&self.region_hotkey),
            json_escape(&self.screen_hotkey),
            self.thumbnail_seconds,
            json_escape(&self.save_folder),
            self.copy_to_clipboard,
            self.start_with_windows,
            json_escape(&self.annotate_tool),
            self.annotate_color & 0xFF_FFFF,
            self.annotate_width
        );
        // Written next to it and flushed, then renamed over it: never a
        // half-written or empty file, even after a power cut.
        let tmp = Self::path().with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, Self::path())
    }

    pub fn resolved_save_folder(&self) -> PathBuf {
        if self.save_folder.trim().is_empty() {
            desktop_dir()
        } else {
            PathBuf::from(self.save_folder.trim())
        }
    }
}

/// "#RRGGBB" (or "RRGGBB") to 0xRRGGBB.
fn parse_hex_color(s: &str) -> Option<u32> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok()
}

pub fn desktop_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const c_void));
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Desktop")
}

#[derive(Debug)]
pub enum JsonVal {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Minimal parser for a flat JSON object of strings, numbers, booleans and null.
pub fn parse_flat_json(text: &str) -> Result<Vec<(String, JsonVal)>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();

    fn skip_ws(chars: &[char], i: &mut usize) {
        while *i < chars.len() && chars[*i].is_whitespace() {
            *i += 1;
        }
    }

    fn parse_string(chars: &[char], i: &mut usize) -> Result<String, String> {
        if chars.get(*i) != Some(&'"') {
            return Err(format!("expected string at {}", *i));
        }
        *i += 1;
        let mut s = String::new();
        while *i < chars.len() {
            let c = chars[*i];
            *i += 1;
            match c {
                '"' => return Ok(s),
                '\\' => {
                    let e = *chars.get(*i).ok_or("bad escape")?;
                    *i += 1;
                    match e {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        'u' => {
                            if *i + 4 > chars.len() {
                                return Err("bad unicode escape".into());
                            }
                            let hex: String = chars[*i..*i + 4].iter().collect();
                            *i += 4;
                            let v = u32::from_str_radix(&hex, 16).map_err(|e| e.to_string())?;
                            s.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                c => s.push(c),
            }
        }
        Err("unterminated string".into())
    }

    skip_ws(&chars, &mut i);
    if chars.get(i) != Some(&'{') {
        return Err("expected '{'".into());
    }
    i += 1;
    loop {
        skip_ws(&chars, &mut i);
        match chars.get(i) {
            Some('}') => break,
            Some(',') => {
                i += 1;
                continue;
            }
            Some('"') => {}
            _ => return Err(format!("unexpected token at {i}")),
        }
        let key = parse_string(&chars, &mut i)?;
        skip_ws(&chars, &mut i);
        if chars.get(i) != Some(&':') {
            return Err("expected ':'".into());
        }
        i += 1;
        skip_ws(&chars, &mut i);
        let val = match chars.get(i) {
            Some('"') => JsonVal::Str(parse_string(&chars, &mut i)?),
            Some('t') if chars[i..].starts_with(&['t', 'r', 'u', 'e']) => {
                i += 4;
                JsonVal::Bool(true)
            }
            Some('f') if chars[i..].starts_with(&['f', 'a', 'l', 's', 'e']) => {
                i += 5;
                JsonVal::Bool(false)
            }
            Some('n') if chars[i..].starts_with(&['n', 'u', 'l', 'l']) => {
                i += 4;
                JsonVal::Null
            }
            Some(c) if c.is_ascii_digit() || *c == '-' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_digit() || matches!(chars[i], '-' | '+' | '.' | 'e' | 'E'))
                {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                JsonVal::Num(s.parse::<f64>().map_err(|e| e.to_string())?)
            }
            _ => return Err(format!("unexpected value at {i}")),
        };
        out.push((key, val));
    }
    Ok(out)
}
