//! "Start with Windows" via HKCU\...\Run.

use windows::core::w;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

use crate::util::wide;

const VALUE_NAME: windows::core::PCWSTR = w!("SgCap");

fn open_run_key() -> Option<HKEY> {
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            None,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            &mut hkey,
        )
    };
    if err == ERROR_SUCCESS {
        Some(hkey)
    } else {
        None
    }
}

pub fn is_enabled() -> bool {
    let Some(hkey) = open_run_key() else {
        return false;
    };
    let mut size = 0u32;
    let err = unsafe { RegQueryValueExW(hkey, VALUE_NAME, None, None, None, Some(&mut size)) };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    err == ERROR_SUCCESS
}

pub fn set_enabled(enable: bool) -> Result<(), String> {
    let hkey = open_run_key().ok_or("cannot open Run key")?;
    let result = unsafe {
        if enable {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd = format!("\"{}\"", exe.display());
            let w = wide(&cmd);
            let bytes = std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2);
            let err = RegSetValueExW(hkey, VALUE_NAME, None, REG_SZ, Some(bytes));
            if err == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("RegSetValueExW failed: {}", err.0))
            }
        } else {
            let err = RegDeleteValueW(hkey, VALUE_NAME);
            // Deleting a value that does not exist is fine.
            if err == ERROR_SUCCESS || err.0 == 2 {
                Ok(())
            } else {
                Err(format!("RegDeleteValueW failed: {}", err.0))
            }
        }
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    result
}
