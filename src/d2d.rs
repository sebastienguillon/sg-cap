//! Direct2D drawing for the annotation editor: per-thread factories, the
//! editor's window render target, a small Canvas API over any render target,
//! and the full-resolution export used when saving or copying. Units are
//! physical pixels (render targets run at 96 DPI).

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, Interface, Result, PCWSTR};
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1DCRenderTarget, ID2D1DeviceContext, ID2D1Factory, ID2D1HwndRenderTarget,
    ID2D1PathGeometry, ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_ALIASED,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_ROUND,
    D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE,
    D2D1_DASH_STYLE_SOLID, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_LINE_JOIN_MITER, D2D1_LINE_JOIN_ROUND,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{GdiFlush, SelectObject};
use windows_numerics::{Matrix3x2, Vector2};

use crate::capture::Image;
use crate::shapes::{arrow_head, Kind, Pt, Shape};
use crate::util::{log, DibSection, MemDc};

/// Device-independent resources (factories, stroke styles), one set per UI thread.
#[derive(Clone)]
struct Gfx {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    /// Round caps and joins: lines, arrows, ellipses.
    round: ID2D1StrokeStyle,
    /// Square corners: rectangles.
    miter: ID2D1StrokeStyle,
}

thread_local! {
    static GFX: RefCell<Option<Gfx>> = const { RefCell::new(None) };
}

fn stroke_style(f: &ID2D1Factory, round: bool) -> Result<ID2D1StrokeStyle> {
    let props = D2D1_STROKE_STYLE_PROPERTIES {
        startCap: D2D1_CAP_STYLE_ROUND,
        endCap: D2D1_CAP_STYLE_ROUND,
        dashCap: D2D1_CAP_STYLE_ROUND,
        lineJoin: if round { D2D1_LINE_JOIN_ROUND } else { D2D1_LINE_JOIN_MITER },
        miterLimit: 10.0,
        dashStyle: D2D1_DASH_STYLE_SOLID,
        dashOffset: 0.0,
    };
    unsafe { f.CreateStrokeStyle(&props, None) }
}

fn gfx() -> Result<Gfx> {
    GFX.with(|cell| {
        if let Some(g) = cell.borrow().as_ref() {
            return Ok(g.clone());
        }
        let factory: ID2D1Factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let g = Gfx {
            round: stroke_style(&factory, true)?,
            miter: stroke_style(&factory, false)?,
            factory,
            dwrite,
        };
        *cell.borrow_mut() = Some(g.clone());
        Ok(g)
    })
}

pub fn needs_recreate(e: &windows::core::Error) -> bool {
    e.code() == D2DERR_RECREATE_TARGET
}

fn color(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a,
    }
}

fn v2(p: Pt) -> Vector2 {
    Vector2 { X: p.x, Y: p.y }
}

fn size_u(w: i32, h: i32) -> D2D_SIZE_U {
    D2D_SIZE_U {
        width: w.max(1) as u32,
        height: h.max(1) as u32,
    }
}

fn pixel_format(alpha: D2D1_ALPHA_MODE) -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: alpha,
    }
}

fn target_props(ty: D2D1_RENDER_TARGET_TYPE) -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: ty,
        pixelFormat: pixel_format(D2D1_ALPHA_MODE_IGNORE),
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

/// Rectangle in pixels (f32), left/top/right/bottom.
#[derive(Clone, Copy, Debug, Default)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub fn inset(self, d: f32) -> RectF {
        RectF {
            l: self.l + d,
            t: self.t + d,
            r: self.r - d,
            b: self.b - d,
        }
    }

    fn d2d(self) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.l,
            top: self.t,
            right: self.r,
            bottom: self.b,
        }
    }
}

impl From<RECT> for RectF {
    fn from(r: RECT) -> RectF {
        RectF {
            l: r.left as f32,
            t: r.top as f32,
            r: r.right as f32,
            b: r.bottom as f32,
        }
    }
}

