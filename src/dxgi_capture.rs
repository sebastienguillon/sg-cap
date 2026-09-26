//! Fast screen grabbing through DXGI Desktop Duplication.
//!
//! One duplication per monitor is kept alive, with a CPU-readable staging
//! texture holding the last frame. A capture then costs one AcquireNextFrame
//! (returns immediately, or times out when nothing changed and the staging
//! copy is still current), a GPU copy and a memcpy. Anything that fails falls
//! back to GDI for that monitor.

use std::cell::RefCell;

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};

use crate::capture;
use crate::util::{log, rect_h, rect_intersect, rect_w, DibSection};

struct Output {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    output: IDXGIOutput1,
    rect: RECT,
    dup: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    /// True when `staging` holds a valid, current frame.
    primed: bool,
}

pub struct Duplicator {
    outputs: Vec<Output>,
}

thread_local! {
    static DUP: RefCell<Option<Duplicator>> = const { RefCell::new(None) };
}

/// Creates devices and duplications for every attached monitor and primes them.
pub fn init() {
    DUP.with(|d| {
        if d.borrow().is_some() {
            return;
        }
        match Duplicator::new() {
            Ok(mut dup) => {
                for o in dup.outputs.iter_mut() {
                    if let Err(e) = unsafe { o.refresh() } {
                        log(&format!("dxgi: priming failed: {e}"));
                    }
                }
                log(&format!("dxgi: duplication ready for {} output(s)", dup.outputs.len()));
                *d.borrow_mut() = Some(dup);
            }
            Err(e) => log(&format!("dxgi: unavailable, using GDI: {e}")),
        }
    });
}

pub fn reset() {
    DUP.with(|d| {
        *d.borrow_mut() = None;
    });
}

impl Duplicator {
    fn new() -> windows::core::Result<Self> {
        let mut outputs = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                ai += 1;
                let mut outs = Vec::new();
                let mut oi = 0;
                while let Ok(o) = adapter.EnumOutputs(oi) {
                    oi += 1;
                    outs.push(o);
                }
                if outs.is_empty() {
                    continue;
                }
                let mut device: Option<ID3D11Device> = None;
                let mut ctx: Option<ID3D11DeviceContext> = None;
                if let Err(e) = D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG(0),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                ) {
                    log(&format!("dxgi: device creation failed: {e}"));
                    continue;
                }
                let (Some(device), Some(ctx)) = (device, ctx) else {
                    continue;
                };
                for o in outs {
                    let Ok(desc) = o.GetDesc() else {
                        continue;
                    };
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                        log("dxgi: rotated output, GDI will be used for it");
                        continue;
                    }
                    let Ok(o1) = o.cast::<IDXGIOutput1>() else {
                        continue;
                    };
                    outputs.push(Output {
                        device: device.clone(),
                        ctx: ctx.clone(),
                        output: o1,
                        rect: desc.DesktopCoordinates,
                        dup: None,
                        staging: None,
                        primed: false,
                    });
                }
            }
        }
        if outputs.is_empty() {
            return Err(windows::core::Error::empty());
        }
        Ok(Duplicator { outputs })
    }
}

impl Output {
    unsafe fn ensure_dup(&mut self) -> windows::core::Result<()> {
        if self.dup.is_none() {
            self.dup = Some(self.output.DuplicateOutput(&self.device)?);
            self.primed = false;
        }
        Ok(())
    }

