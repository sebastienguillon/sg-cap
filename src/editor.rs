//! Annotation editor in the spirit of macOS Markup: the capture with a toolbar
//! to draw lines, arrows, rectangles, ellipses, pen and highlighter strokes,
//! text and pixelated areas (width and colour), select, move, reshape and
//! restyle them, with undo/redo. Done renders the shapes into the image;
//! main.rs writes it over the PNG and slides in a fresh thumbnail.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    COLORREF, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ScreenToClient, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::Controls::{
    TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
    WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VIRTUAL_KEY, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_MENU, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCaretBlinkTime, GetClientRect, GetCursorPos,
    GetSystemMetrics, GetWindowLongPtrW, KillTimer, LoadCursorW, MessageBoxW, PostMessageW, RegisterClassExW,
    SendMessageW, SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW,
    ShowWindow, CS_DBLCLKS, CW_USEDEFAULT, GWLP_USERDATA, HTCLIENT, IDC_ARROW, IDC_CROSS, IDC_IBEAM, IDC_SIZEALL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDNO, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION,
    MB_ICONWARNING, MB_OK, MB_YESNO, MB_YESNOCANCEL, MINMAXINFO, SIZE_MINIMIZED, SM_CXDRAG, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE,
    WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_SETCURSOR, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

use crate::capture::{monitor_at, monitor_dpi, Image};
use crate::d2d::{self, Canvas, Fonts, RectF, Target, TextBox, HIGHLIGHTER_ALPHA};
use crate::dragdrop;
use crate::shapes::{Doc, Handle, Kind, Mosaic, Pt, Shape, Style};
use crate::thumbnail::FileResult;
use crate::util::{from_wide, get_x, get_y, hiword, log, loword, rect_h, rect_w, wide};

/// Thumbnail -> main window: lparam = Box<EditRequest>.
pub const WM_APP_EDIT_REQUEST: u32 = WM_APP + 8;
/// Editor -> main window: lparam = Box<EditorOutcome>.
pub const WM_APP_EDITOR_CLOSED: u32 = WM_APP + 9;
/// Editor -> itself: destroy the window outside of any handler.
const WM_APP_EDITOR_DESTROY: u32 = WM_APP + 10;
/// Main window -> editor (SendMessage, session ending): returns a
/// Box<Snapshot> pointer when there are unsaved annotations, else 0.
pub const WM_APP_EDITOR_SNAPSHOT: u32 = WM_APP + 11;
/// Main window -> editor (SendMessage): 1 if it holds unsaved annotations.
pub const WM_APP_EDITOR_IS_DIRTY: u32 = WM_APP + 14;

/// The capture's PNG: already known, or still being written by the capture's
/// worker thread (the editor opens without waiting for it).
pub enum PendingFile {
    Ready(Option<(PathBuf, Arc<Vec<u8>>)>),
    Saving(Receiver<FileResult>),
}

impl PendingFile {
    fn settle(&mut self, r: Result<FileResult, ()>) {
        *self = PendingFile::Ready(match r {
            Ok(Ok(f)) => Some(f),
            Ok(Err(e)) => {
                log(&format!("editor: the capture was not saved: {e}"));
                None
            }
            Err(()) => None,
        });
    }

    /// Non-blocking; true once the save has finished (or failed).
    fn poll(&mut self) -> bool {
        if let PendingFile::Saving(rx) = self {
            match rx.try_recv() {
                Ok(r) => self.settle(Ok(r)),
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => self.settle(Err(())),
            }
        }
        true
    }

    fn path(&self) -> Option<&PathBuf> {
        match self {
            PendingFile::Ready(Some((p, _))) => Some(p),
            _ => None,
        }
    }
}

pub struct EditRequest {
    pub image: Arc<Image>,
    pub file: PendingFile,
    /// DPI scale of the monitor the capture was taken on: widths scale with it.
    pub capture_scale: f32,
    /// Screen point the editor opens near (the thumbnail's centre).
    pub anchor: POINT,
}

/// The editor's current tool and style, remembered in the settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prefs {
    pub tool: Kind,
    pub color: u32,
    pub width_pt: u32,
}

pub enum Outcome {
    /// Done with annotations: the flattened image, to be written over the file.
    Saved(Image),
    /// Done (or closed) without any annotation.
    Unchanged,
    Cancelled,
}

pub struct EditorOutcome {
    pub hwnd: HWND,
    pub outcome: Outcome,
    /// The capture as it was before editing.
    pub image: Arc<Image>,
    /// Possibly still being saved: resolved on a worker, never on the UI thread.
    pub file: PendingFile,
    pub prefs: Prefs,
    /// The style the editor opened with: only what changed gets remembered.
    pub initial: Prefs,
    /// Centre of the editor's monitor: the next thumbnail goes there.
    pub anchor: POINT,
}

/// Unsaved work handed over when Windows ends the session.
pub struct Snapshot {
    pub image: Image,
    pub file: Option<PathBuf>,
}

const CLASS_NAME: PCWSTR = w!("SgCapEditor");
const TIMER_TOAST: usize = 1;
const TIMER_RETRY: usize = 2;
const TIMER_FILE: usize = 3;
const TIMER_CARET: usize = 4;
const TOAST_MS: u128 = 1600;
const TOAST_FADE_MS: u128 = 300;

/// Colour palette (macOS system colours) with tooltip names.
const PALETTE: [(u32, &str); 8] = [
    (0xFF3B30, "Red"),
    (0xFF9500, "Orange"),
    (0xFFCC00, "Yellow"),
    (0x34C759, "Green"),
    (0x007AFF, "Blue"),
    (0xAF52DE, "Purple"),
    (0x000000, "Black"),
    (0xFFFFFF, "White"),
];
/// Stroke width presets in points (pixels at 100% scaling).
const WIDTHS_PT: [u32; 4] = [2, 4, 7, 12];
const WIDTH_NAMES: [&str; 4] = ["Thin", "Medium", "Thick", "Extra thick"];
/// How thick each width preset looks in the toolbar (pixels at 96 dpi).
const WIDTH_ICONS: [f32; 4] = [1.5, 3.0, 5.0, 8.0];
/// Text sizes in points that the width presets give text.
const TEXT_PT: [f32; 4] = [14.0, 20.0, 28.0, 40.0];
/// Highlighter strokes are this many times wider than the width preset.
const HIGHLIGHTER_WIDTH: f32 = 3.0;
/// Block size of pixelated areas in points: 12 px blocks leave no text readable.
const PIXEL_BLOCK_PT: f32 = 12.0;
/// Freehand strokes keep a point every this many screen pixels...
const FREEHAND_STEP: f32 = 1.5;
/// ...then drop the ones within this distance of the line through their
/// neighbours (screen pixels) when the stroke ends.
const FREEHAND_TOL: f32 = 0.4;
const ACCENT: u32 = 0x0A84FF;
const DONE_BLUE: u32 = 0x0067C0;

// Toolbar and canvas metrics, in pixels at 96 dpi.
const BAR_H: f32 = 48.0;
const CELL: f32 = 34.0;
const SWATCH: f32 = 28.0;
const GAP: f32 = 2.0;
const GROUP: f32 = 17.0;
const TEXT_BTN: f32 = 76.0;
const EDGE: f32 = 10.0;
const MARGIN: f32 = 20.0;
const MIN_CLIENT_H: f32 = 300.0;
/// The toolbar shrinks down to this fraction in narrow windows.
const MIN_BAR_SCALE: f32 = 0.6;
/// Extra reach around a stroke for picking it, and handle sizes (screen pixels).
const HIT_TOL: f32 = 5.0;
/// Selected boxes and ellipses smaller than this (screen pixels) are moved by
/// pressing anywhere inside them: their handles cover most of the stroke.
const SMALL_SHAPE: f32 = 48.0;
const HANDLE_R: f32 = 5.0;
const HANDLE_HIT: f32 = 9.0;
/// GPU render target failures before falling back to the software rasterizer.
const MAX_GPU_FAILURES: u32 = 3;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Item {
    Tool(Kind),
    Width(usize),
    Color(usize),
    MoreColors,
    Undo,
    Redo,
    Delete,
    Copy,
    Cancel,
    Done,
}

impl Item {
    fn tip(self) -> String {
        match self {
            Item::Tool(Kind::Line) => "Line (L)".into(),
            Item::Tool(Kind::Arrow) => "Arrow (A)".into(),
            Item::Tool(Kind::Rect) => "Rectangle (R)".into(),
            Item::Tool(Kind::Ellipse) => "Ellipse (E)".into(),
            Item::Tool(Kind::Pen) => "Pen (P)".into(),
            Item::Tool(Kind::Highlighter) => "Highlighter (H)".into(),
            Item::Tool(Kind::Text) => "Text (T)".into(),
            Item::Tool(Kind::Pixelate) => "Pixelate, to hide details (B)".into(),
            Item::Width(i) => format!("{} ({})", WIDTH_NAMES[i], i + 1),
            Item::Color(i) => PALETTE[i].1.into(),
            Item::MoreColors => "More colours...".into(),
            Item::Undo => "Undo (Ctrl+Z)".into(),
            Item::Redo => "Redo (Ctrl+Y)".into(),
            Item::Delete => "Delete the selected shape (Del)".into(),
            Item::Copy => "Copy the image (Ctrl+C)".into(),
            Item::Cancel => "Close without saving (Esc)".into(),
            Item::Done => "Save and close (Enter)".into(),
        }
    }
}

/// Toolbar geometry for one client width and scale.
#[derive(Default)]
struct Layout {
    buttons: Vec<(Item, RECT)>,
    separators: Vec<i32>,
    bar_h: i32,
    min_w: i32,
    /// Scale the toolbar was laid out at (the DPI scale, less when compacted).
    k: f32,
}

