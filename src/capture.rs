//! Screen grabbing (GDI), dimming, cropping and PNG encoding.

use std::io::Cursor;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, GdiFlush, GetMonitorInfoW, MonitorFromPoint, SelectObject, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use crate::util::{rect_h, rect_w, DibSection, MemDc, ScreenDc};

/// A captured image: top-down BGRA rows, alpha byte ignored.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Brightness multiplier (out of 256) for the area outside the selection.
const DIM_FACTOR: u32 = 140;

pub fn virtual_screen() -> RECT {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }
}

pub struct MonitorInfo {
    pub handle: HMONITOR,
    pub rect: RECT,
    pub work: RECT,
}

pub fn monitor_at(pt: POINT) -> MonitorInfo {
    unsafe {
        let handle = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(handle, &mut mi);
        MonitorInfo {
            handle,
            rect: mi.rcMonitor,
            work: mi.rcWork,
        }
    }
}

pub fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

/// Grabs a rectangle: DXGI desktop duplication when available, GDI otherwise.
pub fn grab_fast(rect: RECT) -> windows::core::Result<DibSection> {
    if let Some(dib) = crate::dxgi_capture::grab(rect) {
        return Ok(dib);
    }
    grab(rect)
}

/// Copies a screen rectangle (virtual-screen coordinates) into a DIB.
pub fn grab(rect: RECT) -> windows::core::Result<DibSection> {
    let w = rect_w(&rect);
    let h = rect_h(&rect);
    let dib = DibSection::new(w, h)?;
    let screen = ScreenDc::new();
    let mem = MemDc::new();
    unsafe {
        let old = SelectObject(mem.0, dib.gdi());
        let res = BitBlt(mem.0, 0, 0, w, h, Some(screen.0), rect.left, rect.top, SRCCOPY);
        let _ = GdiFlush();
        SelectObject(mem.0, old);
        res?;
    }
    Ok(dib)
}

/// Builds a dimmed copy of a DIB, split across the available cores.
pub fn dimmed_copy(src: &DibSection) -> windows::core::Result<DibSection> {
    let mut dst = DibSection::new(src.width, src.height)?;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 8);
    let src_bytes = src.bytes();
    let dst_bytes = dst.bytes_mut();
    let total = src_bytes.len();
    let chunk = ((total / threads) + 3) & !3;
    let chunk = chunk.max(4);
    std::thread::scope(|s| {
        for (sc, dc) in src_bytes.chunks(chunk).zip(dst_bytes.chunks_mut(chunk)) {
            s.spawn(move || {
                for (d, v) in dc.iter_mut().zip(sc.iter()) {
                    *d = ((*v as u32 * DIM_FACTOR) >> 8) as u8;
                }
            });
        }
    });
    Ok(dst)
}

/// Extracts a rectangle (DIB-local coordinates, right/bottom exclusive).
pub fn crop(src: &DibSection, r: RECT) -> Image {
    let left = r.left.clamp(0, src.width);
    let top = r.top.clamp(0, src.height);
    let right = r.right.clamp(left, src.width);
    let bottom = r.bottom.clamp(top, src.height);
    let w = (right - left) as usize;
    let h = (bottom - top) as usize;
    let stride = src.stride();
    let bytes = src.bytes();
    let mut out = Vec::with_capacity(w * h * 4);
    for y in top as usize..bottom as usize {
        let start = y * stride + left as usize * 4;
        out.extend_from_slice(&bytes[start..start + w * 4]);
    }
    Image {
        width: w as u32,
        height: h as u32,
        bgra: out,
    }
}

pub fn whole(src: &DibSection) -> Image {
    Image {
        width: src.width as u32,
        height: src.height as u32,
        bgra: src.bytes().to_vec(),
    }
}

/// Debug helper: encodes a BGRA buffer (alpha kept) as RGBA PNG.
pub fn encode_png_rgba(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    let mut out = Cursor::new(Vec::new());
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out.into_inner())
}

/// Encodes to PNG (RGB, fast compression: this runs on a worker thread but
/// the file must exist quickly for drag-and-drop).
pub fn encode_png(img: &Image) -> Result<Vec<u8>, String> {
    let n = img.width as usize * img.height as usize;
    let mut rgb = Vec::with_capacity(n * 3);
    for px in img.bgra.chunks_exact(4) {
        rgb.push(px[2]);
        rgb.push(px[1]);
        rgb.push(px[0]);
    }
    let mut out = Cursor::new(Vec::with_capacity(n / 2 + 1024));
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out.into_inner())
}
