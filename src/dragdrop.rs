//! OLE drag source (file + bitmap + PNG formats) and clipboard export.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::ptr::null_mut;

use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, COLORREF, HANDLE, HGLOBAL, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB, HBITMAP};
use windows::Win32::System::Com::{
    CoCreateInstance, IDataObject, CLSCTX_INPROC_SERVER, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
    STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{IDropSource, CF_DIB, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IDragSourceHelper, ILClone, ILCreateFromPathW, ILFindLastID, ILFree, ILRemoveLastID, SHCreateDataObject,
    SHDoDragDrop, CLSID_DragDropHelper, DROPFILES, SHDRAGIMAGE,
};

use crate::capture::Image;
use crate::thumb_image::ThumbImage;
use crate::util::{log, wide, DibSection};

/// Copies bytes into a new movable HGLOBAL.
unsafe fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    let hg = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
    let p = GlobalLock(hg) as *mut u8;
    if p.is_null() {
        let _ = GlobalFree(Some(hg));
        return Err(windows::core::Error::from_thread());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    let _ = GlobalUnlock(hg);
    Ok(hg)
}

/// CF_DIB payload: BITMAPINFOHEADER + 24bpp bottom-up pixels (the most widely accepted layout).
fn dib_bytes(img: &Image) -> Vec<u8> {
    let w = img.width as usize;
    let h = img.height as usize;
    let stride = (w * 3 + 3) & !3;
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: img.width as i32,
        biHeight: img.height as i32,
        biPlanes: 1,
        biBitCount: 24,
        biCompression: BI_RGB.0,
        biSizeImage: (stride * h) as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut out = Vec::with_capacity(40 + stride * h);
    let hb = unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<BITMAPINFOHEADER>())
    };
    out.extend_from_slice(hb);
    for y in (0..h).rev() {
        let row = &img.bgra[y * w * 4..(y + 1) * w * 4];
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&px[..3]);
        }
        for _ in 0..(stride - w * 3) {
            out.push(0);
        }
    }
    out
}

/// CF_HDROP payload: DROPFILES header followed by a double-NUL-terminated wide path list.
fn hdrop_bytes(path: &Path) -> Vec<u8> {
    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: windows::core::BOOL(0),
        fWide: windows::core::BOOL(1),
    };
    let mut out = Vec::new();
    let hb = unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<DROPFILES>())
    };
    out.extend_from_slice(hb);
    let mut w = wide(&path.to_string_lossy());
    w.push(0);
    for u in w {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

unsafe fn set_hglobal_format(dobj: &IDataObject, cf: u32, bytes: &[u8]) -> windows::core::Result<()> {
    let hg = hglobal_from(bytes)?;
    let fmt = FORMATETC {
        cfFormat: cf as u16,
        ptd: null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: hg },
        pUnkForRelease: ManuallyDrop::new(None),
    };
    // fRelease = true: the data object owns the HGLOBAL from now on.
    dobj.SetData(&fmt, &medium, true)
}

unsafe fn drag_bitmap(thumb: &ThumbImage) -> windows::core::Result<HBITMAP> {
    let mut dib = DibSection::new(thumb.w, thumb.h)?;
    dib.bytes_mut().copy_from_slice(&thumb.bgra);
    let hbm = dib.hbm;
    // Ownership goes to the drag helper; keep the bitmap alive.
    std::mem::forget(dib);
    Ok(hbm)
}

/// Runs a modal drag-and-drop loop. `offset` is the cursor position inside the drag image.
pub fn start_drag(
    hwnd: HWND,
    path: &Path,
    image: &Image,
    png: Option<&[u8]>,
    drag_image: &ThumbImage,
    offset: POINT,
) -> windows::core::Result<DROPEFFECT> {
    unsafe {
        let path_w = wide(&path.to_string_lossy());
        let abs = ILCreateFromPathW(windows::core::PCWSTR(path_w.as_ptr()));
        if abs.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        // The shell data object wants a parent folder plus child item ids.
        let parent = ILClone(abs);
        let _ = ILRemoveLastID(Some(parent));
        let child = ILFindLastID(abs);
        let children = [child as *const ITEMIDLIST];
        let dobj: windows::core::Result<IDataObject> =
            SHCreateDataObject(Some(parent as *const ITEMIDLIST), Some(&children), None::<&IDataObject>);
        ILFree(Some(parent as *const ITEMIDLIST));
        ILFree(Some(abs as *const ITEMIDLIST));
        let dobj = dobj?;

        // Plain file list for every target that understands file drops.
        if let Err(e) = set_hglobal_format(&dobj, CF_HDROP.0 as u32, &hdrop_bytes(path)) {
            log(&format!("drag: CF_HDROP SetData failed: {e}"));
        }
        if let Err(e) = set_hglobal_format(&dobj, CF_DIB.0 as u32, &dib_bytes(image)) {
            log(&format!("drag: CF_DIB SetData failed: {e}"));
        }
        if let Some(png) = png {
            let cf_png = RegisterClipboardFormatW(w!("PNG"));
            if cf_png != 0 {
                if let Err(e) = set_hglobal_format(&dobj, cf_png, png) {
                    log(&format!("drag: PNG SetData failed: {e}"));
                }
            }
        }

        match CoCreateInstance::<_, IDragSourceHelper>(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) {
            Ok(helper) => match drag_bitmap(drag_image) {
                Ok(hbm) => {
                    let shdi = SHDRAGIMAGE {
                        sizeDragImage: SIZE {
                            cx: drag_image.w,
                            cy: drag_image.h,
                        },
                        ptOffset: offset,
                        hbmpDragImage: hbm,
                        crColorKey: COLORREF(0xFFFFFFFF),
                    };
                    if let Err(e) = helper.InitializeFromBitmap(&shdi, &dobj) {
                        log(&format!("drag: InitializeFromBitmap failed: {e}"));
                    }
                }
                Err(e) => log(&format!("drag: drag bitmap failed: {e}")),
            },
            Err(e) => log(&format!("drag: no drag helper: {e}")),
        }

        SHDoDragDrop(Some(hwnd), &dobj, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_LINK)
    }
}

/// Puts the image on the clipboard as CF_DIB (+ "PNG" when available).
pub fn copy_to_clipboard(hwnd: HWND, image: &Image, png: Option<&[u8]>) -> windows::core::Result<()> {
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let result = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let dib = hglobal_from(&dib_bytes(image))?;
            SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(dib.0 as *mut c_void)))?;
            if let Some(png) = png {
                let cf_png = RegisterClipboardFormatW(w!("PNG"));
                if cf_png != 0 {
                    let hg = hglobal_from(png)?;
                    SetClipboardData(cf_png, Some(HANDLE(hg.0 as *mut c_void)))?;
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}
