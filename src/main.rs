//! SgCap: macOS-style screen capture for Windows.
//!
//! Resident tray application. A global hotkey freezes the screen and shows a
//! selection overlay; the capture is saved as PNG immediately and shown as a
//! draggable thumbnail in the corner that fades out after a delay.

#![windows_subsystem = "windows"]

mod capture;
mod dragdrop;
mod d2d;
mod dxgi_capture;
mod editor;
mod hotkey;
mod overlay;
mod settings;
mod settings_ui;
mod shapes;
mod startup;
mod thumb_image;
mod thumbnail;
mod tray;
mod util;

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::io::Write;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_HOTKEY_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::{ShellExecuteW, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, IsDialogMessageW, IsWindow,
    IsWindowVisible, KillTimer, MessageBoxW, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SendMessageW, SetForegroundWindow, SetTimer, ShowWindow, TranslateMessage,
    GetLastActivePopup, HWND_BROADCAST, IDOK, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_OKCANCEL, MSG, PM_REMOVE, SW_HIDE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONUP, WM_TIMER, WNDCLASSEXW,
    WS_OVERLAPPED,
};

use capture::{cursor_pos, monitor_at, monitor_dpi, Image, MonitorInfo};
use editor::{
    EditRequest, EditorOutcome, Outcome, PendingFile, Prefs, Snapshot, WM_APP_EDITOR_CLOSED, WM_APP_EDITOR_IS_DIRTY,
    WM_APP_EDITOR_SNAPSHOT, WM_APP_EDIT_REQUEST,
};
use hotkey::HotKey;
use overlay::{CaptureResult, Overlay, WM_APP_CAPTURED};
use settings::Settings;
use settings_ui::{WM_APP_SETTINGS_CHANGED, WM_APP_SETTINGS_CLOSED};
use thumb_image::ThumbImage;
use thumbnail::{FileResult, ThumbParams, WM_APP_THUMB_CLOSED, WM_APP_THUMB_MOVE, WM_APP_THUMB_UNSAVED};
use tray::{Tray, CMD_CAPTURE_REGION, CMD_CAPTURE_SCREEN, CMD_OPEN_FOLDER, CMD_QUIT, CMD_SETTINGS, WM_TRAY};
use util::{log, loword};