fn layout_at(k: f32, client_w: i32) -> Layout {
    let px = |v: f32| (v * k).round() as i32;
    let bar_h = px(BAR_H);
    let cell = px(CELL);
    let top = (bar_h - cell) / 2;
    // Offsets are added up at 96 dpi and each edge rounded once, so rounding
    // many small gaps cannot push the toolbar past the window edge.
    let cell_at = |x: f32, w: f32| RECT {
        left: px(x),
        top,
        right: px(x + w),
        bottom: top + cell,
    };
    let mut buttons = Vec::new();
    let mut separators = Vec::new();
    let mut x = EDGE;
    let group = |items: &[(Item, f32)], x: &mut f32, buttons: &mut Vec<(Item, RECT)>| {
        for &(item, w) in items {
            buttons.push((item, cell_at(*x, w)));
            *x += w + GAP;
        }
    };
    let tools: Vec<(Item, f32)> = Kind::ALL.iter().map(|&t| (Item::Tool(t), CELL)).collect();
    group(&tools, &mut x, &mut buttons);
    let widths: Vec<(Item, f32)> = (0..WIDTHS_PT.len()).map(|i| (Item::Width(i), CELL)).collect();
    let colors: Vec<(Item, f32)> = (0..PALETTE.len())
        .map(|i| (Item::Color(i), SWATCH))
        .chain(std::iter::once((Item::MoreColors, SWATCH)))
        .collect();
    let edits = [(Item::Undo, CELL), (Item::Redo, CELL), (Item::Delete, CELL)];
    for items in [&widths[..], &colors[..], &edits[..]] {
        separators.push(px(x - GAP + GROUP / 2.0));
        x += GROUP - GAP;
        group(items, &mut x, &mut buttons);
    }
    let left_end = x - GAP;
    let text_w = px(TEXT_BTN);
    let min_w = px(left_end + GROUP + CELL + 10.0 + TEXT_BTN + 8.0 + TEXT_BTN + EDGE);
    let right = client_w.max(min_w) - px(EDGE);
    let done = RECT {
        left: right - text_w,
        right,
        ..cell_at(0.0, 0.0)
    };
    let cancel = RECT {
        left: done.left - px(8.0) - text_w,
        right: done.left - px(8.0),
        ..done
    };
    let copy = RECT {
        left: cancel.left - px(10.0) - cell,
        right: cancel.left - px(10.0),
        ..cancel
    };
    separators.push(copy.left - px(GROUP) / 2);
    buttons.push((Item::Copy, copy));
    buttons.push((Item::Cancel, cancel));
    buttons.push((Item::Done, done));
    Layout {
        buttons,
        separators,
        bar_h,
        min_w,
        k,
    }
}

/// Toolbar for this client width: shrunk when the window is narrower than the
/// full toolbar (small screens, high DPI), so Cancel and Done stay on screen.
fn layout(dpi: u32, client_w: i32) -> Layout {
    let k = dpi as f32 / 96.0;
    let full = layout_at(k, client_w);
    if client_w <= 0 || client_w >= full.min_w {
        return full;
    }
    let f = ((client_w - 1) as f32 / full.min_w as f32).max(MIN_BAR_SCALE);
    layout_at(k * f, client_w)
}

struct Theme {
    bar: u32,
    canvas: u32,
    /// Hairline around the capture.
    frame: u32,
    line: u32,
    hover: u32,
    active: u32,
    icon: u32,
    dim: u32,
    swatch_edge: (u32, f32),
    button: u32,
    button_edge: u32,
}

const LIGHT: Theme = Theme {
    bar: 0xF3F3F3,
    canvas: 0xD9D9D9,
    frame: 0xB4B4B4,
    line: 0xD0D0D0,
    hover: 0xE2E2E2,
    active: 0xD0D0D0,
    icon: 0x1F1F1F,
    dim: 0xA8A8A8,
    swatch_edge: (0x000000, 0.22),
    button: 0xFFFFFF,
    button_edge: 0xC4C4C4,
};

const DARK: Theme = Theme {
    bar: 0x2B2B2B,
    canvas: 0x1A1A1A,
    frame: 0x4A4A4A,
    line: 0x404040,
    hover: 0x3A3A3A,
    active: 0x4A4A4A,
    icon: 0xF2F2F2,
    dim: 0x6E6E6E,
    swatch_edge: (0xFFFFFF, 0.35),
    button: 0x3A3A3A,
    button_edge: 0x525252,
};

/// Follows the Windows "app mode" (Settings > Personalisation > Colours).
fn apps_use_dark_theme() -> bool {
    let mut value = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    err == ERROR_SUCCESS && value == 0
}

fn set_dark_title_bar(hwnd: HWND, dark: bool) {
    let v = windows::core::BOOL::from(dark);
    unsafe {
        // Attribute 20 since Windows 10 20H1, 19 on earlier builds.
        for attr in [DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE(19)] {
            if DwmSetWindowAttribute(hwnd, attr, &v as *const _ as *const c_void, 4).is_ok() {
                break;
            }
        }
    }
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

/// Has the pointer left the click slop around `origin`?
fn moved_enough(origin: POINT, pt: POINT) -> bool {
    let th = unsafe { GetSystemMetrics(SM_CXDRAG) }.max(3);
    (pt.x - origin.x).abs() >= th || (pt.y - origin.y).abs() >= th
}

/// Text size in points for a stroke width in points: the presets give
/// TEXT_PT, other widths (hand-edited settings) something in between.
fn text_pt(width_pt: u32) -> f32 {
    let w = width_pt as f32;
    let at = |i: usize| (WIDTHS_PT[i] as f32, TEXT_PT[i]);
    if w <= at(0).0 {
        return at(0).1;
    }
    for i in 1..WIDTHS_PT.len() {
        let ((w0, t0), (w1, t1)) = (at(i - 1), at(i));
        if w <= w1 {
            return t0 + (t1 - t0) * (w - w0) / (w1 - w0);
        }
    }
    let (w1, t1) = at(WIDTHS_PT.len() - 1);
    t1 * w / w1
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Byte offset in `s` of UTF-16 offset `pos` (DirectWrite counts in UTF-16).
fn byte_at_utf16(s: &str, pos: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        if n >= pos {
            return i;
        }
        n += c.len_utf16();
    }
    s.len()
}

/// Caret (byte offset) nearest to image point `p` in a text shape.
fn caret_at(s: &Shape, p: Pt) -> usize {
    match TextBox::new(&s.text, s.style.width) {
        Ok(t) => byte_at_utf16(&s.text, t.hit(p.x - s.a.x, p.y - s.a.y)),
        Err(_) => s.text.len(),
    }
}

/// Caret moves and deletions in typed text: each takes the text and the
/// caret (a byte offset on a char boundary) and returns the new caret.
mod caret {
    pub fn left(t: &str, c: usize) -> usize {
        t[..c].chars().next_back().map_or(c, |ch| c - ch.len_utf8())
    }

    pub fn right(t: &str, c: usize) -> usize {
        t[c..].chars().next().map_or(c, |ch| c + ch.len_utf8())
    }

    pub fn home(t: &str, c: usize) -> usize {
        t[..c].rfind('\n').map_or(0, |i| i + 1)
    }

    pub fn end(t: &str, c: usize) -> usize {
        t[c..].find('\n').map_or(t.len(), |i| c + i)
    }

    pub fn backspace(t: &mut String, c: usize) -> usize {
        let n = left(t, c);
        t.replace_range(n..c, "");
        n
    }

    pub fn delete(t: &mut String, c: usize) -> usize {
        let n = right(t, c);
        t.replace_range(c..n, "");
        c
    }

    pub fn insert(t: &mut String, c: usize, s: &str) -> usize {
        t.insert_str(c, s);
        c + s.len()
    }
}

fn bgr(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF))
}

fn rgb(c: COLORREF) -> u32 {
    let v = c.0;
    ((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF)
}

fn title_for(path: Option<&PathBuf>) -> String {
    match path.and_then(|p| p.file_stem()) {
        Some(stem) => format!("{} - SgCap", stem.to_string_lossy()),
        None => "Annotate - SgCap".to_string(),
    }
}

/// Centre of the work area of the monitor showing `hwnd` (its restored
/// position when minimised).
fn monitor_center(hwnd: HWND) -> POINT {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        POINT {
            x: mi.rcWork.left + rect_w(&mi.rcWork) / 2,
            y: mi.rcWork.top + rect_h(&mi.rcWork) / 2,
        }
    }
}

thread_local! {
    /// The colour dialog's 16 custom colours, kept for the session. Copied in
    /// and out around the (modal) dialog, never borrowed across it.
    static CUSTOM_COLORS: Cell<[COLORREF; 16]> = const { Cell::new([COLORREF(0x00FFFFFF); 16]) };
    static CLASS_REGISTERED: Cell<bool> = const { Cell::new(false) };
}

enum Drag {
    Idle,
    /// Drawing a new shape; it only becomes real once the mouse has moved.
    Create {
        shape: Shape,
        origin: POINT,
        started: bool,
    },
    Move {
        idx: usize,
        before: Vec<Shape>,
        start: Pt,
        orig: Shape,
        origin: POINT,
        started: bool,
    },
    Reshape {
        idx: usize,
        handle: Handle,
        before: Vec<Shape>,
        /// Handle position minus the press point, so the handle does not jump.
        grab: Pt,
        origin: POINT,
        started: bool,
    },
    /// A toolbar button is held down.
    Press(Item),
}

/// What a press at some point would grab.
enum Hit {
    Handle(usize, Handle),
    Shape(usize),
    Empty,
}

/// Text being typed into a text shape.
struct TextEdit {
    idx: usize,
    /// Byte offset of the caret in the shape's text.
    caret: usize,
    /// The shapes before the edit began: the whole edit is one undo step.
    before: Vec<Shape>,
}

struct State {
    image: Arc<Image>,
    file: PendingFile,
    capture_scale: f32,
    doc: Doc,
    selected: Option<usize>,
    tool: Kind,
    color: u32,
    width_pt: u32,
    initial: Prefs,
    drag: Drag,
    editing: Option<TextEdit>,
    /// Blink phase of the text caret.
    caret_on: bool,
    /// First half of a character outside the BMP typed as two WM_CHARs.
    high_surrogate: Option<u16>,
    /// The capture averaged into blocks for pixelated areas, made the first
    /// time one is drawn.
    mosaic: Option<Mosaic>,
    /// The last commit was an arrow-key nudge still in progress: further
    /// nudges join that undo step.
    nudging: bool,
    last_mouse: POINT,
    hover: Option<Item>,
    tracking: bool,
    dpi: u32,
    client: (i32, i32),
    lay: Layout,
    view_scale: f32,
    view_off: (f32, f32),
    target: Option<Target>,
    target_failures: u32,
    fonts: Option<Fonts>,
    dark: bool,
    toast: Option<(String, Instant)>,
    tooltip: HWND,
    tooltips_added: usize,
    closed: bool,
}

impl State {
    fn k(&self) -> f32 {
        self.dpi as f32 / 96.0
    }

