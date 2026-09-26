//! Full-desktop selection overlay: shows a frozen, dimmed copy of the screen,
//! the user drags a rectangle, the bright pixels inside it are cropped out.

use std::cell::{Cell, RefCell};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, RoundRect, SelectObject, SetBkMode,
    SetTextColor, Rectangle, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_SEMIBOLD, HDC, HFONT, HGDIOBJ, NULL_BRUSH,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, ReleaseCapture, SetCapture, SetFocus, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_ESCAPE, VK_MENU,
    VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowLongPtrW, LoadCursorW, PostMessageW, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_DBLCLKS, GWLP_USERDATA, HWND_TOPMOST,
    IDC_CROSS, SWP_SHOWWINDOW, SW_HIDE, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_SETFOCUS, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::{self, Image};
use crate::util::{
    get_x, get_y, log, normalize, rect, rect_h, rect_inflate, rect_intersect, rect_union, rect_w, wide,
    DibSection, MemDc,
};

/// Posted to the main window with a Box<CaptureResult> in lparam.
pub const WM_APP_CAPTURED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;

pub struct CaptureResult {
    pub image: Image,
}

const CLASS_NAME: windows::core::PCWSTR = w!("SgCapOverlay");
const MIN_SELECTION: i32 = 3;

struct Shot {
    dc_bright: MemDc,
    dc_dim: MemDc,
    old_bright: HGDIOBJ,
    old_dim: HGDIOBJ,
    bright: DibSection,
    /// Kept alive because it is selected into `dc_dim`.
    #[allow(dead_code)]
    dim: DibSection,
}

impl Drop for Shot {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc_bright.0, self.old_bright);
            SelectObject(self.dc_dim.0, self.old_dim);
        }
    }
}

struct State {
    shot: Option<Shot>,
    origin: POINT,
    size: SIZE,
    dragging: bool,
    anchor: POINT,
    cur: POINT,
    last_mouse: POINT,
    space_down: bool,
    has_focus: bool,
    font: HFONT,
    dpi_scale: f32,
}

pub struct Overlay {
    pub hwnd: HWND,
    main_hwnd: HWND,
    state: RefCell<State>,
    label_rect: Cell<RECT>,
}