/// Text formats for the toolbar, sized for one DPI scale.
pub struct Fonts {
    pub ui: IDWriteTextFormat,
    /// Segoe MDL2 Assets (ships with Windows 10 and 11) for the action icons.
    pub icons: IDWriteTextFormat,
}

fn text_format(dw: &IDWriteFactory, family: PCWSTR, weight: DWRITE_FONT_WEIGHT, px: f32) -> Result<IDWriteTextFormat> {
    unsafe {
        let f = dw.CreateTextFormat(
            family,
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            px,
            w!(""),
        )?;
        f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(f)
    }
}

impl Fonts {
    /// `k` is the DPI scale (1.0 at 96 dpi).
    pub fn new(k: f32) -> Result<Fonts> {
        let g = gfx()?;
        Ok(Fonts {
            ui: text_format(&g.dwrite, w!("Segoe UI"), DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0 * k)?,
            icons: text_format(&g.dwrite, w!("Segoe MDL2 Assets"), DWRITE_FONT_WEIGHT_NORMAL, 16.0 * k)?,
        })
    }
}

/// Uploads the capture as a bitmap; one larger than the GPU allows is shown
/// downscaled (the export always uses the full-resolution pixels).
fn upload(rt: &ID2D1RenderTarget, image: &Image) -> Result<ID2D1Bitmap> {
    let (w, h) = (image.width.max(1), image.height.max(1));
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: pixel_format(D2D1_ALPHA_MODE_IGNORE),
        dpiX: 96.0,
        dpiY: 96.0,
    };
    unsafe {
        let max = rt.GetMaximumBitmapSize().max(2048);
        if w <= max && h <= max {
            return rt.CreateBitmap(
                D2D_SIZE_U { width: w, height: h },
                Some(image.bgra.as_ptr() as *const c_void),
                w * 4,
                &props,
            );
        }
        let s = max as f32 / w.max(h) as f32;
        let tw = ((w as f32 * s) as u32).clamp(1, max);
        let th = ((h as f32 * s) as u32).clamp(1, max);
        log(&format!("editor: {w}x{h} exceeds the {max} px bitmap limit, showing it at {tw}x{th}"));
        let small = crate::thumb_image::downscale(image, tw as usize, th as usize);
        rt.CreateBitmap(
            D2D_SIZE_U { width: tw, height: th },
            Some(small.as_ptr() as *const c_void),
            tw * 4,
            &props,
        )
    }
}

/// The editor window's render target and its device-dependent resources.
/// Drop it and create a new one when drawing reports D2DERR_RECREATE_TARGET.
pub struct Target {
    rt: ID2D1HwndRenderTarget,
    /// Direct2D 1.1 context of the same target, for high-quality downscaling.
    dc: Option<ID2D1DeviceContext>,
    bitmap: ID2D1Bitmap,
    /// The capture resampled once to the on-screen size (width, height, bitmap),
    /// so frames do not redo a full-resolution cubic downscale.
    scaled: RefCell<Option<(u32, u32, ID2D1Bitmap)>>,
    brush: ID2D1SolidColorBrush,
    gfx: Gfx,
}