    fn theme(&self) -> &'static Theme {
        if self.dark {
            &DARK
        } else {
            &LIGHT
        }
    }

    fn image_size(&self) -> (f32, f32) {
        (self.image.width as f32, self.image.height as f32)
    }

    /// Style::width in whole image pixels for a kind of shape and a width
    /// preset in points: the stroke width, three times it for highlighters,
    /// the matching font size for text, a fixed block size for pixelation.
    fn style_width(&self, kind: Kind, width_pt: u32) -> f32 {
        let px = |pt: f32| (pt * self.capture_scale).round().max(1.0);
        match kind {
            Kind::Highlighter => px(width_pt as f32) * HIGHLIGHTER_WIDTH,
            Kind::Text => px(text_pt(width_pt)),
            Kind::Pixelate => px(PIXEL_BLOCK_PT),
            Kind::Line | Kind::Arrow | Kind::Rect | Kind::Ellipse | Kind::Pen => px(width_pt as f32),
        }
    }

    fn current_style(&self) -> Style {
        Style {
            color: self.color,
            width: self.style_width(self.tool, self.width_pt),
        }
    }

    /// The toolbar shows the selected shape's kind and style, or the ones
    /// for new shapes.
    fn shown(&self) -> (Kind, Style) {
        self.selected
            .and_then(|i| self.doc.shapes.get(i))
            .map(|s| (s.kind, s.style))
            .unwrap_or_else(|| (self.tool, self.current_style()))
    }

    fn relayout(&mut self) {
        self.lay = layout(self.dpi, self.client.0);
        let k = self.k();
        let (iw, ih) = self.image_size();
        let (iw, ih) = (iw.max(1.0), ih.max(1.0));
        let margin = MARGIN * k;
        let top = self.lay.bar_h as f32;
        let avail_w = self.client.0 as f32 - 2.0 * margin;
        let avail_h = self.client.1 as f32 - top - 2.0 * margin;
        let scale = (avail_w / iw).min(avail_h / ih).min(1.0).max(0.01);
        // Whole-pixel offsets: crisp at 100%, and the pre-scaled view bitmap
        // is drawn 1:1.
        self.view_scale = scale;
        self.view_off = (
            ((self.client.0 as f32 - iw * scale) * 0.5).floor(),
            (top + (self.client.1 as f32 - top - ih * scale) * 0.5).floor(),
        );
    }

    fn view_rect(&self) -> RectF {
        RectF {
            l: self.view_off.0,
            t: self.view_off.1,
            r: self.view_off.0 + self.image.width as f32 * self.view_scale,
            b: self.view_off.1 + self.image.height as f32 * self.view_scale,
        }
    }

    /// Client pixel (its centre) to image coordinates.
    fn to_img(&self, p: POINT) -> Pt {
        Pt::new(
            (p.x as f32 + 0.5 - self.view_off.0) / self.view_scale,
            (p.y as f32 + 0.5 - self.view_off.1) / self.view_scale,
        )
    }

    fn to_view(&self, p: Pt) -> Pt {
        Pt::new(
            p.x * self.view_scale + self.view_off.0,
            p.y * self.view_scale + self.view_off.1,
        )
    }

    fn item_at(&self, p: POINT) -> Option<Item> {
        if p.y >= self.lay.bar_h {
            return None;
        }
        self.lay
            .buttons
            .iter()
            .find(|(_, r)| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom)
            .map(|(i, _)| *i)
    }

    fn enabled(&self, item: Item) -> bool {
        match item {
            Item::Undo => self.doc.can_undo(),
            Item::Redo => self.doc.can_redo(),
            Item::Delete => self.selected.is_some(),
            // Pixelated areas have neither colour nor width.
            Item::Width(_) | Item::Color(_) | Item::MoreColors => self.shown().0 != Kind::Pixelate,
            _ => true,
        }
    }

    fn is_active(&self, item: Item, shown: (Kind, Style)) -> bool {
        let (kind, style) = shown;
        match item {
            Item::Tool(k) => self.tool == k,
            _ if !self.enabled(item) => false,
            Item::Width(i) => (self.style_width(kind, WIDTHS_PT[i]) - style.width).abs() < 0.01,
            Item::Color(i) => PALETTE[i].0 == style.color,
            Item::MoreColors => !PALETTE.iter().any(|p| p.0 == style.color),
            _ => false,
        }
    }

    /// A press on a drawn handle of the selected shape takes the handle; on a
    /// stroke, or inside a small selected box, the shape; near a handle but
    /// off every stroke, the handle again.
    fn hit_at(&self, pt: POINT) -> Hit {
        let p = self.to_img(pt);
        let per_px = self.k() / self.view_scale;
        let sel = self.selected.and_then(|i| self.doc.shapes.get(i).map(|s| (i, s)));
        if let Some((i, s)) = sel {
            let mut r = HANDLE_R * per_px;
            if s.kind.is_linear() {
                // The middle third of a short line still moves it.
                r = r.min(s.a.dist(s.b) / 3.0);
            } else {
                // Likewise the middle of a tiny box (moved by the rule below).
                let (lo, hi) = s.bounds();
                r = r.min((hi.x - lo.x).min(hi.y - lo.y) / 3.0);
            }
            if let Some(h) = s.handle_at(p, r) {
                return Hit::Handle(i, h);
            }
            if !s.kind.is_linear() {
                let (lo, hi) = s.bounds();
                let m = s.style.width * 0.5;
                let side = (hi.x - lo.x).max(hi.y - lo.y) * self.view_scale;
                let inside = p.x >= lo.x - m && p.x <= hi.x + m && p.y >= lo.y - m && p.y <= hi.y + m;
                if side < SMALL_SHAPE * self.k() && inside {
                    return Hit::Shape(i);
                }
            }
        }
        if let Some(i) = self.doc.shape_at(p, HIT_TOL * per_px) {
            return Hit::Shape(i);
        }
        if let Some((i, s)) = sel {
            if let Some(h) = s.handle_at(p, HANDLE_HIT * per_px) {
                return Hit::Handle(i, h);
            }
        }
        Hit::Empty
    }

    fn cursor_for(&self, pt: POINT) -> PCWSTR {
        if pt.y < self.lay.bar_h && matches!(self.drag, Drag::Idle | Drag::Press(_)) {
            return IDC_ARROW;
        }
        let handle_cursor = |h: Handle| match h {
            Handle::A | Handle::B => IDC_SIZEALL,
            Handle::NW | Handle::SE => IDC_SIZENWSE,
            Handle::NE | Handle::SW => IDC_SIZENESW,
            Handle::N | Handle::S => IDC_SIZENS,
            Handle::E | Handle::W => IDC_SIZEWE,
        };
        let text_tool = self.tool == Kind::Text;
        match &self.drag {
            Drag::Move { .. } => IDC_SIZEALL,
            Drag::Reshape { handle, .. } => handle_cursor(*handle),
            Drag::Create { .. } => IDC_CROSS,
            Drag::Press(_) => IDC_ARROW,
            Drag::Idle if self.in_edited_text(pt) => IDC_IBEAM,
            Drag::Idle => match self.hit_at(pt) {
                Hit::Handle(_, h) => handle_cursor(h),
                // With the Text tool a click on text edits it.
                Hit::Shape(i) if text_tool && self.doc.shapes[i].kind == Kind::Text => IDC_IBEAM,
                Hit::Shape(_) => IDC_SIZEALL,
                Hit::Empty if text_tool => IDC_IBEAM,
                Hit::Empty => IDC_CROSS,
            },
        }
    }

    /// Is `pt` on the text being typed?
    fn in_edited_text(&self, pt: POINT) -> bool {
        let tol = HIT_TOL * self.k() / self.view_scale;
        self.editing
            .as_ref()
            .and_then(|e| self.doc.shapes.get(e.idx))
            .is_some_and(|s| pt.y >= self.lay.bar_h && s.hit(self.to_img(pt), tol))
    }

    /// The capture's mosaic, for pixelated areas.
    fn ensure_mosaic(&mut self) {
        let block = self.style_width(Kind::Pixelate, 0) as u32;
        if self.mosaic.as_ref().is_some_and(|m| m.block == block) {
            return;
        }
        let t0 = Instant::now();
        let img = &self.image;
        self.mosaic = Some(Mosaic::new(&img.bgra, img.width as usize, img.height as usize, block));
        log(&format!("editor: {block} px mosaic made in {} ms", t0.elapsed().as_millis()));
    }

    /// Starts a drag for a press below the toolbar.
    fn begin_drag(&mut self, pt: POINT) {
        let p = self.to_img(pt);
        self.drag = match self.hit_at(pt) {
            Hit::Handle(idx, handle) => {
                let at = self.doc.shapes[idx]
                    .handles()
                    .into_iter()
                    .find(|(h, _)| *h == handle)
                    .map(|(_, q)| q)
                    .unwrap_or(p);
                Drag::Reshape {
                    idx,
                    handle,
                    before: self.doc.shapes.clone(),
                    grab: at.sub(p),
                    origin: pt,
                    started: false,
                }
            }
            Hit::Shape(idx) => {
                self.selected = Some(idx);
                Drag::Move {
                    idx,
                    before: self.doc.shapes.clone(),
                    start: p,
                    orig: self.doc.shapes[idx].clone(),
                    origin: pt,
                    started: false,
                }
            }
            Hit::Empty => {
                self.selected = None;
                let (iw, ih) = self.image_size();
                match self.tool {
                    // Text starts where it is clicked, typed at once (on the
                    // image: the margin around it is clipped away).
                    Kind::Text => {
                        if p.x >= 0.0 && p.x < iw && p.y >= 0.0 && p.y < ih {
                            self.begin_new_text(p);
                        }
                        Drag::Idle
                    }
                    tool => {
                        if tool == Kind::Pixelate {
                            self.ensure_mosaic();
                        }
                        Drag::Create {
                            shape: Shape::new(tool, p, p, self.current_style()),
                            origin: pt,
                            started: false,
                        }
                    }
                }
            }
        };
    }

    /// Applies the mouse position to the current drag; true if something moved.
    /// Nothing happens until the pointer leaves the click slop, so a click
    /// only selects.
    fn apply_drag(&mut self, pt: POINT, shift: bool) -> bool {
        let p = self.to_img(pt);
        let step = FREEHAND_STEP / self.view_scale;
        match &mut self.drag {
            Drag::Create { shape, origin, started } => {
                if !*started && !moved_enough(*origin, pt) {
                    return false;
                }
                *started = true;
                if shape.kind.is_freehand() {
                    shape.extend(p, shift, step);
                } else {
                    shape.set_end(p, shift);
                }
                shape.snap_to_pixels();
                true
            }
            Drag::Move {
                idx,
                start,
                orig,
                origin,
                started,
                ..
            } => {
                if !*started && !moved_enough(*origin, pt) {
                    return false;
                }
                *started = true;
                let mut d = p.sub(*start);
                if shift {
                    // Shift keeps the move horizontal or vertical.
                    if d.x.abs() >= d.y.abs() {
                        d.y = 0.0;
                    } else {
                        d.x = 0.0;
                    }
                }
                let mut moved = orig.clone();
                moved.translate(d);
                moved.snap_to_pixels();
                if let Some(s) = self.doc.shapes.get_mut(*idx) {
                    *s = moved;
                }
                true
            }
            Drag::Reshape {
                idx,
                handle,
                grab,
                origin,
                started,
                ..
            } => {
                if !*started && !moved_enough(*origin, pt) {
                    return false;
                }
                *started = true;
                if let Some(s) = self.doc.shapes.get_mut(*idx) {
                    s.drag_handle(*handle, p.add(*grab), shift);
                    s.snap_to_pixels();
                }
                true
            }
            Drag::Press(_) | Drag::Idle => false,
        }
    }

    /// Ends the current drag, recording it in the undo history. Returns the
    /// toolbar item to activate (a press released over its own button).
    fn end_drag(&mut self, release_at: Option<POINT>) -> Option<Item> {
        self.nudging = false;
        let (iw, ih) = self.image_size();
        match std::mem::replace(&mut self.drag, Drag::Idle) {
            Drag::Create {
                mut shape, started, ..
            } => {
                shape.simplify(FREEHAND_TOL / self.view_scale);
                // Shapes entirely off the image would be invisible yet count.
                if started && !shape.is_degenerate(2.0 / self.view_scale) && shape.touches_image(iw, ih) {
                    shape.normalize();
                    shape.snap_to_pixels();
                    let before = self.doc.shapes.clone();
                    self.doc.shapes.push(shape);
                    self.doc.commit(before);
                    self.selected = Some(self.doc.shapes.len() - 1);
                }
                None
            }
            Drag::Move {
                idx,
                started: false,
                start,
                ..
            } => {
                // A click on text with the Text tool edits it.
                if self.tool == Kind::Text && self.doc.shapes.get(idx).is_some_and(|s| s.kind == Kind::Text) {
                    self.begin_edit(idx, Some(start));
                }
                None
            }
            Drag::Move {
                idx, before, started, ..
            }
            | Drag::Reshape {
                idx, before, started, ..
            } => {
                if started {
                    let keep = match self.doc.shapes.get_mut(idx) {
                        Some(s) => {
                            s.normalize();
                            s.snap_to_pixels();
                            s.touches_image(iw, ih) && !s.is_degenerate(1.0)
                        }
                        None => false,
                    };
                    if keep {
                        self.doc.commit(before);
                    } else {
                        // Dragged off the image or collapsed: put it back.
                        self.doc.shapes = before;
                    }
                }
                None
            }
            Drag::Press(item) => release_at.filter(|&p| self.item_at(p) == Some(item)).map(|_| item),
            Drag::Idle => None,
        }
    }

    /// Esc during a drag: put everything back as it was.
    fn cancel_drag(&mut self) -> bool {
        match std::mem::replace(&mut self.drag, Drag::Idle) {
            Drag::Move { before, .. } | Drag::Reshape { before, .. } => {
                self.doc.shapes = before;
                true
            }
            Drag::Create { .. } | Drag::Press(_) => true,
            Drag::Idle => false,
        }
    }

    /// Sets the style for new shapes and applies it to the selected shape.
    fn set_style(&mut self, color: Option<u32>, width_pt: Option<u32>) {
        self.nudging = false;
        if let Some(c) = color {
            self.color = c;
        }
        if let Some(w) = width_pt {
            self.width_pt = w;
        }
        let Some(i) = self.selected else {
            return;
        };
        let Some(kind) = self.doc.shapes.get(i).map(|s| s.kind) else {
            return;
        };
        if kind == Kind::Pixelate {
            return;
        }
        let before = self.doc.shapes.clone();
        let width = self.style_width(kind, self.width_pt);
        if let Some(s) = self.doc.shapes.get_mut(i) {
            if let Some(c) = color {
                s.style.color = c;
            }
            if width_pt.is_some() {
                s.style.width = width;
                // A width of the other parity moves crisp edges to the other grid.
                s.snap_to_pixels();
            }
        }
        self.fit_text(i);
        // Text being typed: the restyle is part of the edit's undo step.
        if self.editing.is_none() {
            self.doc.commit(before);
        }
    }

    /// Text shapes: b follows the laid-out text (an empty one keeps room
    /// for the caret).
    fn fit_text(&mut self, idx: usize) {
        let Some(s) = self.doc.shapes.get_mut(idx).filter(|s| s.kind == Kind::Text) else {
            return;
        };
        let size = s.style.width;
        let (w, h) = match TextBox::new(&s.text, size) {
            Ok(t) => t.extent(),
            Err(e) => {
                log(&format!("editor: text layout failed: {e}"));
                (0.0, size * 1.33)
            }
        };
        s.b = Pt::new(s.a.x + w.max(size * 0.3).ceil(), s.a.y + h.max(1.0).ceil());
    }

    /// Starts typing a new text whose first line is centred on `p`.
    fn begin_new_text(&mut self, p: Pt) {
        let before = self.doc.shapes.clone();
        self.doc.shapes.push(Shape::new(Kind::Text, p, p, self.current_style()));
        let idx = self.doc.shapes.len() - 1;
        self.fit_text(idx);
        let s = &mut self.doc.shapes[idx];
        let h = s.b.y - s.a.y;
        s.translate(Pt::new(0.0, -h * 0.5));
        s.snap_to_pixels();
        self.selected = Some(idx);
        self.editing = Some(TextEdit { idx, caret: 0, before });
        self.caret_on = true;
    }

    /// Edits text shape `idx`, the caret nearest to image point `at` (else
    /// at the end).
    fn begin_edit(&mut self, idx: usize, at: Option<Pt>) {
        let Some(s) = self.doc.shapes.get(idx).filter(|s| s.kind == Kind::Text) else {
            return;
        };
        let caret = match at {
            Some(p) => caret_at(s, p),
            None => s.text.len(),
        };
        self.editing = Some(TextEdit {
            idx,
            caret,
            before: self.doc.shapes.clone(),
        });
        self.selected = Some(idx);
        self.caret_on = true;
    }

    /// Stops typing: the whole edit becomes one undo step, and text left
    /// blank goes away. True if text was being typed.
    fn end_edit(&mut self) -> bool {
        let Some(e) = self.editing.take() else {
            return false;
        };
        if self.doc.shapes.get(e.idx).is_some_and(|s| s.is_empty_text()) {
            self.doc.shapes.remove(e.idx);
            self.selected = None;
        }
        self.doc.commit(e.before);
        true
    }

    /// Applies `f(text, caret) -> caret` to the text being typed.
    fn edit(&mut self, f: impl FnOnce(&mut String, usize) -> usize) -> bool {
        let Some(e) = &mut self.editing else {
            return false;
        };
        let Some(s) = self.doc.shapes.get_mut(e.idx) else {
            return false;
        };
        let at = e.caret.min(s.text.len());
        e.caret = f(&mut s.text, at);
        let idx = e.idx;
        self.fit_text(idx);
        self.caret_on = true;
        true
    }

    /// Up and Down: the caret moves a line, keeping its x.
    fn edit_line(&mut self, down: bool) -> bool {
        let Some(e) = &self.editing else {
            return false;
        };
        let Some(s) = self.doc.shapes.get(e.idx) else {
            return false;
        };
        let Ok(tb) = TextBox::new(&s.text, s.style.width) else {
            return false;
        };
        let caret = e.caret.min(s.text.len());
        let (x, y, h) = tb.caret(utf16_len(&s.text[..caret]));
        let target = if down { y + h * 1.5 } else { y - h * 0.5 };
        let to = if target < 0.0 {
            0
        } else if target > s.b.y - s.a.y {
            s.text.len()
        } else {
            byte_at_utf16(&s.text, tb.hit(x, target))
        };
        self.edit(|_, _| to)
    }

    /// A press inside the text being typed moves the caret there.
    fn place_caret(&mut self, pt: POINT) {
        let p = self.to_img(pt);
        let Some(e) = &self.editing else {
            return;
        };
        let Some(s) = self.doc.shapes.get(e.idx) else {
            return;
        };
        let to = caret_at(s, p);
        self.edit(|_, _| to);
    }

    /// Arrow keys: one held key (its auto-repeats) is a single undo step.
    fn nudge(&mut self, dx: f32, dy: f32) -> bool {
        let Some(i) = self.selected else {
            return false;
        };
        if !matches!(self.drag, Drag::Idle) {
            return false;
        }
        let Some(mut moved) = self.doc.shapes.get(i).cloned() else {
            return false;
        };
        moved.translate(Pt::new(dx, dy));
        let (iw, ih) = self.image_size();
        if !moved.touches_image(iw, ih) {
            return false;
        }
        let before = self.doc.shapes.clone();
        self.doc.shapes[i] = moved;
        if !self.nudging {
            self.nudging = self.doc.commit(before);
        }
        true
    }

    /// Anything drawn (text still empty does not count).
    fn dirty(&self) -> bool {
        self.doc.shapes.iter().any(|s| !s.is_empty_text())
    }

    fn prefs(&self) -> Prefs {
        Prefs {
            tool: self.tool,
            color: self.color,
            width_pt: self.width_pt,
        }
    }
}