    /// Brings `staging` up to date with the desktop.
    unsafe fn refresh(&mut self) -> windows::core::Result<()> {
        self.ensure_dup()?;
        let dup = self.dup.clone().unwrap();
        // A primed duplication only needs a very short wait: a timeout means nothing
        // changed since the staging copy was taken.
        let timeout = if self.primed { 2 } else { 400 };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match dup.AcquireNextFrame(timeout, &mut info, &mut res) {
            Ok(()) => {
                let result = (|| -> windows::core::Result<()> {
                    let tex: ID3D11Texture2D = res.as_ref().ok_or_else(windows::core::Error::empty)?.cast()?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    tex.GetDesc(&mut desc);
                    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                        return Err(windows::core::Error::empty());
                    }
                    let need_new = match &self.staging {
                        Some(s) => {
                            let mut sd = D3D11_TEXTURE2D_DESC::default();
                            s.GetDesc(&mut sd);
                            sd.Width != desc.Width || sd.Height != desc.Height
                        }
                        None => true,
                    };
                    if need_new {
                        let sdesc = D3D11_TEXTURE2D_DESC {
                            Width: desc.Width,
                            Height: desc.Height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: desc.Format,
                            SampleDesc: desc.SampleDesc,
                            Usage: D3D11_USAGE_STAGING,
                            BindFlags: 0,
                            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                            MiscFlags: 0,
                        };
                        let mut staging: Option<ID3D11Texture2D> = None;
                        self.device.CreateTexture2D(&sdesc, None, Some(&mut staging))?;
                        self.staging = staging;
                    }
                    let staging = self.staging.as_ref().ok_or_else(windows::core::Error::empty)?;
                    self.ctx.CopyResource(staging, &tex);
                    self.primed = true;
                    Ok(())
                })();
                let _ = dup.ReleaseFrame();
                if result.is_err() {
                    self.invalidate();
                }
                result
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT && self.primed => Ok(()),
            Err(e) => {
                self.invalidate();
                Err(e)
            }
        }
    }

    fn invalidate(&mut self) {
        self.dup = None;
        self.staging = None;
        self.primed = false;
    }

    /// Copies `sub` (desktop coordinates, inside this output) into `dst`.
    unsafe fn read_into(&self, dst: &mut DibSection, dst_rect: RECT, sub: RECT) -> windows::core::Result<()> {
        let staging = self.staging.as_ref().ok_or_else(windows::core::Error::empty)?;
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let pitch = m.RowPitch as usize;
        let src_base = m.pData as *const u8;
        let dst_stride = dst.stride();
        let dst_bytes = dst.bytes_mut();
        let x0 = (sub.left - self.rect.left) as usize;
        let y0 = (sub.top - self.rect.top) as usize;
        let w = rect_w(&sub) as usize;
        let h = rect_h(&sub) as usize;
        let dx = (sub.left - dst_rect.left) as usize * 4;
        let dy = (sub.top - dst_rect.top) as usize;
        for y in 0..h {
            let src = src_base.add((y0 + y) * pitch + x0 * 4);
            let off = (dy + y) * dst_stride + dx;
            std::ptr::copy_nonoverlapping(src, dst_bytes.as_mut_ptr().add(off), w * 4);
        }
        self.ctx.Unmap(staging, 0);
        Ok(())
    }
}

/// GDI fallback for one sub-rectangle.
fn gdi_into(dst: &mut DibSection, dst_rect: RECT, sub: RECT) {
    if let Ok(part) = capture::grab(sub) {
        let src = part.bytes();
        let row = part.width as usize * 4;
        let dst_stride = dst.stride();
        let dx = (sub.left - dst_rect.left) as usize * 4;
        let dy = (sub.top - dst_rect.top) as usize;
        let dst_bytes = dst.bytes_mut();
        for y in 0..part.height as usize {
            let off = (dy + y) * dst_stride + dx;
            dst_bytes[off..off + row].copy_from_slice(&src[y * row..(y + 1) * row]);
        }
    }
}

/// Grabs `rect` (virtual-screen coordinates). Returns None when duplication is
/// not available at all, in which case the caller should use GDI.
pub fn grab(rect: RECT) -> Option<DibSection> {
    DUP.with(|cell| {
        let mut guard = cell.borrow_mut();
        let dup = guard.as_mut()?;
        let mut dib = DibSection::new(rect_w(&rect), rect_h(&rect)).ok()?;
        let mut covered = false;
        for o in dup.outputs.iter_mut() {
            let Some(sub) = rect_intersect(&o.rect, &rect) else {
                continue;
            };
            covered = true;
            unsafe {
                let mut ok = o.refresh().is_ok();
                if !ok {
                    // Duplication was lost (lock screen, mode change): rebuild once.
                    ok = o.refresh().is_ok();
                }
                if ok && o.read_into(&mut dib, rect, sub).is_ok() {
                    continue;
                }
                log("dxgi: output read failed, GDI fallback for this monitor");
                o.invalidate();
                gdi_into(&mut dib, rect, sub);
            }
        }
        if !covered {
            return None;
        }
        Some(dib)
    })
}