impl Overlay {
    /// Creates the (hidden) overlay window once; the returned reference lives for the process.
    pub fn create(hinst: HINSTANCE, main_hwnd: HWND) -> windows::core::Result<&'static Overlay> {
        unsafe {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_DBLCLKS,
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                hCursor: LoadCursorW(None, IDC_CROSS)?,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("SgCap capture"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )?;
            let overlay = Box::leak(Box::new(Overlay {
                hwnd,
                main_hwnd,
                state: RefCell::new(State {
                    shot: None,
                    origin: POINT::default(),
                    size: SIZE::default(),
                    dragging: false,
                    anchor: POINT::default(),
                    cur: POINT::default(),
                    last_mouse: POINT::default(),
                    space_down: false,
                    has_focus: false,
                    font: HFONT::default(),
                    dpi_scale: 1.0,
                }),
                label_rect: Cell::new(RECT::default()),
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, overlay as *const Overlay as isize);
            Ok(overlay)
        }
    }

    pub fn is_active(&self) -> bool {
        self.state
            .try_borrow()
            .map(|s| s.shot.is_some())
            .unwrap_or(true)
    }

    /// Freezes the screen and shows the selection UI.
    pub fn begin(&self) {
        let t0 = std::time::Instant::now();
        let vs = capture::virtual_screen();
        let bright = match capture::grab_fast(vs) {
            Ok(d) => d,
            Err(e) => {
                log(&format!("overlay: grab failed: {e}"));
                return;
            }
        };
        let t_grab = t0.elapsed();
        let dim = match capture::dimmed_copy(&bright) {
            Ok(d) => d,
            Err(e) => {
                log(&format!("overlay: dim failed: {e}"));
                return;
            }
        };
        let t_dim = t0.elapsed();
        unsafe {
            let dc_bright = MemDc::new();
            let dc_dim = MemDc::new();
            let old_bright = SelectObject(dc_bright.0, bright.gdi());
            let old_dim = SelectObject(dc_dim.0, dim.gdi());
            {
                let Ok(mut s) = self.state.try_borrow_mut() else {
                    return;
                };
                if s.shot.is_some() {
                    return;
                }
                s.shot = Some(Shot {
                    dc_bright,
                    dc_dim,
                    old_bright,
                    old_dim,
                    bright,
                    dim,
                });
                s.origin = POINT { x: vs.left, y: vs.top };
                s.size = SIZE {
                    cx: rect_w(&vs),
                    cy: rect_h(&vs),
                };
                s.dragging = false;
                s.space_down = false;
                s.has_focus = false;
                self.label_rect.set(RECT::default());
            }
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                vs.left,
                vs.top,
                rect_w(&vs),
                rect_h(&vs),
                SWP_SHOWWINDOW,
            );
            let dpi = GetDpiForWindow(self.hwnd);
            self.ensure_font(if dpi == 0 { 96 } else { dpi });
            if !SetForegroundWindow(self.hwnd).as_bool() {
                // Classic workaround: a synthetic ALT tap lets us take the foreground.
                keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
                keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
                let _ = SetForegroundWindow(self.hwnd);
            }
            let _ = SetFocus(Some(self.hwnd));
        }
        log(&format!(
            "overlay: {}x{} grab {} ms, dim {} ms, shown at {} ms",
            rect_w(&vs),
            rect_h(&vs),
            t_grab.as_millis(),
            (t_dim - t_grab).as_millis(),
            t0.elapsed().as_millis()
        ));
    }

