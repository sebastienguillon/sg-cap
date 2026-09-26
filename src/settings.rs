//! Settings persisted as a flat JSON file in %APPDATA%\SgCap\settings.json.

use std::ffi::c_void;
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            region_hotkey: "Ctrl+Shift+4".to_string(),
            screen_hotkey: "Ctrl+Shift+3".to_string(),
            thumbnail_seconds: 5,
            save_folder: String::new(),
            copy_to_clipboard: false,
            start_with_windows: false,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        app_data_dir().join("settings.json")
    }

    pub fn load() -> Settings {
        let mut s = Settings::default();
        let text = match std::fs::read_to_string(Self::path()) {
            Ok(t) => t,
            Err(_) => return s,
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
                        _ => {}
                    }
                }
            }
            Err(e) => log(&format!("settings: parse error, using defaults: {e}")),
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let _ = std::fs::create_dir_all(app_data_dir());
        let text = format!(
            "{{\n  \"region_hotkey\": \"{}\",\n  \"screen_hotkey\": \"{}\",\n  \"thumbnail_seconds\": {},\n  \"save_folder\": \"{}\",\n  \"copy_to_clipboard\": {},\n  \"start_with_windows\": {}\n}}\n",
            json_escape(&self.region_hotkey),
            json_escape(&self.screen_hotkey),
            self.thumbnail_seconds,
            json_escape(&self.save_folder),
            self.copy_to_clipboard,
            self.start_with_windows
        );
        std::fs::write(Self::path(), text)
    }

    pub fn resolved_save_folder(&self) -> PathBuf {
        if self.save_folder.trim().is_empty() {
            desktop_dir()
        } else {
            PathBuf::from(self.save_folder.trim())
        }
    }
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
