//! SgCap: macOS-style screen capture for Windows.
//!
//! Resident tray application. A global hotkey freezes the screen and shows a
//! selection overlay; the capture is saved as PNG immediately and shown as a
//! draggable thumbnail in the corner that fades out after a delay.

#![windows_subsystem = "windows"]

mod capture;
mod dragdrop;
mod dxgi_capture;
mod hotkey;
mod overlay;
mod settings;
mod settings_ui;
mod startup;
mod thumb_image;
mod thumbnail;
mod tray;
mod util;

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_HOTKEY_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Shell::{ShellExecuteW, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, IsDialogMessageW, IsWindow,
    KillTimer, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetTimer,
    ShowWindow, TranslateMessage, HWND_BROADCAST, MB_ICONERROR, MB_OK, MSG, SW_HIDE, SW_SHOWNOACTIVATE,
    SW_SHOWNORMAL, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_HOTKEY,
    WM_LBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};

use capture::{cursor_pos, monitor_at, Image};
use hotkey::HotKey;
use overlay::{CaptureResult, Overlay, WM_APP_CAPTURED};
use settings::Settings;
use settings_ui::{WM_APP_SETTINGS_CHANGED, WM_APP_SETTINGS_CLOSED};
use thumb_image::ThumbImage;
use thumbnail::{FileResult, ThumbParams, WM_APP_THUMB_CLOSED, WM_APP_THUMB_MOVE};
use tray::{Tray, CMD_CAPTURE_REGION, CMD_CAPTURE_SCREEN, CMD_OPEN_FOLDER, CMD_QUIT, CMD_SETTINGS, WM_TRAY};
use util::{log, loword};

const WM_APP_THUMB_READY: u32 = WM_APP + 3;
const HOTKEY_REGION: i32 = 1;
const HOTKEY_SCREEN: i32 = 2;
const TIMER_DELAYED_REGION: usize = 1;
const TIMER_DELAYED_SCREEN: usize = 2;
const CLASS_NAME: PCWSTR = w!("SgCapMain");

struct ThumbEntry {
    hwnd: HWND,
    content_h: i32,
    work: RECT,
    scale: f32,
}

/// Posted by the worker thread once the thumbnail bitmap is ready.
struct ThumbReady {
    image: Arc<Image>,
    thumb: ThumbImage,
    work: RECT,
    dpi: u32,
    seconds: u32,
    rx: Receiver<FileResult>,
}

struct App {
    hwnd: HWND,
    hinst: HINSTANCE,
    settings: Settings,
    tray: Option<Tray>,
    overlay: Option<&'static Overlay>,
    thumbs: Vec<ThumbEntry>,
    next_id: u64,
    taskbar_created: u32,
    show_settings_msg: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => {
            log("app: re-entrant access skipped");
            None
        }
    })
}

fn main() {
    util::init_log();
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("panic: {info}");
        log(&msg);
        unsafe {
            MessageBoxW(None, &HSTRING::from(msg), w!("SgCap crashed"), MB_OK | MB_ICONERROR);
        }
    }));
    if let Err(e) = run() {
        let msg = format!("SgCap could not start: {e}");
        log(&msg);
        unsafe {
            MessageBoxW(None, &HSTRING::from(msg), w!("SgCap"), MB_OK | MB_ICONERROR);
        }
    }
}

