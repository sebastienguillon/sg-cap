//! Settings window built from plain Win32 controls.

use std::cell::Cell;
use std::ffi::c_void;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, HGDIOBJ, COLOR_3DFACE};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IBindCtx, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Controls::{BST_CHECKED, EM_SETLIMITTEXT, HOTKEY_CLASS, WC_BUTTONW, WC_EDITW, WC_STATICW};
use windows::Win32::UI::Controls::{HKM_GETHOTKEY, HKM_SETHOTKEY};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForMonitor, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, GetWindowTextW, IsChild, LoadCursorW,
    MessageBoxW, PostMessageW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, SetWindowTextW, ShowWindow, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON,
    BS_PUSHBUTTON, ES_AUTOHSCROLL, ES_NUMBER, GWLP_USERDATA, HMENU, IDC_ARROW, MB_ICONWARNING, MB_OK, MSG,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_NCDESTROY, WM_SETFONT,
    WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::capture::{cursor_pos, monitor_at};
use crate::hotkey::HotKey;
use crate::settings::Settings;
use crate::util::{from_wide, hiword, log, loword, rect_h, rect_w, wide};

pub const WM_APP_SETTINGS_CHANGED: u32 = WM_APP + 5;
pub const WM_APP_SETTINGS_CLOSED: u32 = WM_APP + 7;

const CLASS_NAME: PCWSTR = w!("SgCapSettings");

const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;
const ID_REGION_HK: i32 = 101;
const ID_SCREEN_HK: i32 = 102;
const ID_SECONDS: i32 = 103;
const ID_FOLDER: i32 = 104;
const ID_BROWSE: i32 = 105;
const ID_CLIPBOARD: i32 = 106;
const ID_STARTUP: i32 = 107;
const ID_LABEL_BASE: i32 = 120;

const CLIENT_W: i32 = 470;
const CLIENT_H: i32 = 262;

thread_local! {
    static CURRENT: Cell<isize> = const { Cell::new(0) };
}

pub fn current_hwnd() -> HWND {
    HWND(CURRENT.with(|c| c.get()) as *mut c_void)
}

struct Ui {
    hwnd: HWND,
    main_hwnd: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    dpi: u32,
    controls: Vec<(i32, HWND)>,
}

struct Item {
    id: i32,
    class: PCWSTR,
    text: String,
    style: u32,
    rect: (i32, i32, i32, i32),
}

fn items() -> Vec<Item> {
    let mut v = Vec::new();
    let mut label = |id: i32, text: &str, y: i32| {
        v.push(Item {
            id,
            class: WC_STATICW,
            text: text.to_string(),
            style: 0,
            rect: (14, y + 4, 156, 20),
        });
    };
    label(ID_LABEL_BASE, "Region capture hotkey:", 16);
    label(ID_LABEL_BASE + 1, "Screen capture hotkey:", 50);
    label(ID_LABEL_BASE + 2, "Thumbnail stays (seconds):", 84);
    label(ID_LABEL_BASE + 3, "Save folder:", 118);
    v.push(Item {
        id: ID_REGION_HK,
        class: HOTKEY_CLASS,
        text: String::new(),
        style: WS_TABSTOP.0,
        rect: (176, 16, 200, 24),
    });
    v.push(Item {
        id: ID_SCREEN_HK,
        class: HOTKEY_CLASS,
        text: String::new(),
        style: WS_TABSTOP.0,
        rect: (176, 50, 200, 24),
    });
    v.push(Item {
        id: ID_SECONDS,
        class: WC_EDITW,
        text: String::new(),
        style: WS_TABSTOP.0 | ES_NUMBER as u32 | ES_AUTOHSCROLL as u32,
        rect: (176, 84, 60, 24),
    });
    v.push(Item {
        id: ID_FOLDER,
        class: WC_EDITW,
        text: String::new(),
        style: WS_TABSTOP.0 | ES_AUTOHSCROLL as u32,
        rect: (176, 118, 204, 24),
    });
    v.push(Item {
        id: ID_BROWSE,
        class: WC_BUTTONW,
        text: "Browse...".to_string(),
        style: WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        rect: (388, 117, 68, 26),
    });
    v.push(Item {
        id: ID_CLIPBOARD,
        class: WC_BUTTONW,
        text: "Also copy the image to the clipboard".to_string(),
        style: WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        rect: (176, 154, 280, 22),
    });
    v.push(Item {
        id: ID_STARTUP,
        class: WC_BUTTONW,
        text: "Start SgCap with Windows".to_string(),
        style: WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        rect: (176, 180, 280, 22),
    });
    v.push(Item {
        id: ID_OK,
        class: WC_BUTTONW,
        text: "OK".to_string(),
        style: WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        rect: (284, 220, 82, 28),
    });
    v.push(Item {
        id: ID_CANCEL,
        class: WC_BUTTONW,
        text: "Cancel".to_string(),
        style: WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        rect: (374, 220, 82, 28),
    });
    v
}

fn scaled(v: i32, dpi: u32) -> i32 {
    (v as f32 * dpi as f32 / 96.0).round() as i32
}

