//! Floating thumbnail window: slides in at the bottom-right of the monitor,
//! can be dragged into other applications, fades out after a delay.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, HGDIOBJ};
use windows::Win32::System::Ole::DROPEFFECT_NONE;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{
    SHFileOperationW, ShellExecuteW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
    SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GetCursorPos,
    GetSystemMetrics, GetWindowLongPtrW, KillTimer, PostMessageW, RegisterClassExW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, ShowWindow, TrackPopupMenuEx, UpdateLayeredWindow,
    GWLP_USERDATA, MA_NOACTIVATE, MF_SEPARATOR, MF_STRING, SM_CXDRAG, SM_CYDRAG, SW_SHOWNOACTIVATE,
    SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WM_APP, WM_DESTROY,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::Image;
use crate::dragdrop;
use crate::thumb_image::ThumbImage;
use crate::util::{ease_in_cubic, ease_out_cubic, get_x, get_y, log, wide, DibSection, MemDc};

pub const WM_APP_THUMB_CLOSED: u32 = WM_APP + 4;
/// lparam = new bottom edge (screen y) of the thumbnail content.
pub const WM_APP_THUMB_MOVE: u32 = WM_APP + 6;

pub type FileResult = Result<(PathBuf, Arc<Vec<u8>>), String>;

pub struct ThumbParams {
    pub id: u64,
    pub image: Arc<Image>,
    pub thumb: ThumbImage,
    pub work: RECT,
    pub dpi: u32,
    pub seconds: u32,
    pub file_rx: Receiver<FileResult>,
}

const CLASS_NAME: PCWSTR = w!("SgCapThumb");
const TIMER_ANIM: usize = 1;
const TIMER_COUNTDOWN: usize = 2;
const ANIM_INTERVAL_MS: u32 = 15;
const SLIDE_IN_MS: f32 = 220.0;
const SLIDE_OUT_MS: f32 = 260.0;
const SLIDE_DIST: f32 = 40.0;

const CMD_COPY: u32 = 301;
const CMD_SHOW: u32 = 302;
const CMD_DELETE: u32 = 303;
const CMD_DISMISS: u32 = 304;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    SlideIn(Instant),
    Idle,
    SlideOut(Instant),
}

struct State {
    #[allow(dead_code)]
    id: u64,
    image: Arc<Image>,
    thumb: ThumbImage,
    dc: MemDc,
    old: HGDIOBJ,
    /// Kept alive because it is selected into `dc`.
    #[allow(dead_code)]
    dib: DibSection,
    scale: f32,
    x: i32,
    target_y: i32,
    cur_x: f32,
    cur_y: f32,
    alpha: f32,
    last_presented: (i32, i32, u8),
    phase: Phase,
    hovered: bool,
    pressed: Option<POINT>,
    in_drag: bool,
    menu_open: bool,
    countdown_ms: u32,
    file: Option<(PathBuf, Arc<Vec<u8>>)>,
    file_err: Option<String>,
    rx: Option<Receiver<FileResult>>,
    main_hwnd: HWND,
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc.0, self.old);
        }
    }
}

pub struct Thumbnail {
    hwnd: HWND,
    state: RefCell<State>,
}

fn present(hwnd: HWND, s: &mut State) {
    let x = s.cur_x.round() as i32;
    let y = s.cur_y.round() as i32;
    let a = (s.alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    if s.last_presented == (x, y, a) {
        return;
    }
    s.last_presented = (x, y, a);
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: a,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let dst = POINT { x, y };
    let size = SIZE {
        cx: s.thumb.w,
        cy: s.thumb.h,
    };
    let src = POINT { x: 0, y: 0 };
    unsafe {
        if let Err(e) = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&dst),
            Some(&size),
            Some(s.dc.0),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            log(&format!("thumb: UpdateLayeredWindow failed: {e}"));
        }
    }
}

fn begin_slide_out(hwnd: HWND, s: &mut State) {
    if matches!(s.phase, Phase::SlideOut(_)) {
        return;
    }
    s.phase = Phase::SlideOut(Instant::now());
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
    }
}

fn start_countdown(hwnd: HWND, s: &State) {
    if s.hovered || s.in_drag || s.menu_open || s.phase != Phase::Idle {
        return;
    }
    unsafe {
        SetTimer(Some(hwnd), TIMER_COUNTDOWN, s.countdown_ms.max(500), None);
    }
}

fn ensure_file(s: &mut State) -> Option<(PathBuf, Arc<Vec<u8>>)> {
    if s.file.is_none() && s.file_err.is_none() {
        if let Some(rx) = s.rx.take() {
            match rx.recv() {
                Ok(Ok(v)) => s.file = Some(v),
                Ok(Err(e)) => {
                    log(&format!("thumb: save failed: {e}"));
                    s.file_err = Some(e);
                }
                Err(_) => s.file_err = Some("worker thread ended".to_string()),
            }
        }
    }
    s.file.clone()
}