impl Target {
    /// `software` forces the CPU rasterizer (after repeated GPU failures).
    pub fn new(hwnd: HWND, image: &Image, w: i32, h: i32, software: bool) -> Result<Target> {
        let gfx = gfx()?;
        unsafe {
            let ty = if software {
                D2D1_RENDER_TARGET_TYPE_SOFTWARE
            } else {
                D2D1_RENDER_TARGET_TYPE_DEFAULT
            };
            let rt = gfx.factory.CreateHwndRenderTarget(
                &target_props(ty),
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: size_u(w, h),
                    // Present on vsync: caps redraws during a drag at the
                    // display rate instead of spinning on every mouse move.
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?;
            // One DIP per physical pixel whatever the monitor's scaling.
            rt.SetDpi(96.0, 96.0);
            let brush = rt.CreateSolidColorBrush(&color(0, 1.0), None)?;
            let bitmap = upload(&rt, image)?;
            let dc = rt.cast::<ID2D1DeviceContext>().ok();
            Ok(Target {
                rt,
                dc,
                bitmap,
                scaled: RefCell::new(None),
                brush,
                gfx,
            })
        }
    }

    /// Resamples the capture to w x h pixels for a scaled-down view (kept until
    /// the size changes). Without it, frames fall back to scaling on the fly.
    pub fn prepare_view(&self, w: u32, h: u32) {
        if w == 0 || h == 0 || self.dc.is_none() {
            return;
        }
        if matches!(&*self.scaled.borrow(), Some((cw, ch, _)) if *cw == w && *ch == h) {
            return;
        }
        let made = unsafe {
            (|| -> Result<ID2D1Bitmap> {
                let px = D2D_SIZE_U { width: w, height: h };
                let dips = D2D_SIZE_F {
                    width: w as f32,
                    height: h as f32,
                };
                let brt = self.rt.CreateCompatibleRenderTarget(
                    Some(&dips),
                    Some(&px),
                    None,
                    D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE,
                )?;
                let dc: ID2D1DeviceContext = brt.cast()?;
                brt.BeginDraw();
                dc.DrawBitmap(
                    &self.bitmap,
                    Some(&D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: w as f32,
                        bottom: h as f32,
                    }),
                    1.0,
                    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                    None,
                    None,
                );
                brt.EndDraw(None, None)?;
                brt.GetBitmap()
            })()
        };
        match made {
            Ok(b) => *self.scaled.borrow_mut() = Some((w, h, b)),
            Err(e) => {
                log(&format!("editor: view bitmap {w}x{h} failed: {e}"));
                *self.scaled.borrow_mut() = None;
            }
        }
    }

    pub fn resize(&self, w: i32, h: i32) {
        unsafe {
            if let Err(e) = self.rt.Resize(&size_u(w, h)) {
                log(&format!("editor: render target resize failed: {e}"));
            }
        }
    }

    /// One frame. An error with `needs_recreate` means the device was lost.
    pub fn draw(&self, f: impl FnOnce(&Canvas)) -> Result<()> {
        unsafe {
            self.rt.BeginDraw();
            self.rt.SetTransform(&Matrix3x2::identity());
            self.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            self.rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let scaled = self.scaled.borrow().as_ref().map(|(w, h, b)| (*w, *h, b.clone()));
            let c = Canvas {
                rt: &self.rt,
                dc: self.dc.as_ref(),
                bitmap: Some(&self.bitmap),
                scaled,
                brush: &self.brush,
                gfx: &self.gfx,
            };
            f(&c);
            self.rt.EndDraw(None, None)
        }
    }
}

/// Drawing primitives over a render target. Colours are 0xRRGGBB plus alpha.
pub struct Canvas<'a> {
    rt: &'a ID2D1RenderTarget,
    dc: Option<&'a ID2D1DeviceContext>,
    bitmap: Option<&'a ID2D1Bitmap>,
    /// Pre-resampled copy of `bitmap` (see Target::prepare_view).
    scaled: Option<(u32, u32, ID2D1Bitmap)>,
    brush: &'a ID2D1SolidColorBrush,
    gfx: &'a Gfx,
}

fn ellipse(c: Pt, rx: f32, ry: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE {
        point: v2(c),
        radiusX: rx,
        radiusY: ry,
    }
}