fn message_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        if SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            0,
            dpi,
        )
        .is_ok()
        {
            let f = CreateFontIndirectW(&ncm.lfMessageFont);
            if !f.is_invalid() {
                return f;
            }
        }
        HFONT::default()
    }
}

impl Ui {
    fn control(&self, id: i32) -> HWND {
        self.controls
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, h)| *h)
            .unwrap_or_default()
    }

    fn layout(&mut self) {
        let dpi = self.dpi;
        unsafe {
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
            self.font = message_font(dpi);
            for item in items() {
                let h = self.control(item.id);
                if h.is_invalid() {
                    continue;
                }
                let (x, y, w, hh) = item.rect;
                let _ = SetWindowPos(
                    h,
                    None,
                    scaled(x, dpi),
                    scaled(y, dpi),
                    scaled(w, dpi),
                    scaled(hh, dpi),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                SendMessageW(h, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            }
        }
    }

    fn build(&mut self, settings: &Settings) {
        unsafe {
            for item in items() {
                let text = HSTRING::from(item.text.as_str());
                let style = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(item.style);
                match CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    item.class,
                    &text,
                    style,
                    0,
                    0,
                    10,
                    10,
                    Some(self.hwnd),
                    Some(HMENU(item.id as usize as *mut c_void)),
                    Some(self.hinst),
                    None,
                ) {
                    Ok(h) => self.controls.push((item.id, h)),
                    Err(e) => log(&format!("settings: control {} failed: {e}", item.id)),
                }
            }
            self.layout();
            self.fill(settings);
            SendMessageW(self.control(ID_SECONDS), EM_SETLIMITTEXT, Some(WPARAM(3)), None);
            let _ = SetFocus(Some(self.control(ID_REGION_HK)));
        }
    }

    fn fill(&self, s: &Settings) {
        unsafe {
            let set_hk = |id: i32, text: &str| {
                let v = HotKey::parse(text).map(|h| h.to_control()).unwrap_or(0);
                SendMessageW(self.control(id), HKM_SETHOTKEY, Some(WPARAM(v as usize)), None);
            };
            set_hk(ID_REGION_HK, &s.region_hotkey);
            set_hk(ID_SCREEN_HK, &s.screen_hotkey);
            let secs = wide(&s.thumbnail_seconds.to_string());
            let _ = SetWindowTextW(self.control(ID_SECONDS), PCWSTR(secs.as_ptr()));
            let folder = wide(&s.resolved_save_folder().to_string_lossy());
            let _ = SetWindowTextW(self.control(ID_FOLDER), PCWSTR(folder.as_ptr()));
            let check = |id: i32, on: bool| {
                SendMessageW(
                    self.control(id),
                    BM_SETCHECK,
                    Some(WPARAM(if on { BST_CHECKED.0 as usize } else { 0 })),
                    None,
                );
            };
            check(ID_CLIPBOARD, s.copy_to_clipboard);
            check(ID_STARTUP, s.start_with_windows);
        }
    }

    fn text_of(&self, id: i32) -> String {
        let mut buf = [0u16; 1024];
        unsafe {
            GetWindowTextW(self.control(id), &mut buf);
        }
        from_wide(&buf)
    }

    fn read(&self) -> Result<Settings, String> {
        unsafe {
            let hk = |id: i32| -> Option<HotKey> {
                let v = SendMessageW(self.control(id), HKM_GETHOTKEY, None, None).0 as u32;
                HotKey::from_control(v)
            };
            let region = hk(ID_REGION_HK).ok_or("Please choose a region capture hotkey.")?;
            let screen = hk(ID_SCREEN_HK);
            if let Some(sc) = screen {
                if sc == region {
                    return Err("The two hotkeys must be different.".into());
                }
            }
            let secs: u32 = self
                .text_of(ID_SECONDS)
                .trim()
                .parse()
                .map_err(|_| "Thumbnail duration must be a number of seconds.")?;
            if !(1..=120).contains(&secs) {
                return Err("Thumbnail duration must be between 1 and 120 seconds.".into());
            }
            let folder = self.text_of(ID_FOLDER).trim().to_string();
            if folder.is_empty() {
                return Err("Please choose a save folder.".into());
            }
            std::fs::create_dir_all(&folder).map_err(|e| format!("Cannot use folder \"{folder}\": {e}"))?;
            let checked = |id: i32| SendMessageW(self.control(id), BM_GETCHECK, None, None).0 as u32 == BST_CHECKED.0;
            Ok(Settings {
                region_hotkey: region.display(),
                screen_hotkey: screen.map(|h| h.display()).unwrap_or_default(),
                thumbnail_seconds: secs,
                save_folder: folder,
                copy_to_clipboard: checked(ID_CLIPBOARD),
                start_with_windows: checked(ID_STARTUP),
            })
        }
    }

    fn browse(&self) {
        let current = self.text_of(ID_FOLDER);
        unsafe {
            let dlg: IFileOpenDialog = match CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) {
                Ok(d) => d,
                Err(e) => {
                    log(&format!("settings: file dialog failed: {e}"));
                    return;
                }
            };
            if let Ok(opts) = dlg.GetOptions() {
                let _ = dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM);
            }
            let _ = dlg.SetTitle(w!("Choose where screenshots are saved"));
            if !current.trim().is_empty() {
                if let Ok(item) =
                    SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(current.trim()), None::<&IBindCtx>)
                {
                    let _ = dlg.SetFolder(&item);
                }
            }
            if dlg.Show(Some(self.hwnd)).is_ok() {
                if let Ok(item) = dlg.GetResult() {
                    if let Ok(p) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as *const c_void));
                        let w = wide(&s);
                        let _ = SetWindowTextW(self.control(ID_FOLDER), PCWSTR(w.as_ptr()));
                    }
                }
            }
        }
    }

    fn on_ok(&self) {
        match self.read() {
            Ok(s) => {
                if let Err(e) = s.save() {
                    unsafe {
                        MessageBoxW(
                            Some(self.hwnd),
                            &HSTRING::from(format!("Could not save settings: {e}")),
                            w!("SgCap"),
                            MB_OK | MB_ICONWARNING,
                        );
                    }
                    return;
                }
                unsafe {
                    let _ = PostMessageW(Some(self.main_hwnd), WM_APP_SETTINGS_CHANGED, WPARAM(0), LPARAM(0));
                    let _ = DestroyWindow(self.hwnd);
                }
            }
            Err(msg) => unsafe {
                MessageBoxW(Some(self.hwnd), &HSTRING::from(msg), w!("SgCap"), MB_OK | MB_ICONWARNING);
            },
        }
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        unsafe {
            match msg {
                WM_COMMAND => {
                    let id = loword(wparam.0) as i32;
                    let code = hiword(wparam.0);
                    if code == 0 {
                        match id {
                            ID_OK => self.on_ok(),
                            ID_CANCEL => {
                                let _ = DestroyWindow(self.hwnd);
                            }
                            ID_BROWSE => self.browse(),
                            _ => {}
                        }
                    }
                    Some(LRESULT(0))
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(self.hwnd);
                    Some(LRESULT(0))
                }
                WM_DPICHANGED => {
                    self.dpi = hiword(wparam.0);
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
                    self.layout();
                    Some(LRESULT(0))
                }
                WM_DESTROY => {
                    CURRENT.with(|c| c.set(0));
                    if !self.font.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.font.0));
                        self.font = HFONT::default();
                    }
                    let _ = PostMessageW(Some(self.main_hwnd), WM_APP_SETTINGS_CLOSED, WPARAM(0), LPARAM(0));
                    Some(LRESULT(0))
                }
                _ => None,
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if msg == WM_NCDESTROY {
        if !ptr.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    if !ptr.is_null() {
        if let Some(r) = (*ptr).handle(msg, wparam, lparam) {
            return r;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Enter/Escape handling for the settings window (called from the message loop).
pub fn pre_translate(settings_hwnd: HWND, msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN {
        return false;
    }
    unsafe {
        if msg.hwnd != settings_hwnd && !IsChild(settings_hwnd, msg.hwnd).as_bool() {
            return false;
        }
        let vk = msg.wParam.0 as u16;
        if vk == VK_ESCAPE.0 {
            let _ = PostMessageW(Some(settings_hwnd), WM_COMMAND, WPARAM(ID_CANCEL as usize), LPARAM(0));
            return true;
        }
        if vk == VK_RETURN.0 {
            let _ = PostMessageW(Some(settings_hwnd), WM_COMMAND, WPARAM(ID_OK as usize), LPARAM(0));
            return true;
        }
    }
    false
}

pub fn open(hinst: HINSTANCE, main_hwnd: HWND, settings: &Settings) -> windows::core::Result<()> {
    let existing = current_hwnd();
    if !existing.is_invalid() {
        unsafe {
            let _ = ShowWindow(existing, SW_SHOW);
            let _ = SetForegroundWindow(existing);
        }
        return Ok(());
    }
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: crate::tray::load_app_icon(32).unwrap_or_default(),
            hIconSm: crate::tray::load_app_icon(16).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_3DFACE.0 + 1) as usize as *mut c_void),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let mon = monitor_at(cursor_pos());
        let mut dpi = 96u32;
        let mut dpi_y = 96u32;
        if GetDpiForMonitor(mon.handle, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y).is_err() {
            dpi = 96;
        }
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let ex = WS_EX_DLGMODALFRAME;
        let mut r = RECT {
            left: 0,
            top: 0,
            right: scaled(CLIENT_W, dpi),
            bottom: scaled(CLIENT_H, dpi),
        };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let w = rect_w(&r);
        let h = rect_h(&r);
        let x = mon.work.left + (rect_w(&mon.work) - w) / 2;
        let y = mon.work.top + (rect_h(&mon.work) - h) / 2;
        let hwnd = CreateWindowExW(
            ex,
            CLASS_NAME,
            w!("SgCap settings"),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let ui = Box::into_raw(Box::new(Ui {
            hwnd,
            main_hwnd,
            hinst,
            font: HFONT::default(),
            dpi,
            controls: Vec::new(),
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ui as isize);
        CURRENT.with(|c| c.set(hwnd.0 as isize));
        (*ui).build(settings);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(())
}