    fn ensure_font(&self, dpi: u32) {
        let Ok(mut s) = self.state.try_borrow_mut() else {
            return;
        };
        let scale = dpi as f32 / 96.0;
        if !s.font.is_invalid() && (s.dpi_scale - scale).abs() < 0.01 {
            return;
        }
        unsafe {
            if !s.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(s.font.0));
            }
            s.font = CreateFontW(
                -(13.0 * scale).round() as i32,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Segoe UI"),
            );
        }
        s.dpi_scale = scale;
    }

    fn cancel(&self) {
        {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            s.shot = None;
            s.dragging = false;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn finish(&self, sel: RECT) {
        let image = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            let Some(shot) = s.shot.take() else {
                return;
            };
            s.dragging = false;
            capture::crop(&shot.bright, sel)
        };
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let payload = Box::new(CaptureResult { image });
            if PostMessageW(
                Some(self.main_hwnd),
                WM_APP_CAPTURED,
                WPARAM(0),
                LPARAM(Box::into_raw(payload) as isize),
            )
            .is_err()
            {
                log("overlay: PostMessage failed");
            }
        }
    }

    fn selection(s: &State) -> Option<RECT> {
        if !s.dragging {
            return None;
        }
        let r = normalize(s.anchor, s.cur);
        let r = RECT {
            left: r.left.clamp(0, s.size.cx),
            top: r.top.clamp(0, s.size.cy),
            right: r.right.clamp(0, s.size.cx),
            bottom: r.bottom.clamp(0, s.size.cy),
        };
        Some(r)
    }

    /// Region to repaint when the selection changes from `old` to `new`.
    fn invalidate_change(&self, old: Option<RECT>, new: Option<RECT>) {
        let mut r = RECT::default();
        if let Some(o) = old {
            r = rect_union(&r, &rect_inflate(&o, 3, 3));
        }
        if let Some(n) = new {
            r = rect_union(&r, &rect_inflate(&n, 3, 3));
            // generous room for the size label near the bottom-right corner
            let lbl = rect(n.right - 160, n.bottom - 40, n.right + 8, n.bottom + 40);
            r = rect_union(&r, &lbl);
        }
        r = rect_union(&r, &self.label_rect.get());
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), Some(&r), false);
        }
    }

    fn paint(&self, hdc: HDC, clip: RECT) {
        let Ok(s) = self.state.try_borrow() else {
            return;
        };
        let Some(shot) = &s.shot else {
            return;
        };
        unsafe {
            let _ = BitBlt(
                hdc,
                clip.left,
                clip.top,
                rect_w(&clip),
                rect_h(&clip),
                Some(shot.dc_dim.0),
                clip.left,
                clip.top,
                SRCCOPY,
            );
            if let Some(sel) = Self::selection(&s) {
                if let Some(i) = rect_intersect(&sel, &clip) {
                    let _ = BitBlt(
                        hdc,
                        i.left,
                        i.top,
                        rect_w(&i),
                        rect_h(&i),
                        Some(shot.dc_bright.0),
                        i.left,
                        i.top,
                        SRCCOPY,
                    );
                }
                // Frame: dark line outside a white line.
                let null_brush = GetStockObject(NULL_BRUSH);
                let old_brush = SelectObject(hdc, null_brush);
                let dark = CreatePen(PS_SOLID, 1, COLORREF(0x00303030));
                let old_pen = SelectObject(hdc, HGDIOBJ(dark.0));
                let _ = Rectangle(hdc, sel.left - 2, sel.top - 2, sel.right + 2, sel.bottom + 2);
                let white = CreatePen(PS_SOLID, 1, COLORREF(0x00FFFFFF));
                SelectObject(hdc, HGDIOBJ(white.0));
                let _ = Rectangle(hdc, sel.left - 1, sel.top - 1, sel.right + 1, sel.bottom + 1);
                SelectObject(hdc, old_pen);
                SelectObject(hdc, old_brush);
                let _ = DeleteObject(HGDIOBJ(dark.0));
                let _ = DeleteObject(HGDIOBJ(white.0));

                // Size label.
                let text = format!("{} \u{00D7} {}", rect_w(&sel), rect_h(&sel));
                let mut tw = wide(&text);
                let old_font = SelectObject(hdc, HGDIOBJ(s.font.0));
                let mut ext = SIZE::default();
                let _ = GetTextExtentPoint32W(hdc, &tw[..tw.len() - 1], &mut ext);
                let padx = (8.0 * s.dpi_scale) as i32;
                let pady = (4.0 * s.dpi_scale) as i32;
                let bw = ext.cx + 2 * padx;
                let bh = ext.cy + 2 * pady;
                let gap = (6.0 * s.dpi_scale) as i32;
                let mut x = sel.right - bw;
                let mut y = sel.bottom + gap;
                if y + bh > s.size.cy {
                    y = sel.bottom - gap - bh;
                }
                if x < 0 {
                    x = 0;
                }
                let label = rect(x, y, x + bw, y + bh);
                let bg = CreateSolidBrush(COLORREF(0x00282828));
                let old_b2 = SelectObject(hdc, HGDIOBJ(bg.0));
                let old_p2 = SelectObject(hdc, HGDIOBJ(bg.0));
                let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00282828));
                SelectObject(hdc, HGDIOBJ(pen.0));
                let rr = (6.0 * s.dpi_scale) as i32;
                let _ = RoundRect(hdc, label.left, label.top, label.right, label.bottom, rr, rr);
                SelectObject(hdc, old_p2);
                SelectObject(hdc, old_b2);
                let _ = DeleteObject(HGDIOBJ(pen.0));
                let _ = DeleteObject(HGDIOBJ(bg.0));
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, COLORREF(0x00FFFFFF));
                let mut tr = label;
                DrawTextW(hdc, &mut tw[..], &mut tr, DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX);
                SelectObject(hdc, old_font);
                self.label_rect.set(label);
            }
        }
    }

    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_ERASEBKGND => Some(LRESULT(1)),
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(self.hwnd, &mut ps);
                    self.paint(hdc, ps.rcPaint);
                    let _ = EndPaint(self.hwnd, &ps);
                    Some(LRESULT(0))
                }
                WM_LBUTTONDOWN => {
                    let pt = POINT {
                        x: get_x(lparam),
                        y: get_y(lparam),
                    };
                    let (old, new) = {
                        let Ok(mut s) = self.state.try_borrow_mut() else {
                            return Some(LRESULT(0));
                        };
                        if s.shot.is_none() {
                            return Some(LRESULT(0));
                        }
                        let old = Self::selection(&s);
                        s.dragging = true;
                        s.anchor = pt;
                        s.cur = pt;
                        s.last_mouse = pt;
                        (old, Self::selection(&s))
                    };
                    SetCapture(self.hwnd);
                    self.invalidate_change(old, new);
                    Some(LRESULT(0))
                }
                WM_MOUSEMOVE => {
                    let pt = POINT {
                        x: get_x(lparam),
                        y: get_y(lparam),
                    };
                    let change = {
                        let Ok(mut s) = self.state.try_borrow_mut() else {
                            return Some(LRESULT(0));
                        };
                        if s.dragging {
                            let old = Self::selection(&s);
                            if s.space_down {
                                let dx = pt.x - s.last_mouse.x;
                                let dy = pt.y - s.last_mouse.y;
                                s.anchor.x += dx;
                                s.anchor.y += dy;
                                s.cur.x += dx;
                                s.cur.y += dy;
                            } else {
                                s.cur = pt;
                            }
                            s.last_mouse = pt;
                            Some((old, Self::selection(&s)))
                        } else {
                            s.last_mouse = pt;
                            None
                        }
                    };
                    if let Some((old, new)) = change {
                        self.invalidate_change(old, new);
                    }
                    Some(LRESULT(0))
                }
                WM_LBUTTONUP => {
                    let _ = ReleaseCapture();
                    let outcome = {
                        let Ok(mut s) = self.state.try_borrow_mut() else {
                            return Some(LRESULT(0));
                        };
                        if !s.dragging {
                            None
                        } else {
                            let sel = Self::selection(&s);
                            s.dragging = false;
                            sel
                        }
                    };
                    if let Some(sel) = outcome {
                        if rect_w(&sel) >= MIN_SELECTION && rect_h(&sel) >= MIN_SELECTION {
                            self.finish(sel);
                        } else {
                            self.invalidate_change(Some(sel), None);
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_RBUTTONDOWN => {
                    self.cancel();
                    Some(LRESULT(0))
                }
                WM_KEYDOWN => {
                    let vk = wparam.0 as u16;
                    if vk == VK_ESCAPE.0 {
                        self.cancel();
                    } else if vk == VK_SPACE.0 {
                        if let Ok(mut s) = self.state.try_borrow_mut() {
                            s.space_down = true;
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_KEYUP => {
                    if wparam.0 as u16 == VK_SPACE.0 {
                        if let Ok(mut s) = self.state.try_borrow_mut() {
                            s.space_down = false;
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_SETFOCUS => {
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        s.has_focus = true;
                    }
                    Some(LRESULT(0))
                }
                WM_KILLFOCUS => {
                    // Something else took the keyboard while we were up: bail out
                    // instead of leaving a stuck overlay.
                    let should_cancel = self
                        .state
                        .try_borrow()
                        .map(|s| s.has_focus && s.shot.is_some() && !s.dragging)
                        .unwrap_or(false);
                    if should_cancel {
                        self.cancel();
                    }
                    Some(LRESULT(0))
                }
                _ => None,
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Overlay;
    if !ptr.is_null() {
        let overlay = &*ptr;
        if let Some(r) = overlay.handle(msg, wparam, lparam) {
            return r;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