fn triangle(f: &ID2D1Factory, a: Pt, b: Pt, c: Pt) -> Result<ID2D1PathGeometry> {
    unsafe {
        let geom = f.CreatePathGeometry()?;
        let sink = geom.Open()?;
        sink.BeginFigure(v2(a), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(v2(b));
        sink.AddLine(v2(c));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        Ok(geom)
    }
}

impl Canvas<'_> {
    fn paint(&self, rgb: u32, a: f32) -> &ID2D1SolidColorBrush {
        unsafe {
            self.brush.SetColor(&color(rgb, a));
        }
        self.brush
    }

    pub fn clear(&self, rgb: u32) {
        unsafe { self.rt.Clear(Some(&color(rgb, 1.0))) }
    }

    pub fn fill_rect(&self, r: RectF, rgb: u32, a: f32) {
        unsafe { self.rt.FillRectangle(&r.d2d(), self.paint(rgb, a)) }
    }

    pub fn stroke_rect(&self, r: RectF, rgb: u32, a: f32, width: f32) {
        unsafe { self.rt.DrawRectangle(&r.d2d(), self.paint(rgb, a), width, &self.gfx.miter) }
    }

    pub fn fill_round(&self, r: RectF, radius: f32, rgb: u32, a: f32) {
        let rr = D2D1_ROUNDED_RECT {
            rect: r.d2d(),
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.rt.FillRoundedRectangle(&rr, self.paint(rgb, a)) }
    }

    pub fn stroke_round(&self, r: RectF, radius: f32, rgb: u32, a: f32, width: f32) {
        let rr = D2D1_ROUNDED_RECT {
            rect: r.d2d(),
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.rt.DrawRoundedRectangle(&rr, self.paint(rgb, a), width, None) }
    }

    /// Line with round caps.
    pub fn line(&self, p0: Pt, p1: Pt, rgb: u32, a: f32, width: f32) {
        unsafe { self.rt.DrawLine(v2(p0), v2(p1), self.paint(rgb, a), width, &self.gfx.round) }
    }

    pub fn fill_circle(&self, c: Pt, r: f32, rgb: u32, a: f32) {
        unsafe { self.rt.FillEllipse(&ellipse(c, r, r), self.paint(rgb, a)) }
    }

    pub fn stroke_circle(&self, c: Pt, r: f32, rgb: u32, a: f32, width: f32) {
        self.stroke_ellipse(c, r, r, rgb, a, width)
    }

    pub fn stroke_ellipse(&self, c: Pt, rx: f32, ry: f32, rgb: u32, a: f32, width: f32) {
        unsafe { self.rt.DrawEllipse(&ellipse(c, rx, ry), self.paint(rgb, a), width, &self.gfx.round) }
    }

    pub fn fill_triangle(&self, p0: Pt, p1: Pt, p2: Pt, rgb: u32, a: f32) {
        match triangle(&self.gfx.factory, p0, p1, p2) {
            Ok(g) => unsafe { self.rt.FillGeometry(&g, self.paint(rgb, a), None) },
            Err(e) => log(&format!("editor: arrowhead geometry failed: {e}")),
        }
    }

    /// Single line of text centred in `r`.
    pub fn text(&self, s: &str, r: RectF, font: &IDWriteTextFormat, rgb: u32, a: f32) {
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.rt.DrawText(
                &w,
                font,
                &r.d2d(),
                self.paint(rgb, a),
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            )
        }
    }

    /// The capture into `dest`; `smooth` for scaled views, nearest at 100%.
    pub fn image(&self, dest: RectF, smooth: bool) {
        let Some(bmp) = self.bitmap else {
            return;
        };
        if let Some((w, h, small)) = &self.scaled {
            let size = ((dest.r - dest.l).round() as u32, (dest.b - dest.t).round() as u32);
            if smooth && size == (*w, *h) {
                let at = RectF {
                    l: dest.l,
                    t: dest.t,
                    r: dest.l + *w as f32,
                    b: dest.t + *h as f32,
                };
                unsafe {
                    self.rt.DrawBitmap(
                        small,
                        Some(&at.d2d()),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                        None,
                    )
                };
                return;
            }
        }
        unsafe {
            match self.dc {
                Some(dc) => dc.DrawBitmap(
                    bmp,
                    Some(&dest.d2d()),
                    1.0,
                    if smooth {
                        D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
                    } else {
                        D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
                    },
                    None,
                    None,
                ),
                None => self.rt.DrawBitmap(
                    bmp,
                    Some(&dest.d2d()),
                    1.0,
                    if smooth {
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR
                    } else {
                        D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
                    },
                    None,
                ),
            }
        }
    }

    pub fn push_clip(&self, r: RectF) {
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) }
    }

    pub fn pop_clip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() }
    }

    /// Image coordinates from here on: scaled by `scale`, then offset.
    pub fn set_view(&self, scale: f32, dx: f32, dy: f32) {
        let m = Matrix3x2::scale(scale, scale) * Matrix3x2::translation(dx, dy);
        unsafe { self.rt.SetTransform(&m) }
    }

    pub fn reset_view(&self) {
        unsafe { self.rt.SetTransform(&Matrix3x2::identity()) }
    }

    /// An annotation, in the current coordinates.
    pub fn shape(&self, s: &Shape) {
        let (rgb, w) = (s.style.color, s.style.width);
        match s.kind {
            Kind::Line => self.line(s.a, s.b, rgb, 1.0, w),
            Kind::Arrow => match arrow_head(s.a, s.b, w) {
                Some(h) => {
                    if h.shaft {
                        self.line(s.a, h.shaft_end, rgb, 1.0, w);
                    }
                    self.fill_triangle(h.tip, h.left, h.right, rgb, 1.0);
                }
                None => self.line(s.a, s.b, rgb, 1.0, w),
            },
            Kind::Rect => {
                let (lo, hi) = s.bounds();
                self.stroke_rect(
                    RectF {
                        l: lo.x,
                        t: lo.y,
                        r: hi.x,
                        b: hi.y,
                    },
                    rgb,
                    1.0,
                    w,
                );
            }
            Kind::Ellipse => {
                let (lo, hi) = s.bounds();
                let c = Pt::new((lo.x + hi.x) * 0.5, (lo.y + hi.y) * 0.5);
                self.stroke_ellipse(c, (hi.x - lo.x) * 0.5, (hi.y - lo.y) * 0.5, rgb, 1.0, w);
            }
        }
    }
}

