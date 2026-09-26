//! Builds the floating thumbnail bitmap: downscaled capture, rounded corners,
//! light border and a soft drop shadow. Output is premultiplied BGRA, top-down,
//! ready for UpdateLayeredWindow.

use crate::capture::Image;

#[derive(Clone)]
pub struct ThumbImage {
    /// Full bitmap size (includes shadow padding).
    pub w: i32,
    pub h: i32,
    /// Padding around the content on each side (room for the shadow).
    pub pad: i32,
    /// Content (image + border) size.
    pub content_w: i32,
    pub content_h: i32,
    pub bgra: Vec<u8>,
}

const BORDER_COLOR: [f32; 3] = [250.0, 250.0, 250.0]; // B, G, R
const SHADOW_STRENGTH: f32 = 0.55;

/// Area-averaging downscale of a BGRA image to tw x th (returns BGRx).
fn downscale(img: &Image, tw: usize, th: usize) -> Vec<u8> {
    let w = img.width as usize;
    let h = img.height as usize;
    let src = &img.bgra;
    let xr: Vec<(usize, usize)> = (0..tw)
        .map(|dx| {
            let x0 = dx * w / tw;
            let x1 = ((dx + 1) * w / tw).max(x0 + 1).min(w);
            (x0, x1)
        })
        .collect();
    let yr: Vec<(usize, usize)> = (0..th)
        .map(|dy| {
            let y0 = dy * h / th;
            let y1 = ((dy + 1) * h / th).max(y0 + 1).min(h);
            (y0, y1)
        })
        .collect();

    // Pass 1: horizontal reduction of every source row into tw columns (u16 per channel).
    let mut tmp = vec![0u16; h * tw * 3];
    for y in 0..h {
        let row = &src[y * w * 4..(y + 1) * w * 4];
        let trow = &mut tmp[y * tw * 3..(y + 1) * tw * 3];
        for (dx, &(x0, x1)) in xr.iter().enumerate() {
            let mut b = 0u32;
            let mut g = 0u32;
            let mut r = 0u32;
            for x in x0..x1 {
                let p = &row[x * 4..x * 4 + 3];
                b += p[0] as u32;
                g += p[1] as u32;
                r += p[2] as u32;
            }
            let n = (x1 - x0) as u32;
            trow[dx * 3] = (b / n) as u16;
            trow[dx * 3 + 1] = (g / n) as u16;
            trow[dx * 3 + 2] = (r / n) as u16;
        }
    }
    // Pass 2: vertical reduction.
    let mut out = vec![0u8; tw * th * 4];
    for (dy, &(y0, y1)) in yr.iter().enumerate() {
        let n = (y1 - y0) as u32;
        for dx in 0..tw {
            let mut b = 0u32;
            let mut g = 0u32;
            let mut r = 0u32;
            for y in y0..y1 {
                let p = &tmp[(y * tw + dx) * 3..(y * tw + dx) * 3 + 3];
                b += p[0] as u32;
                g += p[1] as u32;
                r += p[2] as u32;
            }
            let o = (dy * tw + dx) * 4;
            out[o] = (b / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (r / n) as u8;
            out[o + 3] = 255;
        }
    }
    out
}

/// Anti-aliased coverage of a rounded rectangle at pixel centre (px, py).
fn rounded_coverage(px: f32, py: f32, l: f32, t: f32, r: f32, b: f32, radius: f32) -> f32 {
    let cx = (l + r) * 0.5;
    let cy = (t + b) * 0.5;
    let hw = (r - l) * 0.5 - radius;
    let hh = (b - t) * 0.5 - radius;
    let qx = (px - cx).abs() - hw;
    let qy = (py - cy).abs() - hh;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    let d = (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - radius;
    (0.5 - d).clamp(0.0, 1.0)
}

fn box_blur(buf: &mut [f32], w: usize, h: usize, r: usize) {
    if r == 0 {
        return;
    }
    let mut tmp = vec![0f32; w * h];
    let norm = 1.0 / (2 * r + 1) as f32;
    // horizontal
    for y in 0..h {
        let row = &buf[y * w..(y + 1) * w];
        let mut acc = 0.0;
        for x in 0..=r.min(w - 1) {
            acc += row[x];
        }
        for x in 0..w {
            tmp[y * w + x] = acc * norm;
            let add = x + r + 1;
            if add < w {
                acc += row[add];
            }
            if x >= r {
                acc -= row[x - r];
            }
        }
    }
    // vertical
    for x in 0..w {
        let mut acc = 0.0;
        for y in 0..=r.min(h - 1) {
            acc += tmp[y * w + x];
        }
        for y in 0..h {
            buf[y * w + x] = acc * norm;
            let add = y + r + 1;
            if add < h {
                acc += tmp[add * w + x];
            }
            if y >= r {
                acc -= tmp[(y - r) * w + x];
            }
        }
    }
}

/// `scale` is the DPI scale of the target monitor (1.0 at 96 dpi).
pub fn build(img: &Image, max_w: i32, max_h: i32, scale: f32) -> ThumbImage {
    let iw = img.width.max(1) as f32;
    let ih = img.height.max(1) as f32;
    let s = (max_w as f32 / iw).min(max_h as f32 / ih).min(1.0);
    let tw = ((iw * s).round() as usize).max(1);
    let th = ((ih * s).round() as usize).max(1);
    let small = downscale(img, tw, th);

    let border = (1.5 * scale).round().max(1.0) as i32;
    let pad = (16.0 * scale).round() as i32;
    let radius = 7.0 * scale;
    let content_w = tw as i32 + 2 * border;
    let content_h = th as i32 + 2 * border;
    let w = content_w + 2 * pad;
    let h = content_h + 2 * pad;
    let (wu, hu) = (w as usize, h as usize);

    // Shadow: rounded rect, offset down, blurred.
    let shadow_dy = 3.0 * scale;
    let mut shadow = vec![0f32; wu * hu];
    let (cl, ct, cr, cb) = (
        pad as f32,
        pad as f32 + shadow_dy,
        (pad + content_w) as f32,
        (pad + content_h) as f32 + shadow_dy,
    );
    for y in 0..hu {
        for x in 0..wu {
            shadow[y * wu + x] = rounded_coverage(x as f32 + 0.5, y as f32 + 0.5, cl, ct, cr, cb, radius);
        }
    }
    let blur_r = (4.0 * scale).round() as usize;
    box_blur(&mut shadow, wu, hu, blur_r);
    box_blur(&mut shadow, wu, hu, blur_r);
    box_blur(&mut shadow, wu, hu, blur_r);

    let (ol, ot, or, ob) = (
        pad as f32,
        pad as f32,
        (pad + content_w) as f32,
        (pad + content_h) as f32,
    );
    let (il, it, ir, ib) = (
        (pad + border) as f32,
        (pad + border) as f32,
        (pad + content_w - border) as f32,
        (pad + content_h - border) as f32,
    );
    let inner_radius = (radius - border as f32).max(0.0);

    let mut out = vec![0u8; wu * hu * 4];
    for y in 0..hu {
        for x in 0..wu {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let sa = shadow[y * wu + x] * SHADOW_STRENGTH;
            let c = rounded_coverage(px, py, ol, ot, or, ob, radius);
            let mut color = [0f32; 3];
            if c > 0.0 {
                let inner = rounded_coverage(px, py, il, it, ir, ib, inner_radius);
                let sx = (x as i32 - pad - border).clamp(0, tw as i32 - 1) as usize;
                let sy = (y as i32 - pad - border).clamp(0, th as i32 - 1) as usize;
                let p = &small[(sy * tw + sx) * 4..(sy * tw + sx) * 4 + 3];
                for ch in 0..3 {
                    color[ch] = inner * p[ch] as f32 + (1.0 - inner) * BORDER_COLOR[ch];
                }
            }
            let alpha = c + (1.0 - c) * sa;
            let o = (y * wu + x) * 4;
            // Premultiplied: content colour * c; the shadow is black so it adds nothing.
            out[o] = (color[0] * c).round().clamp(0.0, 255.0) as u8;
            out[o + 1] = (color[1] * c).round().clamp(0.0, 255.0) as u8;
            out[o + 2] = (color[2] * c).round().clamp(0.0, 255.0) as u8;
            out[o + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    ThumbImage {
        w,
        h,
        pad,
        content_w,
        content_h,
        bgra: out,
    }
}