fn draw(c: &Canvas, s: &State) {
    let th = s.theme();
    let k = s.k();
    c.clear(th.canvas);
    let v = s.view_rect();
    // Hairline frame so a dark capture stays distinct from the dark canvas.
    c.stroke_rect(v.inset(-0.5), th.frame, 1.0, 1.0);
    c.image(v, s.view_scale < 0.999);
    c.push_clip(v);
    c.set_view(s.view_scale, s.view_off.0, s.view_off.1);
    for shape in &s.doc.shapes {
        c.shape(shape);
    }
    if let Drag::Create {
        shape, started: true, ..
    } = &s.drag
    {
        c.shape(shape);
    }
    c.reset_view();
    c.pop_clip();

    if let Some(shape) = s.selected.and_then(|i| s.doc.shapes.get(i)) {
        // Shapes without an outline on their bounds show the box: ellipses
        // and pixelated areas (their handles sit on it), text and strokes.
        let frame = match shape.kind {
            Kind::Ellipse | Kind::Pixelate => Some(0.0),
            Kind::Text => Some(3.0 * k),
            Kind::Pen | Kind::Highlighter => Some(shape.style.width * 0.5 * s.view_scale + 3.0 * k),
            Kind::Line | Kind::Arrow | Kind::Rect => None,
        };
        if let Some(pad) = frame {
            let (lo, hi) = shape.bounds();
            let (a, b) = (s.to_view(lo), s.to_view(hi));
            let r = RectF { l: a.x, t: a.y, r: b.x, b: b.y }.inset(-pad);
            c.stroke_rect(r, ACCENT, 0.7, 1.0);
        }
        for (_, p) in shape.handles() {
            let q = s.to_view(p);
            c.fill_circle(q, HANDLE_R * k, 0xFFFFFF, 1.0);
            c.stroke_circle(q, HANDLE_R * k, ACCENT, 1.0, 1.5 * k);
        }
    }
    if let Some((e, t)) = s.editing.as_ref().and_then(|e| s.doc.shapes.get(e.idx).map(|t| (e, t))) {
        if s.caret_on {
            if let Ok(tb) = TextBox::new(&t.text, t.style.width) {
                let (x, y, h) = tb.caret(utf16_len(&t.text[..e.caret.min(t.text.len())]));
                let top = s.to_view(Pt::new(t.a.x + x, t.a.y + y));
                let bottom = s.to_view(Pt::new(t.a.x + x, t.a.y + y + h));
                let half = (k * 0.75).max(0.5);
                c.fill_rect(RectF { l: top.x - half, t: top.y, r: top.x + half, b: bottom.y }, t.style.color, 1.0);
            }
        }
    }

    draw_toolbar(c, s, th);
    draw_toast(c, s);
}