fn shell_open(file: &str, params: Option<&str>) {
    let f = wide(file);
    let p = params.map(wide);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(f.as_ptr()),
            p.as_ref().map(|v| PCWSTR(v.as_ptr())).unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn recycle(path: &PathBuf) {
    let mut from = wide(&path.to_string_lossy());
    from.push(0); // double NUL terminated list
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        fAnyOperationsAborted: windows::core::BOOL(0),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };
    unsafe {
        let r = SHFileOperationW(&mut op);
        if r != 0 {
            log(&format!("thumb: delete failed ({r}), removing directly"));
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Thumbnail {
    fn tick(&self) {
        let mut destroy = false;
        let mut countdown = false;
        {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            let slide = SLIDE_DIST * s.scale;
            match s.phase {
                Phase::SlideIn(t0) => {
                    let t = t0.elapsed().as_secs_f32() * 1000.0 / SLIDE_IN_MS;
                    let e = ease_out_cubic(t);
                    s.cur_x = s.x as f32 + slide * (1.0 - e);
                    s.cur_y = s.target_y as f32;
                    s.alpha = e;
                    if t >= 1.0 {
                        s.phase = Phase::Idle;
                        countdown = true;
                    }
                }
                Phase::SlideOut(t0) => {
                    let t = t0.elapsed().as_secs_f32() * 1000.0 / SLIDE_OUT_MS;
                    let e = ease_in_cubic(t);
                    s.cur_x = s.x as f32 + slide * e;
                    s.alpha = 1.0 - e;
                    if t >= 1.0 {
                        destroy = true;
                    }
                }
                Phase::Idle => {
                    let dy = s.target_y as f32 - s.cur_y;
                    if dy.abs() > 0.5 {
                        s.cur_y += dy * 0.3;
                    } else {
                        s.cur_y = s.target_y as f32;
                    }
                    s.cur_x = s.x as f32;
                    s.alpha = 1.0;
                }
            }
            let hwnd = self.hwnd;
            present(hwnd, &mut s);
            if countdown {
                start_countdown(hwnd, &s);
            }
        }
        if destroy {
            let hwnd = self.hwnd;
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    fn start_drag(&self, pt: POINT) {
        let hwnd = self.hwnd;
        let prepared = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            s.pressed = None;
            s.in_drag = true;
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
            }
            match ensure_file(&mut s) {
                Some((path, png)) => Some((path, png, s.image.clone(), s.thumb.clone())),
                None => {
                    s.in_drag = false;
                    None
                }
            }
        };
        unsafe {
            let _ = ReleaseCapture();
        }
        let Some((path, png, image, thumb)) = prepared else {
            return;
        };
        let result = dragdrop::start_drag(hwnd, &path, &image, Some(&png), &thumb, pt);
        let Ok(mut s) = self.state.try_borrow_mut() else {
            return;
        };
        s.in_drag = false;
        match result {
            Ok(effect) if effect != DROPEFFECT_NONE => begin_slide_out(hwnd, &mut s),
            Ok(_) => start_countdown(hwnd, &s),
            Err(e) => {
                log(&format!("thumb: drag failed: {e}"));
                start_countdown(hwnd, &s);
            }
        }
    }

    fn context_menu(&self) {
        let hwnd = self.hwnd;
        {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            if s.in_drag || matches!(s.phase, Phase::SlideOut(_)) {
                return;
            }
            s.menu_open = true;
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
            }
        }
        let cmd = unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let _ = AppendMenuW(menu, MF_STRING, CMD_COPY as usize, w!("Copy image"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_SHOW as usize, w!("Show in folder"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_DELETE as usize, w!("Delete"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_DISMISS as usize, w!("Dismiss"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(hwnd);
            let r = TrackPopupMenuEx(menu, (TPM_RIGHTBUTTON | TPM_RETURNCMD).0, pt.x, pt.y, hwnd, None);
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            r.0 as u32
        };
        let Ok(mut s) = self.state.try_borrow_mut() else {
            return;
        };
        s.menu_open = false;
        match cmd {
            CMD_COPY => {
                let png = ensure_file(&mut s).map(|(_, p)| p);
                if let Err(e) = dragdrop::copy_to_clipboard(hwnd, &s.image, png.as_deref().map(|v| v.as_slice())) {
                    log(&format!("thumb: clipboard failed: {e}"));
                }
                begin_slide_out(hwnd, &mut s);
            }
            CMD_SHOW => {
                if let Some((path, _)) = ensure_file(&mut s) {
                    shell_open("explorer.exe", Some(&format!("/select,\"{}\"", path.display())));
                }
                begin_slide_out(hwnd, &mut s);
            }
            CMD_DELETE => {
                if let Some((path, _)) = ensure_file(&mut s) {
                    recycle(&path);
                }
                begin_slide_out(hwnd, &mut s);
            }
            CMD_DISMISS => begin_slide_out(hwnd, &mut s),
            _ => start_countdown(hwnd, &s),
        }
    }

    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let hwnd = self.hwnd;
        unsafe {
            match msg {
                WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
                WM_TIMER => {
                    match wparam.0 {
                        TIMER_ANIM => self.tick(),
                        TIMER_COUNTDOWN => {
                            let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
                            if let Ok(mut s) = self.state.try_borrow_mut() {
                                if s.phase == Phase::Idle && !s.hovered && !s.in_drag && !s.menu_open {
                                    begin_slide_out(hwnd, &mut s);
                                }
                            }
                        }
                        _ => {}
                    }
                    Some(LRESULT(0))
                }
                WM_MOUSEMOVE => {
                    let pt = POINT {
                        x: get_x(lparam),
                        y: get_y(lparam),
                    };
                    let mut drag_from = None;
                    {
                        let Ok(mut s) = self.state.try_borrow_mut() else {
                            return Some(LRESULT(0));
                        };
                        if s.in_drag {
                            return Some(LRESULT(0));
                        }
                        if !s.hovered {
                            s.hovered = true;
                            let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
                            let mut tme = TRACKMOUSEEVENT {
                                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: hwnd,
                                dwHoverTime: 0,
                            };
                            let _ = TrackMouseEvent(&mut tme);
                        }
                        if let Some(p0) = s.pressed {
                            let dx = GetSystemMetrics(SM_CXDRAG).max(4);
                            let dy = GetSystemMetrics(SM_CYDRAG).max(4);
                            if (pt.x - p0.x).abs() >= dx || (pt.y - p0.y).abs() >= dy {
                                drag_from = Some(p0);
                            }
                        }
                    }
                    if let Some(p0) = drag_from {
                        self.start_drag(p0);
                    }
                    Some(LRESULT(0))
                }
                WM_MOUSELEAVE => {
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        s.hovered = false;
                        if !s.in_drag {
                            s.pressed = None;
                            start_countdown(hwnd, &s);
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_LBUTTONDOWN => {
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        if !s.in_drag && !matches!(s.phase, Phase::SlideOut(_)) {
                            s.pressed = Some(POINT {
                                x: get_x(lparam),
                                y: get_y(lparam),
                            });
                            SetCapture(hwnd);
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_LBUTTONUP => {
                    let _ = ReleaseCapture();
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        if s.pressed.take().is_some() && !s.in_drag {
                            // Click: open the file in the default viewer, then go away.
                            if let Some((path, _)) = ensure_file(&mut s) {
                                shell_open(&path.to_string_lossy(), None);
                            }
                            begin_slide_out(hwnd, &mut s);
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_RBUTTONUP => {
                    self.context_menu();
                    Some(LRESULT(0))
                }
                WM_APP_THUMB_MOVE => {
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        let bottom = lparam.0 as i32;
                        s.target_y = bottom - s.thumb.content_h - s.thumb.pad;
                    }
                    Some(LRESULT(0))
                }
                WM_DESTROY => {
                    let _ = KillTimer(Some(hwnd), TIMER_ANIM);
                    let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
                    let main = self.state.try_borrow().map(|s| s.main_hwnd).unwrap_or_default();
                    let _ = PostMessageW(Some(main), WM_APP_THUMB_CLOSED, WPARAM(hwnd.0 as usize), LPARAM(0));
                    Some(LRESULT(0))
                }
                _ => None,
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Thumbnail;
    if msg == WM_NCDESTROY {
        if !ptr.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    if !ptr.is_null() {
        let t = &*ptr;
        if let Some(r) = t.handle(msg, wparam, lparam) {
            return r;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn register_class(hinst: HINSTANCE) {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

/// Creates and shows a thumbnail whose content bottom edge sits at `bottom` (screen y).
pub fn create(hinst: HINSTANCE, main_hwnd: HWND, p: ThumbParams, bottom: i32) -> windows::core::Result<HWND> {
    register_class(hinst);
    let scale = p.dpi as f32 / 96.0;
    let margin = (16.0 * scale).round() as i32;
    let x = p.work.right - margin - p.thumb.content_w - p.thumb.pad;
    let y = bottom - p.thumb.content_h - p.thumb.pad;
    let mut dib = DibSection::new(p.thumb.w, p.thumb.h)?;
    dib.bytes_mut().copy_from_slice(&p.thumb.bgra);
    let dc = MemDc::new();
    let old = unsafe { SelectObject(dc.0, dib.gdi()) };
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CLASS_NAME,
            w!("SgCap thumbnail"),
            WS_POPUP,
            x,
            y,
            p.thumb.w,
            p.thumb.h,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let state = State {
            id: p.id,
            image: p.image,
            countdown_ms: p.seconds.saturating_mul(1000),
            thumb: p.thumb,
            dc,
            old,
            dib,
            scale,
            x,
            target_y: y,
            cur_x: x as f32 + SLIDE_DIST * scale,
            cur_y: y as f32,
            alpha: 0.0,
            last_presented: (i32::MIN, i32::MIN, 0),
            phase: Phase::SlideIn(Instant::now()),
            hovered: false,
            pressed: None,
            in_drag: false,
            menu_open: false,
            file: None,
            file_err: None,
            rx: Some(p.file_rx),
            main_hwnd,
        };
        let t = Box::into_raw(Box::new(Thumbnail {
            hwnd,
            state: RefCell::new(state),
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, t as isize);
        {
            let mut s = (*t).state.borrow_mut();
            present(hwnd, &mut s);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), TIMER_ANIM, ANIM_INTERVAL_MS, None);
        Ok(hwnd)
    }
}