const WM_APP_THUMB_READY: u32 = WM_APP + 3;
/// Worker -> main window when a PNG could not be written: lparam = Box<String>.
const WM_APP_SAVE_FAILED: u32 = WM_APP + 12;
/// Quit once the messages already queued (editor results) are handled.
const WM_APP_QUIT: u32 = WM_APP + 13;
/// How long shutdown waits for PNG writes still in progress.
const SHUTDOWN_SAVE_WAIT: Duration = Duration::from_secs(10);
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
    /// Open annotation editors.
    editors: Vec<HWND>,
    /// Threads still writing PNGs: waited for before the process exits. Each
    /// returns an error message it could not report itself.
    workers: Vec<JoinHandle<Option<String>>>,
    /// False when settings.json exists but could not be read: the defaults in
    /// use are not the user's choice, so they must not change the system.
    settings_ok: bool,
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
        let mutex = CreateMutexW(None, false, w!("Local\\SgCap.SingleInstance"))?;
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

        let (settings, settings_ok) = match Settings::try_load() {
            Ok(s) => (s, true),
            Err(e) => {
                log(&format!("settings: {e}, running with defaults"));
                // Defaults, but showing the system's real autostart state so
                // the settings window cannot switch it off by accident.
                let defaults = Settings {
                    start_with_windows: startup::is_enabled(),
                    ..Settings::default()
                };
                (defaults, false)
            }
        };
        APP.with(|cell| {
            *cell.borrow_mut() = Some(App {
                hwnd,
                hinst,
                settings,
                tray: None,
                overlay: None,
                thumbs: Vec::new(),
                editors: Vec::new(),
                workers: Vec::new(),
                settings_ok,
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
        if !settings_ok {
            with_app(|a| {
                if let Some(t) = &a.tray {
                    t.balloon(
                        "SgCap could not read its settings",
                        "It runs with the defaults until you save the settings again.",
                        true,
                    );
                }
            });
        }
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

        // Shutdown: thumbnails, hotkeys, tray icon and the single-instance
        // mutex go first (a relaunch now starts normally), then PNG writes
        // still in progress get to finish.
        let thumbs: Vec<HWND> = with_app(|a| a.thumbs.iter().map(|t| t.hwnd).collect()).unwrap_or_default();
        for t in thumbs {
            let _ = DestroyWindow(t);
        }
        HotKey::unregister(hwnd, HOTKEY_REGION);
        HotKey::unregister(hwnd, HOTKEY_SCREEN);
        with_app(|a| a.tray = None);
        let _ = CloseHandle(mutex);
        let failures = wait_for_saves();
        if !failures.is_empty() {
            let text = format!("Some screenshots could not be saved:\n\n{}", failures.join("\n"));
            MessageBoxW(None, &HSTRING::from(text), w!("SgCap"), MB_OK | MB_ICONWARNING);
        }
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
    log(&format!(
        "settings: region \"{}\", screen \"{}\", thumbnail {} s, folder \"{}\", clipboard {}, startup {}",
        s.region_hotkey,
        s.screen_hotkey,
        s.thumbnail_seconds,
        s.resolved_save_folder().display(),
        s.copy_to_clipboard,
        s.start_with_windows
    ));
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
    let settings_ok = with_app(|a| a.settings_ok).unwrap_or(false);
    let startup_result = if !settings_ok {
        // Defaults standing in for an unreadable file: leave the Run key alone.
        Ok(())
    } else if s.start_with_windows {
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
    let t0 = std::time::Instant::now();
    let mut grabbed = None;
    with_thumbs_hidden(|| match capture::grab_fast(mon.rect) {
        Ok(dib) => grabbed = Some(capture::whole(&dib)),
        Err(e) => log(&format!("screen capture failed: {e}")),
    });
    log(&format!(
        "screen: {}x{} grab {} ms",
        util::rect_w(&mon.rect),
        util::rect_h(&mon.rect),
        t0.elapsed().as_millis()
    ));
    if let Some(image) = grabbed {
        process_capture(image);
    }
}

/// First free path among "<base>.png", "<base> (2).png", ...
fn unique_path(folder: &Path, base: &str) -> PathBuf {
    let mut path = folder.join(format!("{base}.png"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{base} ({n}).png"));
        n += 1;
    }
    path
}

/// Writes a whole file and flushes it to disk.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Writes `bytes` as a new "<base>.png" (or "<base> (n).png") in `folder`,
/// through a flushed temporary file, so it is never left half-written.
fn write_new(folder: &Path, base: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let path = unique_path(folder, base);
    let tmp = path.with_extension("png.tmp");
    write_synced(&tmp, bytes)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot write {}: {e}", path.display())
        })?;
    Ok(path)
}

/// Tries each folder in turn (the save folder, then the Desktop when it differs).
fn write_new_somewhere(folders: &[PathBuf], base: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let mut last = String::from("no folder to save to");
    for folder in folders {
        match write_new(folder, base, bytes) {
            Ok(p) => return Ok(p),
            Err(e) => {
                log(&format!("save: {e}"));
                last = e;
            }
        }
    }
    Err(last)
}

fn fallback_folders(first: &Path) -> Vec<PathBuf> {
    let mut v = vec![first.to_path_buf()];
    let desktop = settings::desktop_dir();
    if desktop != first {
        v.push(desktop);
    }
    v
}

fn save_png(img: &Image, folder: &Path) -> FileResult {
    let bytes = capture::encode_png(img)?;
    let path = write_new_somewhere(&fallback_folders(folder), &util::timestamp_name(), &bytes)?;
    Ok((path, Arc::new(bytes)))
}

/// Replaces `path` with the annotated image: written next to it and flushed,
/// then renamed over it. If that fails (the file is held open, the folder is
/// gone or read-only), the image goes to "<name> (annotated).png" in the same
/// folder, then the save folder, then the Desktop.
fn replace_png(img: &Image, path: &Path, save_folder: &Path) -> FileResult {
    let bytes = capture::encode_png(img)?;
    let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let tmp = path.with_extension("png.tmp");
    let replaced = std::fs::create_dir_all(&folder)
        .and_then(|_| write_synced(&tmp, &bytes))
        .and_then(|_| std::fs::rename(&tmp, path));
    match replaced {
        Ok(()) => Ok((path.to_path_buf(), Arc::new(bytes))),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            log(&format!("cannot replace {}: {e}", path.display()));
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mut folders = vec![folder];
            folders.extend(fallback_folders(save_folder));
            folders.dedup();
            let alt = write_new_somewhere(&folders, &format!("{stem} (annotated)"), &bytes)?;
            log(&format!("saved the annotated capture as {}", alt.display()));
            Ok((alt, Arc::new(bytes)))
        }
    }
}

/// Waits (bounded) for the threads still writing PNGs; returns the errors
/// nobody has been told about yet.
fn wait_for_saves() -> Vec<String> {
    let workers = with_app(|a| std::mem::take(&mut a.workers)).unwrap_or_default();
    let t0 = Instant::now();
    let mut failures = Vec::new();
    for w in workers {
        while !w.is_finished() && t0.elapsed() < SHUTDOWN_SAVE_WAIT {
            std::thread::sleep(Duration::from_millis(20));
        }
        if w.is_finished() {
            if let Ok(Some(e)) = w.join() {
                failures.push(e);
            }
        } else {
            log("shutdown: a PNG write is still running, not waiting any longer");
            failures.push("A screenshot was still being saved and may be missing.".to_string());
        }
    }
    failures
}

/// Images held only by thumbnails whose save failed.
fn unsaved_thumbnail_images() -> Vec<Arc<Image>> {
    let thumbs: Vec<HWND> = with_app(|a| a.thumbs.iter().map(|t| t.hwnd).collect()).unwrap_or_default();
    let mut images = Vec::new();
    for t in thumbs {
        let raw = unsafe {
            if !IsWindow(Some(t)).as_bool() {
                continue;
            }
            SendMessageW(t, WM_APP_THUMB_UNSAVED, None, None).0
        };
        if raw != 0 {
            images.push(*unsafe { Box::from_raw(raw as *mut Arc<Image>) });
        }
    }
    images
}

/// Last attempt for images only a thumbnail held: written as new files (the
/// Desktop first), on workers covered by wait_for_saves.
fn save_unsaved_thumbnails() {
    let Some(folder) = with_app(|a| a.settings.resolved_save_folder()) else {
        return;
    };
    for image in unsaved_thumbnail_images() {
        let mut folders = vec![settings::desktop_dir(), folder.clone()];
        folders.dedup();
        let worker = std::thread::spawn(move || {
            match capture::encode_png(&image).and_then(|bytes| write_new_somewhere(&folders, &util::timestamp_name(), &bytes)) {
                Ok(p) => {
                    log(&format!("saved an unsaved capture as {}", p.display()));
                    None
                }
                Err(e) => Some(e),
            }
        });
        with_app(|a| a.workers.push(worker));
    }
}

/// Where the image behind a new thumbnail ends up on disk.
enum SaveTarget {
    /// A new timestamped file in the save folder.
    NewFile,
    /// Overwrite this file (Done in the editor).
    Replace(PathBuf),
    /// Already saved: the thumbnail just shows it again.
    Existing(PathBuf, Arc<Vec<u8>>),
    /// The capture's own save was still running when the editor closed:
    /// wait for it here (on the worker), then replace it if `edited`.
    AfterSave(Receiver<FileResult>, bool),
}

/// Common tail of every capture: clipboard, worker thread for thumbnail + PNG.
fn process_capture(image: Image) {
    if image.width == 0 || image.height == 0 {
        return;
    }
    copy_if_enabled(&image);
    show_thumbnail(Arc::new(image), SaveTarget::NewFile, monitor_at(cursor_pos()));
}

fn copy_if_enabled(image: &Image) {
    let Some((copy, hwnd)) = with_app(|a| (a.settings.copy_to_clipboard, a.hwnd)) else {
        return;
    };
    if copy {
        if let Err(e) = dragdrop::copy_to_clipboard(hwnd, image, None) {
            log(&format!("clipboard copy failed: {e}"));
        }
    }
}

/// Builds the thumbnail on a worker thread (posted back as WM_APP_THUMB_READY),
/// then puts the PNG on disk as `target` says.
fn show_thumbnail(image: Arc<Image>, target: SaveTarget, mon: MonitorInfo) {
    let dpi = monitor_dpi(mon.handle);
    let scale = dpi as f32 / 96.0;
    let Some((folder, seconds, hwnd)) =
        with_app(|a| (a.settings.resolved_save_folder(), a.settings.thumbnail_seconds, a.hwnd))
    else {
        return;
    };
    let (tx, rx) = mpsc::channel::<FileResult>();
    let main_raw = hwnd.0 as isize;
    let work = mon.work;
    let max_w = (320.0 * scale) as i32;
    let max_h = (220.0 * scale) as i32;
    let worker = std::thread::spawn(move || {
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
        let result = match target {
            SaveTarget::NewFile => save_png(&image, &folder),
            SaveTarget::Replace(path) => replace_png(&image, &path, &folder),
            SaveTarget::Existing(path, png) => Ok((path, png)),
            SaveTarget::AfterSave(rx, edited) => match rx.recv() {
                Ok(Ok((path, _))) if edited => replace_png(&image, &path, &folder),
                Ok(Ok(file)) => Ok(file),
                _ => save_png(&image, &folder),
            },
        };
        let mut unreported = None;
        if let Err(e) = &result {
            log(&format!("save failed: {e}"));
            // Tell the user: otherwise the image only lives in the thumbnail.
            let msg = Box::into_raw(Box::new(e.clone()));
            unsafe {
                if PostMessageW(Some(HWND(main_raw as *mut c_void)), WM_APP_SAVE_FAILED, WPARAM(0), LPARAM(msg as isize))
                    .is_err()
                {
                    // The app is closing: the shutdown reports it instead.
                    drop(Box::from_raw(msg));
                    unreported = Some(e.clone());
                }
            }
        }
        let _ = tx.send(result);
        unreported
    });
    with_app(|a| {
        a.workers.retain(|w| !w.is_finished());
        a.workers.push(worker);
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

fn open_editor(req: EditRequest) {
    let Some((hinst, hwnd, s)) = with_app(|a| (a.hinst, a.hwnd, a.settings.clone())) else {
        return;
    };
    let prefs = Prefs {
        tool: shapes::Kind::from_name(&s.annotate_tool).unwrap_or(shapes::Kind::Arrow),
        color: s.annotate_color,
        width_pt: s.annotate_width,
    };
    match editor::open(hinst, hwnd, req, prefs) {
        Ok(ed) => {
            with_app(|a| a.editors.push(ed));
        }
        Err((e, req)) => {
            // Without an editor: open the file if it is there, and bring the
            // thumbnail back (never waiting for a save on this thread).
            log(&format!("editor failed to open: {e}"));
            let target = match req.file {
                PendingFile::Ready(Some((path, png))) => {
                    let w = util::wide(&path.to_string_lossy());
                    unsafe {
                        ShellExecuteW(None, w!("open"), PCWSTR(w.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                    }
                    SaveTarget::Existing(path, png)
                }
                PendingFile::Ready(None) => SaveTarget::NewFile,
                PendingFile::Saving(rx) => SaveTarget::AfterSave(rx, false),
            };
            show_thumbnail(req.image, target, monitor_at(req.anchor));
        }
    }
}

/// Remembers the tool, colour and width the user changed in an editor. Only
/// changed fields are written (another editor may have changed the others),
/// into the settings as they are on disk, and never over a file that could
/// not be read.
fn remember_style(initial: Prefs, now: Prefs) {
    if initial == now {
        return;
    }
    let apply = |s: &mut Settings| {
        if now.tool != initial.tool {
            s.annotate_tool = now.tool.name().to_string();
        }
        if now.color != initial.color {
            s.annotate_color = now.color;
        }
        if now.width_pt != initial.width_pt {
            s.annotate_width = now.width_pt;
        }
    };
    with_app(|a| apply(&mut a.settings));
    match Settings::try_load() {
        Ok(mut disk) => {
            apply(&mut disk);
            if let Err(e) = disk.save() {
                log(&format!("settings: cannot save the annotation style: {e}"));
            }
        }
        Err(e) => log(&format!("settings: not saving the annotation style, the file is unreadable: {e}")),
    }
}

fn editor_on_screen(e: HWND) -> bool {
    unsafe { IsWindow(Some(e)).as_bool() && IsWindowVisible(e).as_bool() }
}

fn any_editor_on_screen() -> bool {
    with_app(|a| a.editors.clone()).unwrap_or_default().into_iter().any(editor_on_screen)
}

/// Tray > Quit (or WM_CLOSE): let each open editor ask about unsaved
/// annotations first. Cancel in any of those prompts calls the quit off.
fn request_quit(hwnd: HWND) {
    let editors: Vec<HWND> = with_app(|a| a.editors.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|&e| editor_on_screen(e))
        .collect();
    unsafe {
        // Editors with annotations first, so a Cancel stops the quit before
        // anything else has been closed.
        let (dirty, clean): (Vec<HWND>, Vec<HWND>) =
            editors.into_iter().partition(|&e| SendMessageW(e, WM_APP_EDITOR_IS_DIRTY, None, None).0 != 0);
        // An editor still on screen after WM_CLOSE (its user chose Cancel,
        // or one of its dialogs is open) stops the quit and comes forward.
        for e in dirty.into_iter().chain(clean) {
            SendMessageW(e, WM_CLOSE, None, None);
            if editor_on_screen(e) {
                let _ = SetForegroundWindow(GetLastActivePopup(e));
                return;
            }
        }
        // Editors opened meanwhile (from a thumbnail, during a prompt) count too.
        if any_editor_on_screen() {
            return;
        }
        // Thumbnails whose save failed hold the only copy of their image.
        let unsaved = unsaved_thumbnail_images().len();
        if unsaved > 0 {
            let text = format!(
                "{unsaved} screenshot(s) could not be saved and exist only as thumbnails. \
                 Quit anyway? SgCap will try once more to save them on the Desktop."
            );
            if MessageBoxW(None, &HSTRING::from(text), w!("SgCap"), MB_OKCANCEL | MB_ICONWARNING) != IDOK {
                return;
            }
            save_unsaved_thumbnails();
        }
        // The editors' results are already queued: they get saved first.
        let _ = PostMessageW(Some(hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
    }
}

/// Windows is ending the session: nobody can answer a prompt now, so any
/// unsaved annotations are written next to their capture as
/// "<name> (annotated).png", leaving the original untouched. The writes run
/// on workers so that one deadline (wait_for_saves) covers them all.
fn save_editors_for_session_end() {
    let Some((editors, folder)) = with_app(|a| (a.editors.clone(), a.settings.resolved_save_folder())) else {
        return;
    };
    for e in editors {
        let raw = unsafe {
            if !IsWindow(Some(e)).as_bool() {
                continue;
            }
            SendMessageW(e, WM_APP_EDITOR_SNAPSHOT, None, None).0
        };
        if raw == 0 {
            continue;
        }
        let snap = unsafe { Box::from_raw(raw as *mut Snapshot) };
        let (folders, base) = match &snap.file {
            Some(p) => {
                let mut v = vec![p.parent().map(Path::to_path_buf).unwrap_or_else(|| folder.clone())];
                v.extend(fallback_folders(&folder));
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                (v, format!("{stem} (annotated)"))
            }
            // Its capture was never saved: a local folder first, the save
            // folder (maybe a slow share) last.
            None => (vec![settings::desktop_dir(), folder.clone()], format!("{} (annotated)", util::timestamp_name())),
        };
        let mut folders = folders;
        folders.dedup();
        let worker = std::thread::spawn(move || {
            match capture::encode_png(&snap.image).and_then(|bytes| write_new_somewhere(&folders, &base, &bytes)) {
                Ok(p) => log(&format!("session end: saved open annotations as {}", p.display())),
                Err(e) => log(&format!("session end: could not save open annotations: {e}")),
            }
            None
        });
        with_app(|a| a.workers.push(worker));
    }
}

/// The editor closed: remember its style, then bring the thumbnail back,
/// with the annotated image (written over the original file) after Done.
fn on_editor_closed(o: EditorOutcome) {
    with_app(|a| a.editors.retain(|&e| e != o.hwnd));
    remember_style(o.initial, o.prefs);
    let mon = monitor_at(o.anchor);
    match o.outcome {
        Outcome::Saved(edited) => {
            copy_if_enabled(&edited);
            let target = match o.file {
                PendingFile::Ready(Some((path, _))) => SaveTarget::Replace(path),
                PendingFile::Ready(None) => SaveTarget::NewFile,
                PendingFile::Saving(rx) => SaveTarget::AfterSave(rx, true),
            };
            show_thumbnail(Arc::new(edited), target, mon);
        }
        Outcome::Unchanged | Outcome::Cancelled => {
            let target = match o.file {
                PendingFile::Ready(Some((path, png))) => SaveTarget::Existing(path, png),
                PendingFile::Ready(None) => SaveTarget::NewFile,
                PendingFile::Saving(rx) => SaveTarget::AfterSave(rx, false),
            };
            show_thumbnail(o.image, target, mon);
        }
    }
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
                request_quit(hwnd);
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
        WM_APP_EDIT_REQUEST => {
            let req = Box::from_raw(lparam.0 as *mut EditRequest);
            open_editor(*req);
            LRESULT(0)
        }
        WM_APP_EDITOR_CLOSED => {
            let outcome = Box::from_raw(lparam.0 as *mut EditorOutcome);
            on_editor_closed(*outcome);
            LRESULT(0)
        }
        WM_APP_SAVE_FAILED => {
            let e = Box::from_raw(lparam.0 as *mut String);
            with_app(|a| {
                if let Some(t) = &a.tray {
                    let text = format!("{e}. The thumbnail stays on screen: right-click it to copy the image.");
                    t.balloon("SgCap could not save the screenshot", &text, true);
                }
            });
            LRESULT(0)
        }
        WM_APP_QUIT => {
            if any_editor_on_screen() {
                // An editor opened after the prompts: ask it too.
                request_quit(hwnd);
            } else {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Polite close requests (taskkill, scripts) go through the editors' prompts.
            request_quit(hwnd);
            LRESULT(0)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                // Results of editors that closed just now are still queued.
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, Some(hwnd), WM_APP_EDITOR_CLOSED, WM_APP_EDITOR_CLOSED, PM_REMOVE).as_bool() {
                    on_editor_closed(*Box::from_raw(msg.lParam.0 as *mut EditorOutcome));
                }
                save_editors_for_session_end();
                save_unsaved_thumbnails();
                for e in wait_for_saves() {
                    log(&format!("session end: {e}"));
                }
            }
            LRESULT(0)
        }
        WM_APP_SETTINGS_CHANGED => {
            match Settings::try_load() {
                Ok(s) => {
                    with_app(|a| {
                        a.settings = s;
                        a.settings_ok = true;
                    });
                }
                Err(e) => log(&format!("settings: {e}")),
            }
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