fn draw_toolbar(c: &Canvas, s: &State, th: &Theme) {
    let Some(fonts) = &s.fonts else {
        return;
    };
    let k = s.lay.k;
    let w = s.client.0 as f32;
    let bar = s.lay.bar_h as f32;
    let hair = s.k().round().max(1.0);
    c.fill_rect(RectF { l: 0.0, t: 0.0, r: w, b: bar }, th.bar, 1.0);
    c.fill_rect(RectF { l: 0.0, t: bar - hair, r: w, b: bar }, th.line, 1.0);
    for &x in &s.lay.separators {
        let x = x as f32;
        c.fill_rect(RectF { l: x, t: bar * 0.27, r: x + hair, b: bar * 0.73 }, th.line, 1.0);
    }
    let shown = s.shown();
    let radius = 6.0 * k;
    for &(item, r) in &s.lay.buttons {
        let rf = RectF::from(r);
        let enabled = s.enabled(item);
        let hover = enabled && s.hover == Some(item);
        let pressed = hover && matches!(s.drag, Drag::Press(p) if p == item);
        let active = s.is_active(item, shown);
        let fg = if enabled { th.icon } else { th.dim };
        let ctr = Pt::new((rf.l + rf.r) * 0.5, (rf.t + rf.b) * 0.5);
        match item {
            Item::Done => {
                let a = if pressed { 0.8 } else if hover { 0.9 } else { 1.0 };
                c.fill_round(rf, radius, DONE_BLUE, a);
                c.text("Done", rf, &fonts.ui, 0xFFFFFF, 1.0);
            }
            Item::Cancel => {
                let bg = if pressed {
                    th.active
                } else if hover {
                    th.hover
                } else {
                    th.button
                };
                c.fill_round(rf, radius, bg, 1.0);
                c.stroke_round(rf.inset(0.5), radius, th.button_edge, 1.0, 1.0);
                c.text("Cancel", rf, &fonts.ui, th.icon, 1.0);
            }
            Item::Color(_) | Item::MoreColors => {
                if hover && !active {
                    c.fill_round(rf, radius, th.hover, 1.0);
                }
                let color = match item {
                    Item::Color(i) => Some(PALETTE[i].0),
                    _ if active => Some(shown.1.color),
                    _ => None,
                };
                let a = if enabled { 1.0 } else { 0.3 };
                match color {
                    Some(col) => {
                        c.fill_circle(ctr, 9.0 * k, col, a);
                        c.stroke_circle(ctr, 9.0 * k - 0.5, th.swatch_edge.0, th.swatch_edge.1 * a, 1.0);
                        if active {
                            c.stroke_circle(ctr, 12.0 * k, th.icon, 1.0, 2.0 * k);
                        }
                    }
                    None => c.text("\u{E790}", rf, &fonts.icons, fg, 1.0),
                }
            }
            _ => {
                if pressed || active {
                    c.fill_round(rf, radius, th.active, 1.0);
                } else if hover {
                    c.fill_round(rf, radius, th.hover, 1.0);
                }
                draw_icon(c, item, ctr, k, fg, rf, fonts);
            }
        }
    }
}

fn draw_icon(c: &Canvas, item: Item, ctr: Pt, k: f32, fg: u32, rf: RectF, fonts: &Fonts) {
    let at = |x: f32, y: f32| Pt::new(ctr.x + x * k, ctr.y + y * k);
    match item {
        Item::Tool(Kind::Line) => c.line(at(-7.0, 7.0), at(7.0, -7.0), fg, 1.0, 2.0 * k),
        Item::Tool(Kind::Arrow) => {
            c.line(at(-7.0, 7.0), at(3.0, -3.0), fg, 1.0, 2.0 * k);
            c.fill_triangle(at(8.0, -8.0), at(0.5, -5.5), at(5.5, -0.5), fg, 1.0);
        }
        Item::Tool(Kind::Rect) => c.stroke_rect(
            RectF {
                l: ctr.x - 8.0 * k,
                t: ctr.y - 6.0 * k,
                r: ctr.x + 8.0 * k,
                b: ctr.y + 6.0 * k,
            },
            fg,
            1.0,
            1.8 * k,
        ),
        Item::Tool(Kind::Ellipse) => c.stroke_ellipse(ctr, 8.5 * k, 6.5 * k, fg, 1.0, 1.8 * k),
        Item::Tool(Kind::Pen) => {
            // A squiggle: one period of a sine.
            let wave: Vec<Pt> = (0..=16)
                .map(|i| {
                    let x = i as f32 - 8.0;
                    at(x, -4.5 * (x * std::f32::consts::PI / 8.0).sin())
                })
                .collect();
            c.stroke_points(&wave, fg, 1.0, 1.8 * k);
        }
        Item::Tool(Kind::Highlighter) => {
            // A marker leaning right, its chisel tip on a translucent band.
            c.line(at(-9.0, 6.5), at(3.0, 6.5), fg, HIGHLIGHTER_ALPHA, 4.5 * k);
            let (b0, b1, e0, e1) = (at(0.6, 3.6), at(-3.6, -0.6), at(8.4, -4.1), at(4.1, -8.4));
            for (p, q) in [(b0, b1), (b1, e1), (e1, e0), (e0, b0)] {
                c.line(p, q, fg, 1.0, 1.5 * k);
            }
            c.fill_triangle(b0, b1, at(-4.0, 4.0), fg, 1.0);
        }
        Item::Tool(Kind::Text) => {
            c.line(at(-6.0, -7.0), at(6.0, -7.0), fg, 1.0, 2.2 * k);
            c.line(at(0.0, -7.0), at(0.0, 7.5), fg, 1.0, 2.2 * k);
        }
        Item::Tool(Kind::Pixelate) => {
            for i in 0..3 {
                for j in 0..3 {
                    let (x, y) = (-7.5 + 5.0 * i as f32, -7.5 + 5.0 * j as f32);
                    let a = if (i + j) % 2 == 0 { 1.0 } else { 0.35 };
                    let r = RectF {
                        l: ctr.x + x * k,
                        t: ctr.y + y * k,
                        r: ctr.x + (x + 5.0) * k,
                        b: ctr.y + (y + 5.0) * k,
                    };
                    c.fill_rect(r, fg, a);
                }
            }
        }
        Item::Width(i) => c.line(at(-8.0, 0.0), at(8.0, 0.0), fg, 1.0, WIDTH_ICONS[i] * k),
        Item::Undo => c.text("\u{E7A7}", rf, &fonts.icons, fg, 1.0),
        Item::Redo => c.text("\u{E7A6}", rf, &fonts.icons, fg, 1.0),
        Item::Delete => c.text("\u{E74D}", rf, &fonts.icons, fg, 1.0),
        Item::Copy => c.text("\u{E8C8}", rf, &fonts.icons, fg, 1.0),
        _ => {}
    }
}

fn draw_toast(c: &Canvas, s: &State) {
    let (Some((text, t0)), Some(fonts)) = (&s.toast, &s.fonts) else {
        return;
    };
    let age = t0.elapsed().as_millis();
    if age >= TOAST_MS {
        return;
    }
    let a = if age > TOAST_MS - TOAST_FADE_MS {
        (TOAST_MS - age) as f32 / TOAST_FADE_MS as f32
    } else {
        1.0
    };
    let k = s.k();
    let w = 240.0 * k;
    let h = 36.0 * k;
    let cx = s.client.0 as f32 * 0.5;
    let b = s.client.1 as f32 - 24.0 * k;
    let r = RectF {
        l: cx - w * 0.5,
        t: b - h,
        r: cx + w * 0.5,
        b,
    };
    c.fill_round(r, h * 0.5, 0x202020, 0.88 * a);
    c.text(text, r, &fonts.ui, 0xFFFFFF, a);
}

pub struct Editor {
    hwnd: HWND,
    main_hwnd: HWND,
    state: RefCell<State>,
    /// Message boxes / colour dialog of ours currently open (nested loops).
    modal: Cell<u32>,
    /// Destroy requested while a dialog was open: done once it returns.
    pending_destroy: Cell<bool>,
}