fn run() -> windows::core::Result<()> {
    unsafe {
        let _mutex = CreateMutexW(None, false, w!("Local\\SgCap.SingleInstance"))?;
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
        let show_settings_msg = RegisterWindowMessageW(w!("SgCap.ShowSettings"));
        if already_running {
            let _ = PostMessageW(Some(HWND_BROADCAST), show_settings_msg, WPARAM(0), LPARAM(0));
            return Ok(());
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        OleInitialize(None)?;
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_HOTKEY_CLASS | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let hinst = HINSTANCE(GetModuleHandleW(None)?.0);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!("SgCap"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )?;

        let settings = Settings::load();
        APP.with(|cell| {
            *cell.borrow_mut() = Some(App {
                hwnd,
                hinst,
                settings,
                tray: None,
                overlay: None,
                thumbs: Vec::new(),
                next_id: 0,
                taskbar_created,
                show_settings_msg,
            });
        });

        let overlay = Overlay::create(hinst, hwnd)?;
        let tray = Tray::new(hwnd)?;
        with_app(|a| {
            a.overlay = Some(overlay);
            a.tray = Some(tray);
        });
        apply_settings(true);
        dxgi_capture::init();
        log("ready");

        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break;
            }
            let s = settings_ui::current_hwnd();
            if !s.is_invalid() {
                if settings_ui::pre_translate(s, &msg) {
                    continue;
                }
                if IsDialogMessageW(s, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Shutdown: drop the tray icon, hotkeys and any thumbnails.
        let thumbs: Vec<HWND> = with_app(|a| a.thumbs.iter().map(|t| t.hwnd).collect()).unwrap_or_default();
        for t in thumbs {
            let _ = DestroyWindow(t);
        }
        HotKey::unregister(hwnd, HOTKEY_REGION);
        HotKey::unregister(hwnd, HOTKEY_SCREEN);
        APP.with(|cell| {
            let _ = cell.borrow_mut().take();
        });
        OleUninitialize();
    }
    Ok(())
}

/// Registers the hotkeys and syncs the startup entry from the current settings.
fn apply_settings(startup_phase: bool) {
    let Some((hwnd, s)) = with_app(|a| (a.hwnd, a.settings.clone())) else {
        return;
    };
    HotKey::unregister(hwnd, HOTKEY_REGION);
    HotKey::unregister(hwnd, HOTKEY_SCREEN);
    let mut problems = Vec::new();
    match HotKey::parse(&s.region_hotkey) {
        Some(hk) => {
            if hk.register(hwnd, HOTKEY_REGION).is_err() {
                problems.push(format!("{} is already used by another program.", hk.display()));
            }
        }
        None => problems.push(format!("Cannot understand the region hotkey \"{}\".", s.region_hotkey)),
    }
    if !s.screen_hotkey.trim().is_empty() {
        match HotKey::parse(&s.screen_hotkey) {
            Some(hk) => {
                if hk.register(hwnd, HOTKEY_SCREEN).is_err() {
                    problems.push(format!("{} is already used by another program.", hk.display()));
                }
            }
            None => problems.push(format!("Cannot understand the screen hotkey \"{}\".", s.screen_hotkey)),
        }
    }
    let startup_result = if s.start_with_windows {
        startup::set_enabled(true)
    } else if startup::is_enabled() {
        startup::set_enabled(false)
    } else {
        Ok(())
    };
    if let Err(e) = startup_result {
        log(&format!("startup registration failed: {e}"));
    }
    if !problems.is_empty() {
        let text = format!("{} Change it in Settings.", problems.join(" "));
        log(&text);
        with_app(|a| {
            if let Some(t) = &a.tray {
                t.balloon("SgCap hotkey problem", &text, true);
            }
        });
        if startup_phase {
            open_settings();
        }
    }
}

fn open_settings() {
    let Some((hinst, hwnd, s)) = with_app(|a| (a.hinst, a.hwnd, a.settings.clone())) else {
        return;
    };
    if let Err(e) = settings_ui::open(hinst, hwnd, &s) {
        log(&format!("settings window failed: {e}"));
    }
}

/// Layered windows cannot opt out of screen capture, so existing thumbnails are
/// hidden for the duration of the grab (two DWM frames) and shown again after.
fn with_thumbs_hidden(f: impl FnOnce()) {
    let thumbs: Vec<HWND> = with_app(|a| a.thumbs.iter().map(|t| t.hwnd).collect()).unwrap_or_default();
    unsafe {
        if !thumbs.is_empty() {
            for h in &thumbs {
                let _ = ShowWindow(*h, SW_HIDE);
            }
            let _ = DwmFlush();
            let _ = DwmFlush();
        }
        f();
        for h in &thumbs {
            if IsWindow(Some(*h)).as_bool() {
                let _ = ShowWindow(*h, SW_SHOWNOACTIVATE);
            }
        }
    }
}

fn begin_region() {
    let overlay = with_app(|a| a.overlay).flatten();
    if let Some(ov) = overlay {
        if !ov.is_active() {
            with_thumbs_hidden(|| ov.begin());
        }
    }
}

fn capture_screen() {
    let active = with_app(|a| a.overlay.map(|o| o.is_active()).unwrap_or(false)).unwrap_or(false);
    if active {
        return;
    }
    let mon = monitor_at(cursor_pos());
    let mut grabbed = None;
    with_thumbs_hidden(|| match capture::grab(mon.rect) {
        Ok(dib) => grabbed = Some(capture::whole(&dib)),
        Err(e) => log(&format!("screen capture failed: {e}")),
    });
    if let Some(image) = grabbed {
        process_capture(image);
    }
}

fn monitor_dpi(handle: windows::Win32::Graphics::Gdi::HMONITOR) -> u32 {
    let mut dx = 96u32;
    let mut dy = 96u32;
    unsafe {
        if GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_err() || dx == 0 {
            return 96;
        }
    }
    dx
}

fn save_png(img: &Image, folder: &Path) -> FileResult {
    let bytes = capture::encode_png(img)?;
    std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let base = util::timestamp_name();
    let mut path = folder.join(format!("{base}.png"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{base} ({n}).png"));
        n += 1;
    }
    std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((path, Arc::new(bytes)))
}

/// Common tail of every capture: clipboard, worker thread for thumbnail + PNG.
fn process_capture(image: Image) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    let mon = monitor_at(cursor_pos());
    let dpi = monitor_dpi(mon.handle);
    let scale = dpi as f32 / 96.0;
    let Some((folder, seconds, copy, hwnd)) = with_app(|a| {
        (
            a.settings.resolved_save_folder(),
            a.settings.thumbnail_seconds,
            a.settings.copy_to_clipboard,
            a.hwnd,
        )
    }) else {
        return;
    };
    if copy {
        if let Err(e) = dragdrop::copy_to_clipboard(hwnd, &image, None) {
            log(&format!("clipboard copy failed: {e}"));
        }
    }
    let image = Arc::new(image);
    let (tx, rx) = mpsc::channel::<FileResult>();
    let main_raw = hwnd.0 as isize;
    let work = mon.work;
    let max_w = (320.0 * scale) as i32;
    let max_h = (220.0 * scale) as i32;
    std::thread::spawn(move || {
        let thumb = thumb_image::build(&image, max_w, max_h, scale);
        if let Some(dir) = std::env::var_os("SGCAP_DUMP_THUMB") {
            if let Ok(bytes) = capture::encode_png_rgba(thumb.w as u32, thumb.h as u32, &thumb.bgra) {
                let _ = std::fs::write(Path::new(&dir).join("thumb_debug.png"), bytes);
            }
        }
        let ready = Box::new(ThumbReady {
            image: image.clone(),
            thumb,
            work,
            dpi,
            seconds,
            rx,
        });
        unsafe {
            let ok = PostMessageW(
                Some(HWND(main_raw as *mut c_void)),
                WM_APP_THUMB_READY,
                WPARAM(0),
                LPARAM(Box::into_raw(ready) as isize),
            )
            .is_ok();
            if !ok {
                log("worker: PostMessage failed");
            }
        }
        let result = save_png(&image, &folder);
        if let Err(e) = &result {
            log(&format!("save failed: {e}"));
        }
        let _ = tx.send(result);
    });
}

fn on_thumb_ready(r: ThumbReady) {
    let Some((hinst, main_hwnd, id)) = with_app(|a| {
        a.next_id += 1;
        (a.hinst, a.hwnd, a.next_id)
    }) else {
        return;
    };
    let scale = r.dpi as f32 / 96.0;
    let margin = (16.0 * scale).round() as i32;
    let bottom = r.work.bottom - margin;
    let content_h = r.thumb.content_h;
    let work = r.work;
    let params = ThumbParams {
        id,
        image: r.image,
        thumb: r.thumb,
        work: r.work,
        dpi: r.dpi,
        seconds: r.seconds,
        file_rx: r.rx,
    };
    match thumbnail::create(hinst, main_hwnd, params, bottom) {
        Ok(hwnd) => {
            with_app(|a| {
                a.thumbs.insert(
                    0,
                    ThumbEntry {
                        hwnd,
                        content_h,
                        work,
                        scale,
                    },
                );
                relayout(a);
            });
        }
        Err(e) => log(&format!("thumbnail window failed: {e}")),
    }
}

fn same_rect(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// Newest thumbnail at the bottom, older ones stacked above it (per monitor).
fn relayout(a: &App) {
    let n = a.thumbs.len();
    for i in 0..n {
        let t = &a.thumbs[i];
        let gap = (10.0 * t.scale).round() as i32;
        let margin = (16.0 * t.scale).round() as i32;
        let mut acc = 0;
        for j in 0..i {
            let o = &a.thumbs[j];
            if same_rect(&o.work, &t.work) {
                acc += o.content_h + gap;
            }
        }
        let bottom = t.work.bottom - margin - acc;
        unsafe {
            let _ = PostMessageW(Some(t.hwnd), WM_APP_THUMB_MOVE, WPARAM(0), LPARAM(bottom as isize));
        }
    }
}

fn on_thumb_closed(hwnd: HWND) {
    with_app(|a| {
        a.thumbs.retain(|t| t.hwnd != hwnd);
        relayout(a);
    });
}

fn open_save_folder() {
    let Some(folder) = with_app(|a| a.settings.resolved_save_folder()) else {
        return;
    };
    let _ = std::fs::create_dir_all(&folder);
    let w = util::wide(&folder.to_string_lossy());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(w.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn on_command(hwnd: HWND, cmd: u32) {
    unsafe {
        match cmd {
            CMD_CAPTURE_REGION => {
                SetTimer(Some(hwnd), TIMER_DELAYED_REGION, 300, None);
            }
            CMD_CAPTURE_SCREEN => {
                SetTimer(Some(hwnd), TIMER_DELAYED_SCREEN, 300, None);
            }
            CMD_SETTINGS => open_settings(),
            CMD_OPEN_FOLDER => open_save_folder(),
            CMD_QUIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

fn on_tray(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let event = loword(lparam.0 as usize);
    let x = (wparam.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    unsafe {
        match event {
            WM_LBUTTONUP | NIN_SELECT => {
                SetTimer(Some(hwnd), TIMER_DELAYED_REGION, 250, None);
            }
            e if e == NIN_SELECT + 1 => {
                SetTimer(Some(hwnd), TIMER_DELAYED_REGION, 250, None);
            }
            WM_CONTEXTMENU => {
                let cmd = Tray::show_menu(hwnd, x, y);
                on_command(hwnd, cmd);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            match wparam.0 as i32 {
                HOTKEY_REGION => begin_region(),
                HOTKEY_SCREEN => capture_screen(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = KillTimer(Some(hwnd), wparam.0);
            match wparam.0 {
                TIMER_DELAYED_REGION => begin_region(),
                TIMER_DELAYED_SCREEN => capture_screen(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_CAPTURED => {
            let result = Box::from_raw(lparam.0 as *mut CaptureResult);
            process_capture(result.image);
            LRESULT(0)
        }
        WM_APP_THUMB_READY => {
            let ready = Box::from_raw(lparam.0 as *mut ThumbReady);
            on_thumb_ready(*ready);
            LRESULT(0)
        }
        WM_APP_THUMB_CLOSED => {
            on_thumb_closed(HWND(wparam.0 as *mut c_void));
            LRESULT(0)
        }
        WM_APP_SETTINGS_CHANGED => {
            with_app(|a| a.settings = Settings::load());
            apply_settings(false);
            LRESULT(0)
        }
        WM_APP_SETTINGS_CLOSED => LRESULT(0),
        WM_TRAY => {
            on_tray(hwnd, wparam, lparam);
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, loword(wparam.0));
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            // Monitor layout changed: rebuild the duplication objects.
            dxgi_capture::reset();
            dxgi_capture::init();
            LRESULT(0)
        }
        _ => {
            let (taskbar_created, show_settings) =
                with_app(|a| (a.taskbar_created, a.show_settings_msg)).unwrap_or((0, 0));
            if msg != 0 && msg == taskbar_created {
                with_app(|a| {
                    if let Some(t) = &a.tray {
                        if let Err(e) = t.add() {
                            log(&format!("tray re-add failed: {e}"));
                        }
                    }
                });
                return LRESULT(0);
            }
            if msg != 0 && msg == show_settings {
                open_settings();
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
