//! Notification-area icon and its menu.

use std::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetSystemMetrics,
    PostMessageW, SetForegroundWindow, TrackPopupMenuEx, HICON, ICONINFO, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_NULL,
};

use crate::util::{log, wide, DibSection};

pub const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;

pub const CMD_CAPTURE_REGION: u32 = 201;
pub const CMD_CAPTURE_SCREEN: u32 = 202;
pub const CMD_SETTINGS: u32 = 203;
pub const CMD_OPEN_FOLDER: u32 = 204;
pub const CMD_QUIT: u32 = 205;

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
}

fn copy_str(dst: &mut [u16], s: &str) {
    let w = wide(s);
    let n = w.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&w[..n]);
    dst[n] = 0;
}

/// Draws a simple camera-like icon: dark rounded square with a white lens ring.
fn make_icon(size: i32) -> windows::core::Result<HICON> {
    let s = size.max(16);
    let mut dib = DibSection::new(s, s)?;
    {
        let px = dib.bytes_mut();
        let sf = s as f32;
        let c = sf * 0.5;
        let radius_box = sf * 0.22;
        let ring_outer = sf * 0.30;
        let ring_inner = sf * 0.19;
        let dot = sf * 0.08;
        for y in 0..s {
            for x in 0..s {
                let fx = x as f32 + 0.5;
                let fy = y as f32 + 0.5;
                // rounded square coverage
                let hw = c - radius_box;
                let qx = (fx - c).abs() - hw;
                let qy = (fy - c).abs() - hw;
                let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - radius_box;
                let cover = (0.5 - d).clamp(0.0, 1.0);
                let dist = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
                let ring = ((0.5 - (dist - ring_outer)).clamp(0.0, 1.0)) * ((0.5 + (dist - ring_inner)).clamp(0.0, 1.0));
                let dotc = (0.5 - (dist - dot)).clamp(0.0, 1.0);
                let white = ring.max(dotc);
                // base colour: slate blue-grey
                let (b, g, r) = (0x5Eu32, 0x3Fu32, 0x2Bu32);
                let bb = (b as f32 * (1.0 - white) + 255.0 * white) * cover;
                let gg = (g as f32 * (1.0 - white) + 255.0 * white) * cover;
                let rr = (r as f32 * (1.0 - white) + 255.0 * white) * cover;
                let o = ((y * s + x) * 4) as usize;
                px[o] = bb as u8;
                px[o + 1] = gg as u8;
                px[o + 2] = rr as u8;
                px[o + 3] = (cover * 255.0) as u8;
            }
        }
    }
    unsafe {
        let mask_stride = ((s as usize + 15) / 16) * 2;
        let zeros = vec![0u8; mask_stride * s as usize];
        let mask = CreateBitmap(s, s, 1, 1, Some(zeros.as_ptr() as *const c_void));
        let info = ICONINFO {
            fIcon: windows::core::BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: dib.hbm,
        };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

impl Tray {
    pub fn new(hwnd: HWND) -> windows::core::Result<Tray> {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let icon = make_icon(size)?;
        let tray = Tray { hwnd, icon };
        tray.add()?;
        Ok(tray)
    }

    fn base(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        };
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        nid
    }

    pub fn add(&self) -> windows::core::Result<()> {
        let mut nid = self.base();
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = self.icon;
        copy_str(&mut nid.szTip, "SgCap – screen capture");
        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                return Err(windows::core::Error::from_thread());
            }
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        Ok(())
    }

    pub fn remove(&self) {
        let nid = self.base();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    pub fn balloon(&self, title: &str, text: &str, warning: bool) {
        let mut nid = self.base();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        copy_str(&mut nid.szInfoTitle, title);
        copy_str(&mut nid.szInfo, text);
        unsafe {
            if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                log("tray: balloon failed");
            }
        }
    }

    /// Shows the context menu at (x, y) and returns the chosen command id (0 = none).
    pub fn show_menu(hwnd: HWND, x: i32, y: i32) -> u32 {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return 0;
            };
            let _ = AppendMenuW(menu, MF_STRING, CMD_CAPTURE_REGION as usize, w!("Capture region"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_CAPTURE_SCREEN as usize, w!("Capture screen"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS as usize, w!("Settings..."));
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_FOLDER as usize, w!("Open save folder"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, w!("Quit SgCap"));
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenuEx(
                menu,
                (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_RIGHTALIGN).0,
                x,
                y,
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            cmd.0 as u32
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        unsafe {
            let _ = DestroyIcon(self.icon);
        }
    }
}