impl Editor {
    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Runs a modal dialog. While it is up, close requests are ignored and a
    /// destroy waits, since the caller still uses the Editor afterwards.
    fn modal<R>(&self, f: impl FnOnce() -> R) -> R {
        self.modal.set(self.modal.get() + 1);
        let r = f();
        self.modal.set(self.modal.get() - 1);
        if self.modal.get() == 0 && self.pending_destroy.replace(false) {
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_EDITOR_DESTROY, WPARAM(0), LPARAM(0));
            }
        }
        r
    }

    fn paint(&self) {
        let mut recreate = false;
        let mut retry = false;
        if let Ok(mut guard) = self.state.try_borrow_mut() {
            let s = &mut *guard;
            if s.target.is_none() {
                let software = s.target_failures >= MAX_GPU_FAILURES;
                match Target::new(self.hwnd, &s.image, s.client.0, s.client.1, software) {
                    Ok(t) => s.target = Some(t),
                    Err(e) => {
                        s.target_failures += 1;
                        log(&format!("editor: render target failed (software {software}): {e}"));
                        retry = true;
                    }
                }
            }
            if s.fonts.is_none() {
                match Fonts::new(s.k()) {
                    Ok(f) => s.fonts = Some(f),
                    Err(e) => log(&format!("editor: fonts failed: {e}")),
                }
            }
            let s: &State = s;
            if let Some(t) = &s.target {
                if s.view_scale < 0.999 {
                    let v = s.view_rect();
                    t.prepare_view((v.r - v.l).round() as u32, (v.b - v.t).round() as u32);
                }
                match t.draw(s.mosaic.as_ref(), |c| draw(c, s)) {
                    Err(e) if d2d::needs_recreate(&e) => recreate = true,
                    Err(e) => log(&format!("editor: draw failed: {e}")),
                    Ok(()) => {}
                }
            }
        }
        if recreate {
            if let Ok(mut s) = self.state.try_borrow_mut() {
                s.target = None;
            }
            self.invalidate();
        }
        if retry {
            // The window would stay blank: try again shortly.
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_RETRY, 250, None);
            }
        }
    }

    /// Registers each toolbar button with the tooltip control (or moves it).
    fn sync_tooltips(&self) {
        let (tip, added, buttons) = {
            let Ok(s) = self.state.try_borrow() else {
                return;
            };
            (s.tooltip, s.tooltips_added, s.lay.buttons.clone())
        };
        if tip.is_invalid() {
            return;
        }
        for (i, (item, rect)) in buttons.iter().enumerate() {
            let mut text = wide(&item.tip());
            let mut ti = TTTOOLINFOW {
                cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_SUBCLASS,
                hwnd: self.hwnd,
                uId: i,
                rect: *rect,
                lpszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            let msg = if i < added { TTM_NEWTOOLRECTW } else { TTM_ADDTOOLW };
            unsafe {
                SendMessageW(tip, msg, None, Some(LPARAM(&mut ti as *mut _ as isize)));
            }
        }
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.tooltips_added = s.tooltips_added.max(buttons.len());
        }
    }

    fn on_size(&self, w: i32, h: i32) {
        {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            s.client = (w.max(1), h.max(1));
            s.relayout();
            if let Some(t) = &s.target {
                t.resize(s.client.0, s.client.1);
            }
        }
        self.sync_tooltips();
        self.invalidate();
    }

    /// The capture's save finished in the background: show its name.
    fn poll_file(&self) {
        let title = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            if !s.file.poll() {
                return;
            }
            title_for(s.file.path())
        };
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_FILE);
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(title));
        }
    }

    fn on_down(&self, pt: POINT) {
        let capture = {
            let Ok(mut guard) = self.state.try_borrow_mut() else {
                return;
            };
            let s = &mut *guard;
            if s.closed || !matches!(s.drag, Drag::Idle) {
                return;
            }
            s.nudging = false;
            s.last_mouse = pt;
            if pt.y < s.lay.bar_h {
                match s.item_at(pt) {
                    Some(item) if s.enabled(item) => {
                        s.drag = Drag::Press(item);
                        true
                    }
                    _ => false,
                }
            } else if s.in_edited_text(pt) {
                s.place_caret(pt);
                false
            } else {
                // A press anywhere else finishes the text being typed.
                s.end_edit();
                s.begin_drag(pt);
                true
            }
        };
        if capture {
            unsafe {
                SetCapture(self.hwnd);
            }
        }
        self.restart_caret();
        self.invalidate();
    }

    /// A double-click on text edits it, whatever the tool; anything else is
    /// an ordinary press.
    fn on_double_click(&self, pt: POINT) {
        let edit = {
            let Ok(mut guard) = self.state.try_borrow_mut() else {
                return;
            };
            let s = &mut *guard;
            let idle = !s.closed && matches!(s.drag, Drag::Idle) && s.editing.is_none() && pt.y >= s.lay.bar_h;
            match s.hit_at(pt) {
                Hit::Shape(i) if idle && s.doc.shapes[i].kind == Kind::Text => {
                    let p = s.to_img(pt);
                    s.begin_edit(i, Some(p));
                    true
                }
                _ => false,
            }
        };
        if edit {
            self.restart_caret();
            self.invalidate();
        } else {
            self.on_down(pt);
        }
    }

    /// Restarts the caret's blink (shown at once), or stops it when no text
    /// is being typed.
    fn restart_caret(&self) {
        let editing = self
            .state
            .try_borrow_mut()
            .map(|mut s| {
                s.caret_on = true;
                s.editing.is_some()
            })
            .unwrap_or(false);
        unsafe {
            let ms = GetCaretBlinkTime();
            // INFINITE (u32::MAX): the caret does not blink.
            if editing && ms != 0 && ms != u32::MAX {
                SetTimer(Some(self.hwnd), TIMER_CARET, ms, None);
            } else {
                let _ = KillTimer(Some(self.hwnd), TIMER_CARET);
            }
        }
    }

    /// WM_CHAR: typed text goes into the text being edited.
    fn on_char(&self, unit: u16) -> bool {
        let typed = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return false;
            };
            if s.closed || s.editing.is_none() {
                return false;
            }
            if (0xD800..0xDC00).contains(&unit) {
                s.high_surrogate = Some(unit);
                return true;
            }
            let units = match s.high_surrogate.take() {
                Some(high) => vec![high, unit],
                None => vec![unit],
            };
            // Control characters (Enter, Backspace, Ctrl+letter) are keys.
            let text: String = char::decode_utf16(units)
                .filter_map(|c| c.ok())
                .filter(|c| !c.is_control())
                .collect();
            !text.is_empty() && s.edit(|t, c| caret::insert(t, c, &text))
        };
        if typed {
            self.restart_caret();
            self.invalidate();
        }
        true
    }

    /// Keys while text is being typed: None for those the editor handles as
    /// usual (Ctrl shortcuts), else whether the key was used.
    fn edit_key(&self, vk: u16, ctrl: bool) -> Option<bool> {
        let paste = ctrl && vk == 0x56; // V
        if ctrl && !paste && vk != VK_LEFT.0 && vk != VK_RIGHT.0 && vk != VK_HOME.0 && vk != VK_END.0 {
            return None;
        }
        let clip = if paste { dragdrop::clipboard_text(self.hwnd) } else { None };
        let changed = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return Some(true);
            };
            if s.editing.is_none() {
                return None;
            }
            match vk {
                v if v == VK_ESCAPE.0 => s.end_edit(),
                v if v == VK_RETURN.0 => s.edit(|t, c| caret::insert(t, c, "\n")),
                v if v == VK_BACK.0 => s.edit(caret::backspace),
                v if v == VK_DELETE.0 => s.edit(caret::delete),
                v if v == VK_LEFT.0 => s.edit(|t, c| caret::left(t, c)),
                v if v == VK_RIGHT.0 => s.edit(|t, c| caret::right(t, c)),
                v if v == VK_HOME.0 && ctrl => s.edit(|_, _| 0),
                v if v == VK_END.0 && ctrl => s.edit(|t, _| t.len()),
                v if v == VK_HOME.0 => s.edit(|t, c| caret::home(t, c)),
                v if v == VK_END.0 => s.edit(|t, c| caret::end(t, c)),
                v if v == VK_UP.0 => s.edit_line(false),
                v if v == VK_DOWN.0 => s.edit_line(true),
                _ if paste => {
                    let text: String = clip.unwrap_or_default().chars().filter(|&c| c == '\n' || !c.is_control()).collect();
                    !text.is_empty() && s.edit(|t, c| caret::insert(t, c, &text))
                }
                // Typing arrives as WM_CHAR; no tool shortcut while typing.
                _ => false,
            }
        };
        if changed {
            self.restart_caret();
            self.invalidate();
        }
        Some(true)
    }

    fn on_move(&self, pt: POINT) {
        let (redraw, track) = {
            let Ok(mut guard) = self.state.try_borrow_mut() else {
                return;
            };
            let s = &mut *guard;
            if s.closed {
                return;
            }
            s.last_mouse = pt;
            let track = !s.tracking;
            s.tracking = true;
            let mut redraw = s.apply_drag(pt, key_down(VK_SHIFT));
            let hover = s.item_at(pt);
            if hover != s.hover {
                s.hover = hover;
                redraw = true;
            }
            (redraw, track)
        };
        if track {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
        }
        if redraw {
            self.invalidate();
        }
    }

    fn on_up(&self, pt: POINT) {
        let activate = {
            let Ok(mut guard) = self.state.try_borrow_mut() else {
                return;
            };
            let s = &mut *guard;
            if matches!(s.drag, Drag::Idle) {
                None
            } else {
                s.apply_drag(pt, key_down(VK_SHIFT));
                s.end_drag(Some(pt))
            }
        };
        unsafe {
            let _ = ReleaseCapture();
        }
        if let Some(item) = activate {
            self.activate(item);
        }
        self.restart_caret();
        self.invalidate();
    }

    /// Mouse capture taken away mid-drag (another window, a modal dialog):
    /// keep what was drawn so far, never fire a toolbar button.
    fn on_capture_lost(&self) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            if !matches!(s.drag, Drag::Idle) {
                s.end_drag(None);
            }
        }
        self.invalidate();
    }

    /// `repeat`: the key is held down (auto-repeat).
    fn on_key(&self, vk: u16, repeat: bool) -> bool {
        let arrow = [VK_LEFT.0, VK_RIGHT.0, VK_UP.0, VK_DOWN.0].contains(&vk);
        {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return true;
            };
            if !matches!(s.drag, Drag::Idle) && vk != VK_ESCAPE.0 {
                return true;
            }
            if !arrow || !repeat {
                // A fresh press starts a new undo step.
                s.nudging = false;
            }
        }
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        if key_down(VK_MENU) {
            return false;
        }
        if let Some(used) = self.edit_key(vk, ctrl) {
            return used;
        }
        // Only nudges and undo/redo repeat: a held Enter, Esc or Delete acts once.
        let repeatable = arrow || (ctrl && (vk == 0x5A || vk == 0x59));
        if repeat && !repeatable {
            return true;
        }
        let item = match vk {
            0x5A if ctrl => Some(if shift { Item::Redo } else { Item::Undo }), // Z
            0x59 if ctrl => Some(Item::Redo),                                   // Y
            0x43 if ctrl => Some(Item::Copy),                                   // C
            0x53 if ctrl => Some(Item::Done),                                   // S
            _ if ctrl => None,
            0x4C => Some(Item::Tool(Kind::Line)),            // L
            0x41 => Some(Item::Tool(Kind::Arrow)),           // A
            0x52 => Some(Item::Tool(Kind::Rect)),            // R
            0x45 | 0x4F => Some(Item::Tool(Kind::Ellipse)), // E, O
            0x50 => Some(Item::Tool(Kind::Pen)),             // P
            0x48 => Some(Item::Tool(Kind::Highlighter)),     // H
            0x54 => Some(Item::Tool(Kind::Text)),            // T
            0x42 => Some(Item::Tool(Kind::Pixelate)),        // B (blur)
            0x31..=0x34 => Some(Item::Width((vk - 0x31) as usize)),
            v if v == VK_RETURN.0 => Some(Item::Done),
            v if v == VK_DELETE.0 || v == VK_BACK.0 => Some(Item::Delete),
            _ => None,
        };
        if let Some(item) = item {
            self.activate(item);
            return true;
        }
        if vk == VK_ESCAPE.0 {
            self.escape();
            return true;
        }
        let step = if shift { 10.0 } else { 1.0 };
        let (dx, dy) = match vk {
            v if v == VK_LEFT.0 => (-step, 0.0),
            v if v == VK_RIGHT.0 => (step, 0.0),
            v if v == VK_UP.0 => (0.0, -step),
            v if v == VK_DOWN.0 => (0.0, step),
            _ => return false,
        };
        let moved = self.state.try_borrow_mut().map(|mut s| s.nudge(dx, dy)).unwrap_or(false);
        if moved {
            self.invalidate();
        }
        true
    }

    /// Shift pressed or released mid-drag: re-apply the constraint at once.
    fn on_shift(&self) {
        let redraw = self
            .state
            .try_borrow_mut()
            .map(|mut s| {
                let p = s.last_mouse;
                s.apply_drag(p, key_down(VK_SHIFT))
            })
            .unwrap_or(false);
        if redraw {
            self.invalidate();
        }
    }

    /// Esc: abandon the drag, else drop the selection, else close.
    fn escape(&self) {
        let step = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            if s.cancel_drag() {
                1
            } else if s.selected.take().is_some() {
                2
            } else {
                3
            }
        };
        match step {
            1 => unsafe {
                let _ = ReleaseCapture();
            },
            3 => return self.request_cancel(),
            _ => {}
        }
        self.invalidate();
    }

    fn activate(&self, item: Item) {
        // Width and colour restyle the text being typed; anything else
        // finishes it first.
        if !matches!(item, Item::Width(_) | Item::Color(_) | Item::MoreColors) {
            self.end_edit();
        }
        match item {
            Item::MoreColors => return self.choose_color(),
            Item::Copy => return self.copy(),
            Item::Cancel => return self.request_cancel(),
            Item::Done => return self.done(),
            _ => {}
        }
        {
            let Ok(mut guard) = self.state.try_borrow_mut() else {
                return;
            };
            let s = &mut *guard;
            if s.closed || !s.enabled(item) || !matches!(s.drag, Drag::Idle) {
                return;
            }
            s.nudging = false;
            match item {
                Item::Tool(k) => {
                    // Picking a tool means a new shape is next: the width and
                    // colour chosen now are for it, not for the last one drawn.
                    s.tool = k;
                    s.selected = None;
                }
                Item::Width(i) => s.set_style(None, Some(WIDTHS_PT[i])),
                Item::Color(i) => s.set_style(Some(PALETTE[i].0), None),
                Item::Undo => {
                    if s.doc.undo() {
                        s.selected = None;
                    }
                }
                Item::Redo => {
                    if s.doc.redo() {
                        s.selected = None;
                    }
                }
                Item::Delete => {
                    if let Some(i) = s.selected.take() {
                        let before = s.doc.shapes.clone();
                        if i < s.doc.shapes.len() {
                            s.doc.shapes.remove(i);
                        }
                        s.doc.commit(before);
                    }
                }
                _ => {}
            }
        }
        self.invalidate();
    }

    fn end_edit(&self) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.end_edit();
        }
        self.restart_caret();
    }

    fn choose_color(&self) {
        let initial = match self.state.try_borrow() {
            Ok(s) if !s.closed => s.shown().1.color,
            _ => return,
        };
        let mut custom = CUSTOM_COLORS.with(|c| c.get());
        let mut cc = CHOOSECOLORW {
            lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: self.hwnd,
            rgbResult: bgr(initial),
            lpCustColors: custom.as_mut_ptr(),
            Flags: CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR,
            ..Default::default()
        };
        let ok = self.modal(|| unsafe { ChooseColorW(&mut cc).as_bool() });
        CUSTOM_COLORS.with(|c| c.set(custom));
        if ok {
            if let Ok(mut s) = self.state.try_borrow_mut() {
                s.set_style(Some(rgb(cc.rgbResult)), None);
            }
            self.invalidate();
        }
    }

    /// The image with the annotations drawn in, at full resolution.
    fn flatten(&self) -> Option<Result<Image, String>> {
        let s = self.state.try_borrow().ok()?;
        if !s.dirty() {
            return None;
        }
        Some(d2d::render_export(&s.image, &s.doc.shapes, s.mosaic.as_ref()))
    }

    fn copy(&self) {
        let image = match self.flatten() {
            Some(Ok(img)) => Arc::new(img),
            Some(Err(e)) => {
                log(&format!("editor: flatten failed: {e}"));
                return;
            }
            None => match self.state.try_borrow() {
                Ok(s) => s.image.clone(),
                Err(_) => return,
            },
        };
        let text = match dragdrop::copy_to_clipboard(self.hwnd, &image, None) {
            Ok(()) => "Copied to the clipboard",
            Err(e) => {
                log(&format!("editor: clipboard failed: {e}"));
                "Could not copy the image"
            }
        };
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.toast = Some((text.to_string(), Instant::now()));
        }
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_TOAST, 40, None);
        }
        self.invalidate();
    }

    fn done(&self) {
        match self.flatten() {
            None => self.finish(Outcome::Unchanged),
            Some(Ok(img)) => self.finish(Outcome::Saved(img)),
            Some(Err(e)) => {
                log(&format!("editor: flatten failed: {e}"));
                self.modal(|| unsafe {
                    MessageBoxW(
                        Some(self.hwnd),
                        &HSTRING::from(format!("The annotations could not be applied: {e}")),
                        w!("SgCap"),
                        MB_OK | MB_ICONWARNING,
                    )
                });
            }
        }
    }

    fn is_dirty(&self) -> bool {
        self.state.try_borrow().map(|s| !s.closed && s.dirty()).unwrap_or(false)
    }

    /// Cancel button or Esc: confirm before throwing annotations away.
    fn request_cancel(&self) {
        if self.is_dirty() {
            let r = self.modal(|| unsafe {
                MessageBoxW(
                    Some(self.hwnd),
                    w!("Discard your annotations?"),
                    w!("SgCap"),
                    MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
                )
            });
            if r != IDYES {
                return;
            }
        }
        self.finish(Outcome::Cancelled);
    }

    /// Title bar close button, Alt+F4, the taskbar, Quit from the tray.
    fn on_close(&self) {
        if self.modal.get() > 0 {
            // One of our dialogs is already asking something.
            return;
        }
        self.end_edit();
        if !self.is_dirty() {
            return self.finish(Outcome::Unchanged);
        }
        let r = self.modal(|| unsafe {
            MessageBoxW(
                Some(self.hwnd),
                w!("Save your annotations?"),
                w!("SgCap"),
                MB_YESNOCANCEL | MB_ICONQUESTION,
            )
        });
        if r == IDYES {
            self.done();
        } else if r == IDNO {
            self.finish(Outcome::Cancelled);
        }
    }

    /// Windows is ending the session: hand the unsaved annotations to the
    /// main window (which writes them next to the original) as a raw Box.
    fn snapshot(&self) -> isize {
        let Ok(mut s) = self.state.try_borrow_mut() else {
            return 0;
        };
        if s.closed || !s.dirty() {
            return 0;
        }
        match d2d::render_export(&s.image, &s.doc.shapes, s.mosaic.as_ref()) {
            Ok(image) => {
                // No waiting at session end: an unfinished save leaves no name.
                s.file.poll();
                let file = s.file.path().cloned();
                Box::into_raw(Box::new(Snapshot { image, file })) as isize
            }
            Err(e) => {
                log(&format!("editor: flatten at session end failed: {e}"));
                0
            }
        }
    }

    /// Hands the result to the main window and closes.
    fn finish(&self, outcome: Outcome) {
        let payload = {
            let Ok(mut s) = self.state.try_borrow_mut() else {
                return;
            };
            if s.closed {
                return;
            }
            s.closed = true;
            s.drag = Drag::Idle;
            let file = std::mem::replace(&mut s.file, PendingFile::Ready(None));
            Box::new(EditorOutcome {
                hwnd: self.hwnd,
                outcome,
                image: s.image.clone(),
                file,
                prefs: s.prefs(),
                initial: s.initial,
                anchor: monitor_center(self.hwnd),
            })
        };
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let raw = Box::into_raw(payload);
            if PostMessageW(Some(self.main_hwnd), WM_APP_EDITOR_CLOSED, WPARAM(0), LPARAM(raw as isize)).is_err() {
                drop(Box::from_raw(raw));
                log("editor: cannot post the result");
            }
            if self.modal.get() > 0 {
                self.pending_destroy.set(true);
            } else {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_EDITOR_DESTROY, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let pt = || POINT {
            x: get_x(lparam),
            y: get_y(lparam),
        };
        unsafe {
            match msg {
                WM_ERASEBKGND => Some(LRESULT(1)),
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let _ = BeginPaint(self.hwnd, &mut ps);
                    self.paint();
                    let _ = EndPaint(self.hwnd, &ps);
                    Some(LRESULT(0))
                }
                WM_SIZE => {
                    if wparam.0 as u32 != SIZE_MINIMIZED {
                        self.on_size(loword(lparam.0 as usize) as i32, hiword(lparam.0 as usize) as i32);
                    }
                    Some(LRESULT(0))
                }
                WM_GETMINMAXINFO => {
                    let dpi = self.state.try_borrow().map(|s| s.dpi).unwrap_or(96);
                    let info = &mut *(lparam.0 as *mut MINMAXINFO);
                    let k = dpi as f32 / 96.0;
                    // The toolbar compacts down to MIN_BAR_SCALE of its full width.
                    let mut r = RECT {
                        left: 0,
                        top: 0,
                        right: layout_at(k * MIN_BAR_SCALE, 0).min_w,
                        bottom: (MIN_CLIENT_H * k).round() as i32,
                    };
                    let _ = AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0), dpi);
                    info.ptMinTrackSize = POINT {
                        x: rect_w(&r),
                        y: rect_h(&r),
                    };
                    Some(LRESULT(0))
                }
                WM_DPICHANGED => {
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        s.dpi = hiword(wparam.0);
                        s.fonts = None;
                    }
                    let r = &*(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        rect_w(r),
                        rect_h(r),
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    self.invalidate();
                    Some(LRESULT(0))
                }
                WM_SETTINGCHANGE => {
                    if setting_topic(lparam) == "ImmersiveColorSet" {
                        let dark = apps_use_dark_theme();
                        if let Ok(mut s) = self.state.try_borrow_mut() {
                            s.dark = dark;
                        }
                        set_dark_title_bar(self.hwnd, dark);
                        self.invalidate();
                    }
                    None
                }
                WM_SETCURSOR => {
                    if loword(lparam.0 as usize) != HTCLIENT {
                        return None;
                    }
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    let _ = ScreenToClient(self.hwnd, &mut p);
                    let id = self.state.try_borrow().map(|s| s.cursor_for(p)).unwrap_or(IDC_ARROW);
                    SetCursor(LoadCursorW(None, id).ok());
                    Some(LRESULT(1))
                }
                WM_LBUTTONDOWN => {
                    self.on_down(pt());
                    Some(LRESULT(0))
                }
                WM_LBUTTONDBLCLK => {
                    self.on_double_click(pt());
                    Some(LRESULT(0))
                }
                WM_CHAR => {
                    if self.on_char(wparam.0 as u16) {
                        Some(LRESULT(0))
                    } else {
                        None
                    }
                }
                WM_MOUSEMOVE => {
                    self.on_move(pt());
                    Some(LRESULT(0))
                }
                WM_LBUTTONUP => {
                    self.on_up(pt());
                    Some(LRESULT(0))
                }
                WM_CAPTURECHANGED => {
                    if HWND(lparam.0 as *mut c_void) != self.hwnd {
                        self.on_capture_lost();
                    }
                    Some(LRESULT(0))
                }
                WM_MOUSELEAVE => {
                    if let Ok(mut s) = self.state.try_borrow_mut() {
                        s.tracking = false;
                        s.hover = None;
                    }
                    self.invalidate();
                    Some(LRESULT(0))
                }
                WM_KEYDOWN => {
                    let vk = wparam.0 as u16;
                    if vk == VK_SHIFT.0 {
                        self.on_shift();
                        return Some(LRESULT(0));
                    }
                    let repeat = lparam.0 & (1 << 30) != 0;
                    if self.on_key(vk, repeat) {
                        Some(LRESULT(0))
                    } else {
                        None
                    }
                }
                WM_KEYUP => {
                    if wparam.0 as u16 == VK_SHIFT.0 {
                        self.on_shift();
                    }
                    None
                }
                WM_TIMER => {
                    match wparam.0 {
                        TIMER_TOAST => {
                            let expired = self
                                .state
                                .try_borrow_mut()
                                .map(|mut s| {
                                    let gone =
                                        s.toast.as_ref().map(|t| t.1.elapsed().as_millis() >= TOAST_MS).unwrap_or(true);
                                    if gone {
                                        s.toast = None;
                                    }
                                    gone
                                })
                                .unwrap_or(false);
                            if expired {
                                let _ = KillTimer(Some(self.hwnd), TIMER_TOAST);
                            }
                            self.invalidate();
                        }
                        TIMER_RETRY => {
                            let _ = KillTimer(Some(self.hwnd), TIMER_RETRY);
                            self.invalidate();
                        }
                        TIMER_FILE => self.poll_file(),
                        TIMER_CARET => {
                            if let Ok(mut s) = self.state.try_borrow_mut() {
                                s.caret_on = !s.caret_on;
                            }
                            self.invalidate();
                        }
                        _ => {}
                    }
                    Some(LRESULT(0))
                }
                WM_CLOSE => {
                    self.on_close();
                    Some(LRESULT(0))
                }
                WM_APP_EDITOR_SNAPSHOT => Some(LRESULT(self.snapshot())),
                WM_APP_EDITOR_IS_DIRTY => Some(LRESULT(self.is_dirty() as isize)),
                _ => None,
            }
        }
    }
}