/// Draws `shapes` into a copy of `image` at full resolution (software
/// rasterizer: no size limit, same anti-aliasing as the editor shows).
pub fn render_export(image: &Image, shapes: &[Shape]) -> std::result::Result<Image, String> {
    let g = gfx().map_err(|e| format!("Direct2D unavailable: {e}"))?;
    let (w, h) = (image.width as i32, image.height as i32);
    let mut dib = DibSection::new(w, h).map_err(|e| format!("cannot allocate {w}x{h}: {e}"))?;
    if dib.len() != image.bgra.len() {
        return Err(format!("image buffer size mismatch for {w}x{h}"));
    }
    dib.bytes_mut().copy_from_slice(&image.bgra);
    let mem = MemDc::new();
    let result = unsafe {
        let old = SelectObject(mem.0, dib.gdi());
        let result = (|| -> Result<()> {
            let rt: ID2D1DCRenderTarget = g.factory.CreateDCRenderTarget(&target_props(D2D1_RENDER_TARGET_TYPE_SOFTWARE))?;
            rt.BindDC(mem.0, &RECT { left: 0, top: 0, right: w, bottom: h })?;
            let brush = rt.CreateSolidColorBrush(&color(0, 1.0), None)?;
            rt.BeginDraw();
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            let c = Canvas {
                rt: &rt,
                dc: None,
                bitmap: None,
                scaled: None,
                brush: &brush,
                gfx: &g,
            };
            for s in shapes {
                c.shape(s);
            }
            rt.EndDraw(None, None)
        })();
        let _ = GdiFlush();
        SelectObject(mem.0, old);
        result
    };
    result.map_err(|e| format!("drawing failed: {e}"))?;
    Ok(Image {
        width: image.width,
        height: image.height,
        bgra: dib.bytes().to_vec(),
    })
}
