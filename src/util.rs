//! Small helpers: wide strings, rectangles, logging, GDI RAII wrappers.

use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::ptr::null_mut;

use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, RGBQUAD,
};
use windows::Win32::System::SystemInformation::GetLocalTime;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn get_x(lp: LPARAM) -> i32 {
    (lp.0 & 0xFFFF) as u16 as i16 as i32
}

pub fn get_y(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32
}

pub fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

pub fn hiword(v: usize) -> u32 {
    ((v >> 16) & 0xFFFF) as u32
}

pub fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT { left, top, right, bottom }
}

pub fn rect_w(r: &RECT) -> i32 {
    r.right - r.left
}

pub fn rect_h(r: &RECT) -> i32 {
    r.bottom - r.top
}

pub fn rect_is_empty(r: &RECT) -> bool {
    r.right <= r.left || r.bottom <= r.top
}

pub fn rect_union(a: &RECT, b: &RECT) -> RECT {
    if rect_is_empty(a) {
        return *b;
    }
    if rect_is_empty(b) {
        return *a;
    }
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

pub fn rect_intersect(a: &RECT, b: &RECT) -> Option<RECT> {
    let r = RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    if rect_is_empty(&r) {
        None
    } else {
        Some(r)
    }
}

pub fn rect_inflate(r: &RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left - dx,
        top: r.top - dy,
        right: r.right + dx,
        bottom: r.bottom + dy,
    }
}

/// Rectangle spanned by two points (right/bottom exclusive).
pub fn normalize(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

pub fn app_data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("SgCap")
}

fn log_path() -> PathBuf {
    app_data_dir().join("sgcap.log")
}

/// Keeps the log file from growing without bound.
pub fn init_log() {
    let path = log_path();
    let _ = std::fs::create_dir_all(app_data_dir());
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1_000_000 {
            let _ = std::fs::remove_file(&path);
        }
    }
    log("--- sgcap start ---");
}

pub fn log(msg: &str) {
    let path = log_path();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let t = unsafe { GetLocalTime() };
        let _ = writeln!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, msg
        );
    }
}

/// "Screenshot 2026-09-26 at 14.32.11" (macOS-style, 24h clock).
pub fn timestamp_name() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "Screenshot {:04}-{:02}-{:02} at {:02}.{:02}.{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Top-down 32bpp DIB section (BGRA, 4 bytes per pixel, stride = width * 4).
pub struct DibSection {
    pub hbm: HBITMAP,
    bits: *mut u8,
    pub width: i32,
    pub height: i32,
}

impl DibSection {
    pub fn new(width: i32, height: i32) -> windows::core::Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD::default()],
        };
        let mut bits: *mut c_void = null_mut();
        let hbm = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)? };
        Ok(Self {
            hbm,
            bits: bits as *mut u8,
            width,
            height,
        })
    }

    pub fn len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, self.len()) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len()) }
    }

    pub fn gdi(&self) -> HGDIOBJ {
        HGDIOBJ(self.hbm.0)
    }
}

impl Drop for DibSection {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.hbm.0));
        }
    }
}

/// Memory DC that is deleted on drop.
pub struct MemDc(pub HDC);

impl MemDc {
    pub fn new() -> Self {
        MemDc(unsafe { CreateCompatibleDC(None) })
    }
}

impl Drop for MemDc {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// Screen DC that is released on drop.
pub struct ScreenDc(pub HDC);

impl ScreenDc {
    pub fn new() -> Self {
        ScreenDc(unsafe { GetDC(None) })
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(None, self.0);
        }
    }
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}