/// The section name WM_SETTINGCHANGE carries in lparam (may be absent).
unsafe fn setting_topic(lparam: LPARAM) -> String {
    let p = lparam.0 as *const u16;
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while n < 64 && *p.add(n) != 0 {
        n += 1;
    }
    from_wide(std::slice::from_raw_parts(p, n))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Editor;
    if msg == WM_NCDESTROY {
        if !ptr.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    if msg == WM_APP_EDITOR_DESTROY {
        // Not inside Editor::handle: destroying frees the Editor. If one of its
        // dialogs is still open, the frame that opened it runs on afterwards.
        if !ptr.is_null() && (*ptr).modal.get() > 0 {
            (*ptr).pending_destroy.set(true);
        } else {
            let _ = DestroyWindow(hwnd);
        }
        return LRESULT(0);
    }
    if !ptr.is_null() {
        if let Some(r) = (*ptr).handle(msg, wparam, lparam) {
            return r;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn register_class(hinst: HINSTANCE) {
    if CLASS_REGISTERED.with(|c| c.replace(true)) {
        return;
    }
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // Double-clicks edit text.
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: crate::tray::load_app_icon(32).unwrap_or_default(),
            hIconSm: crate::tray::load_app_icon(16).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

/// Opens an editor window for one capture. On failure the request is handed
/// back so the caller can fall back to something else.
pub fn open(
    hinst: HINSTANCE,
    main_hwnd: HWND,
    req: EditRequest,
    prefs: Prefs,
) -> Result<HWND, (windows::core::Error, EditRequest)> {
    register_class(hinst);
    let mon = monitor_at(req.anchor);
    let dpi = monitor_dpi(mon.handle);
    let k = dpi as f32 / 96.0;
    let lay = layout(dpi, 0);
    let work_w = rect_w(&mon.work);
    let work_h = rect_h(&mon.work);
    let margin = (MARGIN * k).round() as i32;
    let want_w = req.image.width as i32 + 2 * margin;
    let want_h = req.image.height as i32 + 2 * margin + lay.bar_h;
    let client_w = want_w.min(work_w * 9 / 10).max(lay.min_w);
    let client_h = want_h.min(work_h * 9 / 10).max((MIN_CLIENT_H * k).round() as i32);
    let style = WS_OVERLAPPEDWINDOW;
    let mut r = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    unsafe {
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, WINDOW_EX_STYLE(0), dpi);
    }
    let w = rect_w(&r).min(work_w);
    let h = rect_h(&r).min(work_h);
    let x = mon.work.left + (work_w - w) / 2;
    let y = mon.work.top + (work_h - h) / 2;
    let title = title_for(req.file.path());
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            &HSTRING::from(title),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => return Err((e, req)),
    };
    let mut cr = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut cr);
    }
    let target = match Target::new(hwnd, &req.image, rect_w(&cr).max(1), rect_h(&cr).max(1), false) {
        Ok(t) => t,
        Err(e) => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err((e, req));
        }
    };
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => dpi,
        d => d,
    };
    let dark = apps_use_dark_theme();
    set_dark_title_bar(hwnd, dark);
    let tooltip = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            TOOLTIPS_CLASSW,
            PCWSTR::null(),
            WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(hwnd),
            None,
            Some(hinst),
            None,
        )
    }
    .unwrap_or_default();
    let saving = matches!(req.file, PendingFile::Saving(_));
    let initial = Prefs {
        tool: prefs.tool,
        color: prefs.color & 0xFF_FFFF,
        width_pt: prefs.width_pt.clamp(1, 50),
    };
    let mut state = State {
        image: req.image,
        file: req.file,
        capture_scale: req.capture_scale.max(0.5),
        doc: Doc::default(),
        selected: None,
        tool: initial.tool,
        color: initial.color,
        width_pt: initial.width_pt,
        initial,
        drag: Drag::Idle,
        editing: None,
        caret_on: true,
        high_surrogate: None,
        mosaic: None,
        nudging: false,
        last_mouse: POINT::default(),
        hover: None,
        tracking: false,
        dpi,
        client: (rect_w(&cr).max(1), rect_h(&cr).max(1)),
        lay: Layout::default(),
        view_scale: 1.0,
        view_off: (0.0, 0.0),
        target: Some(target),
        target_failures: 0,
        fonts: None,
        dark,
        toast: None,
        tooltip,
        tooltips_added: 0,
        closed: false,
    };
    state.relayout();
    log(&format!(
        "editor: {}x{} capture shown at {:.0}% in a {}x{} window",
        state.image.width,
        state.image.height,
        state.view_scale * 100.0,
        w,
        h
    ));
    let editor = Box::into_raw(Box::new(Editor {
        hwnd,
        main_hwnd,
        state: RefCell::new(state),
        modal: Cell::new(0),
        pending_destroy: Cell::new(false),
    }));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, editor as isize);
        (*editor).sync_tooltips();
        if saving {
            SetTimer(Some(hwnd), TIMER_FILE, 100, None);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(hwnd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_presets_give_text_sizes() {
        for (i, &w) in WIDTHS_PT.iter().enumerate() {
            assert_eq!(text_pt(w), TEXT_PT[i]);
        }
        assert_eq!(text_pt(1), 14.0);
        assert_eq!(text_pt(3), 17.0);
        assert_eq!(text_pt(24), 80.0);
    }

    #[test]
    fn utf16_offsets_map_to_bytes() {
        let s = "a\u{e9}\u{1F600}b";
        assert_eq!(utf16_len(s), 5);
        assert_eq!(byte_at_utf16(s, 0), 0);
        assert_eq!(byte_at_utf16(s, 1), 1);
        assert_eq!(byte_at_utf16(s, 2), 3);
        assert_eq!(byte_at_utf16(s, 4), 7);
        assert_eq!(byte_at_utf16(s, 5), 8);
        assert_eq!(byte_at_utf16(s, 99), 8);
    }

    #[test]
    fn caret_moves_and_edits() {
        let mut t = String::from("ab\ncd\u{e9}");
        let mut c = caret::left(&t, t.len());
        assert_eq!(&t[c..], "\u{e9}");
        c = caret::home(&t, c);
        assert_eq!(c, 3);
        assert_eq!(caret::left(&t, c), 2, "back over the newline");
        assert_eq!(caret::end(&t, 0), 2);
        assert_eq!(caret::end(&t, 3), t.len());
        let end = t.len();
        c = caret::backspace(&mut t, end);
        assert_eq!((t.as_str(), c), ("ab\ncd", 5));
        c = caret::insert(&mut t, 2, "\u{20ac}");
        assert_eq!((t.as_str(), c), ("ab\u{20ac}\ncd", 5));
        c = caret::delete(&mut t, c);
        assert_eq!((t.as_str(), c), ("ab\u{20ac}cd", 5));
        assert_eq!(caret::backspace(&mut t, 0), 0);
        assert_eq!(caret::right(&t, t.len()), t.len());
        assert_eq!(caret::delete(&mut t, 7), 7);
        assert_eq!(t, "ab\u{20ac}cd");
    }
}
