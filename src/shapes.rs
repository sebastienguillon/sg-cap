//! Annotation model: vector shapes in image pixel coordinates, hit testing,
//! selection handles, Shift constraints and a snapshot-based undo history.
//! Pure Rust, no Win32: the editor window and the renderer build on it.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Line,
    Arrow,
    Rect,
    Ellipse,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Line, Kind::Arrow, Kind::Rect, Kind::Ellipse];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Line => "line",
            Kind::Arrow => "arrow",
            Kind::Rect => "rect",
            Kind::Ellipse => "ellipse",
        }
    }

    pub fn from_name(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    /// Line-like shapes have two endpoint handles; the others a bounding box.
    pub fn is_linear(self) -> bool {
        matches!(self, Kind::Line | Kind::Arrow)
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

impl Pt {
    pub const fn new(x: f32, y: f32) -> Pt {
        Pt { x, y }
    }

    pub fn add(self, o: Pt) -> Pt {
        Pt::new(self.x + o.x, self.y + o.y)
    }

    pub fn sub(self, o: Pt) -> Pt {
        Pt::new(self.x - o.x, self.y - o.y)
    }

    pub fn mul(self, k: f32) -> Pt {
        Pt::new(self.x * k, self.y * k)
    }

    pub fn dot(self, o: Pt) -> f32 {
        self.x * o.x + self.y * o.y
    }

    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub fn dist(self, o: Pt) -> f32 {
        self.sub(o).len()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    /// 0xRRGGBB.
    pub color: u32,
    /// Stroke width in image pixels.
    pub width: f32,
}

/// Selection handles. Lines use A/B (their endpoints); boxes use the four
/// corners and four edge midpoints of their normalised bounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handle {
    A,
    B,
    NW,
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
}

impl Handle {
    pub const BOX: [Handle; 8] = [
        Handle::NW,
        Handle::N,
        Handle::NE,
        Handle::E,
        Handle::SE,
        Handle::S,
        Handle::SW,
        Handle::W,
    ];
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Shape {
    pub kind: Kind,
    /// Start point (lines) or one corner (boxes).
    pub a: Pt,
    /// End point, where the arrowhead sits (lines), or the opposite corner (boxes).
    pub b: Pt,
    pub style: Style,
}

/// Arrowhead triangle plus the point where the shaft stops: far enough inside
/// the head that the shaft's round cap never pokes out of it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ArrowHead {
    pub tip: Pt,
    pub left: Pt,
    pub right: Pt,
    pub shaft_end: Pt,
    /// False for arrows too short for a shaft: just the triangle, from a to b.
    pub shaft: bool,
}

/// Head length grows with the stroke but takes at most 3/4 of the arrow; the
/// head is always wider than the shaft.
pub fn arrow_head(a: Pt, b: Pt, width: f32) -> Option<ArrowHead> {
    let d = b.sub(a);
    let len = d.len();
    if len < 0.5 {
        return None;
    }
    let u = d.mul(1.0 / len);
    let n = Pt::new(-u.y, u.x);
    let geometry = |head_len: f32| {
        let half_w = (head_len * 0.55).max(width * 0.75);
        // The cap (radius width/2) fits in the head's wedge once its centre is
        // (width/2) / sin(half angle) back from the tip.
        let sin = half_w / (head_len * head_len + half_w * half_w).sqrt();
        let back = (head_len * 0.6).max(width * 0.5 / sin);
        (half_w, back)
    };
    let mut head_len = (width * 3.0 + 9.0).min(len * 0.75);
    let (mut half_w, back) = geometry(head_len);
    let shaft = back + 0.5 < len;
    if !shaft {
        head_len = len;
        half_w = geometry(len).0;
    }
    let base = b.sub(u.mul(head_len));
    Some(ArrowHead {
        tip: b,
        left: base.add(n.mul(half_w)),
        right: base.sub(n.mul(half_w)),
        shaft_end: if shaft { b.sub(u.mul(back)) } else { a },
        shaft,
    })
}

/// Distance from `p` to the segment a-b.
pub fn dist_to_segment(p: Pt, a: Pt, b: Pt) -> f32 {
    let ab = b.sub(a);
    let l2 = ab.dot(ab);
    if l2 <= f32::EPSILON {
        return p.dist(a);
    }
    let t = (p.sub(a).dot(ab) / l2).clamp(0.0, 1.0);
    p.dist(a.add(ab.mul(t)))
}

fn cross(o: Pt, a: Pt, b: Pt) -> f32 {
    (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
}

fn in_triangle(p: Pt, a: Pt, b: Pt, c: Pt) -> bool {
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

/// Do segments p-q and r-s intersect (touching counts)?
fn segments_cross(p: Pt, q: Pt, r: Pt, s: Pt) -> bool {
    let d1 = cross(r, s, p);
    let d2 = cross(r, s, q);
    let d3 = cross(p, q, r);
    let d4 = cross(p, q, s);
    (d1 * d2 <= 0.0) && (d3 * d4 <= 0.0)
}

/// Does the segment a-b, thickened by `pad`, reach the w x h image?
fn segment_touches_rect(a: Pt, b: Pt, pad: f32, w: f32, h: f32) -> bool {
    let grown = |p: Pt| p.x >= -pad && p.x <= w + pad && p.y >= -pad && p.y <= h + pad;
    if grown(a) || grown(b) {
        return true;
    }
    let r = [Pt::new(-pad, -pad), Pt::new(w + pad, -pad), Pt::new(w + pad, h + pad), Pt::new(-pad, h + pad)];
    (0..4).any(|i| segments_cross(a, b, r[i], r[(i + 1) % 4]))
}

/// Does the triangle overlap the w x h image?
fn triangle_touches_rect(t: [Pt; 3], w: f32, h: f32) -> bool {
    let inside = |p: &Pt| p.x >= 0.0 && p.x <= w && p.y >= 0.0 && p.y <= h;
    let r = [Pt::new(0.0, 0.0), Pt::new(w, 0.0), Pt::new(w, h), Pt::new(0.0, h)];
    t.iter().any(inside)
        || r.iter().any(|&c| in_triangle(c, t[0], t[1], t[2]))
        || (0..3).any(|i| (0..4).any(|j| segments_cross(t[i], t[(i + 1) % 3], r[j], r[(j + 1) % 4])))
}

/// Distance from `p` to the outline of the axis-aligned box [lo, hi].
fn dist_to_box_outline(p: Pt, lo: Pt, hi: Pt) -> f32 {
    let inside = p.x >= lo.x && p.x <= hi.x && p.y >= lo.y && p.y <= hi.y;
    if inside {
        (p.x - lo.x).min(hi.x - p.x).min(p.y - lo.y).min(hi.y - p.y)
    } else {
        let dx = (lo.x - p.x).max(0.0).max(p.x - hi.x);
        let dy = (lo.y - p.y).max(0.0).max(p.y - hi.y);
        (dx * dx + dy * dy).sqrt()
    }
}

/// Distance from `p` to the ellipse inscribed in [lo, hi], inside or outside.
fn dist_to_ellipse_outline(p: Pt, lo: Pt, hi: Pt) -> f32 {
    let rx = (hi.x - lo.x) * 0.5;
    let ry = (hi.y - lo.y) * 0.5;
    let c = Pt::new((lo.x + hi.x) * 0.5, (lo.y + hi.y) * 0.5);
    if rx < 1.0 || ry < 1.0 {
        // Degenerate: a flat ellipse is drawn as a line through its long axis.
        return if rx >= ry {
            dist_to_segment(p, Pt::new(lo.x, c.y), Pt::new(hi.x, c.y))
        } else {
            dist_to_segment(p, Pt::new(c.x, lo.y), Pt::new(c.x, hi.y))
        };
    }
    // Closest point on the outline (first quadrant by symmetry), refined along
    // the ellipse's evolute until it settles. In f64: on flat ellipses the two
    // terms of each update nearly cancel and f32 loses whole pixels.
    let (rx, ry) = (rx as f64, ry as f64);
    let (px, py) = ((p.x - c.x).abs() as f64, (p.y - c.y).abs() as f64);
    let (mut tx, mut ty) = (std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2);
    for _ in 0..8 {
        let (x, y) = (rx * tx, ry * ty);
        let ex = (rx * rx - ry * ry) * tx.powi(3) / rx;
        let ey = (ry * ry - rx * rx) * ty.powi(3) / ry;
        let (qx, qy) = (px - ex, py - ey);
        let q = (qx * qx + qy * qy).sqrt();
        if q < 1e-12 {
            break;
        }
        let r = ((x - ex).powi(2) + (y - ey).powi(2)).sqrt();
        let nx = ((qx * r / q + ex) / rx).clamp(0.0, 1.0);
        let ny = ((qy * r / q + ey) / ry).clamp(0.0, 1.0);
        let t = (nx * nx + ny * ny).sqrt();
        let (nx, ny) = (nx / t, ny / t);
        let settled = (nx - tx).abs() + (ny - ty).abs() < 1e-9;
        tx = nx;
        ty = ny;
        if settled {
            break;
        }
    }
    ((px - rx * tx).powi(2) + (py - ry * ty).powi(2)).sqrt() as f32
}

impl Shape {
    /// Normalised bounds of the two defining points (top-left, bottom-right).
    pub fn bounds(&self) -> (Pt, Pt) {
        (
            Pt::new(self.a.x.min(self.b.x), self.a.y.min(self.b.y)),
            Pt::new(self.a.x.max(self.b.x), self.a.y.max(self.b.y)),
        )
    }

    /// Makes a/b the top-left and bottom-right corners (boxes only), so the
    /// box handles can move individual edges.
    pub fn normalize(&mut self) {
        if !self.kind.is_linear() {
            let (lo, hi) = self.bounds();
            self.a = lo;
            self.b = hi;
        }
    }

    /// Too small to be worth keeping (a click rather than a drag).
    pub fn is_degenerate(&self, min_len: f32) -> bool {
        if self.kind.is_linear() {
            self.a.dist(self.b) < min_len
        } else {
            let (lo, hi) = self.bounds();
            hi.x - lo.x < min_len && hi.y - lo.y < min_len
        }
    }

    /// Whole-pixel stroke width, and axis-aligned edges on the pixel grid
    /// (pixel centres for odd widths, pixel edges for even ones), so
    /// rectangles and straight lines come out crisp instead of half-covered.
    /// The two grids are half a pixel apart and ties go opposite ways
    /// (k -> k+0.5 for odd, k+0.5 -> k for even), so switching widths back
    /// and forth does not walk the shape across the image.
    pub fn snap_to_pixels(&mut self) {
        self.style.width = self.style.width.round().max(1.0);
        let odd = self.style.width as i32 % 2 == 1;
        let snap = |v: f32| if odd { v.floor() + 0.5 } else { (v - 0.5).ceil() };
        match self.kind {
            Kind::Rect => {
                // Keep the size a whole number of pixels, rounded the same way
                // in every direction, so a Shift square stays square.
                let d = Pt::new((self.b.x - self.a.x).round(), (self.b.y - self.a.y).round());
                self.a = Pt::new(snap(self.a.x), snap(self.a.y));
                self.b = self.a.add(d);
            }
            Kind::Line | Kind::Arrow => {
                if (self.a.y - self.b.y).abs() < 1e-3 {
                    let y = snap(self.a.y);
                    self.a.y = y;
                    self.b.y = y;
                }
                if (self.a.x - self.b.x).abs() < 1e-3 {
                    let x = snap(self.a.x);
                    self.a.x = x;
                    self.b.x = x;
                }
            }
            Kind::Ellipse => {}
        }
    }

    /// Does any of the stroke land on a w x h image? Shapes that do not would
    /// be invisible yet still count as annotations.
    pub fn touches_image(&self, w: f32, h: f32) -> bool {
        let (lo, hi) = self.bounds();
        let pad = self.style.width * 0.5;
        let corners = [Pt::new(0.0, 0.0), Pt::new(w, 0.0), Pt::new(w, h), Pt::new(0.0, h)];
        match self.kind {
            Kind::Line => segment_touches_rect(self.a, self.b, pad, w, h),
            Kind::Arrow => match arrow_head(self.a, self.b, self.style.width) {
                Some(head) => {
                    (head.shaft && segment_touches_rect(self.a, head.shaft_end, pad, w, h))
                        || triangle_touches_rect([head.tip, head.left, head.right], w, h)
                }
                None => segment_touches_rect(self.a, self.b, pad, w, h),
            },
            Kind::Rect => {
                let overlaps = hi.x + pad > 0.0 && lo.x - pad < w && hi.y + pad > 0.0 && lo.y - pad < h;
                // An outline can also surround the whole image without crossing it.
                let encloses = lo.x + pad < 0.0 && lo.y + pad < 0.0 && hi.x - pad > w && hi.y - pad > h;
                overlaps && !encloses
            }
            Kind::Ellipse => {
                let c = Pt::new((lo.x + hi.x) * 0.5, (lo.y + hi.y) * 0.5);
                let (rx, ry) = ((hi.x - lo.x) * 0.5, (hi.y - lo.y) * 0.5);
                let within = |p: Pt, rx: f32, ry: f32| {
                    rx > 0.0 && ry > 0.0 && ((p.x - c.x) / rx).powi(2) + ((p.y - c.y) / ry).powi(2) <= 1.0
                };
                // Some image point must reach the outer edge of the stroke: the
                // image point nearest the centre (exact once both axes are
                // scaled to a circle)...
                let nearest = Pt::new(c.x.clamp(0.0, w), c.y.clamp(0.0, h));
                let reaches = within(nearest, rx + pad, ry + pad);
                // ...and the image must not sit entirely inside the hole: every
                // corner inside the outline and farther from it than the stroke
                // reaches (the nearest image point to an outline enclosing the
                // image is a corner).
                let in_hole = corners
                    .iter()
                    .all(|&p| within(p, rx, ry) && dist_to_ellipse_outline(p, lo, hi) > pad);
                reaches && !in_hole
            }
        }
    }

    /// Does `p` touch the drawn stroke? `tol` is extra slack in image pixels.
    /// Rectangles and ellipses are outlines: their inside does not count, so
    /// new shapes can still be drawn within them.
    pub fn hit(&self, p: Pt, tol: f32) -> bool {
        let reach = self.style.width * 0.5 + tol;
        match self.kind {
            Kind::Line => dist_to_segment(p, self.a, self.b) <= reach,
            Kind::Arrow => {
                if dist_to_segment(p, self.a, self.b) <= reach {
                    return true;
                }
                match arrow_head(self.a, self.b, self.style.width) {
                    Some(h) => {
                        in_triangle(p, h.tip, h.left, h.right)
                            || dist_to_segment(p, h.tip, h.left) <= tol
                            || dist_to_segment(p, h.tip, h.right) <= tol
                            || dist_to_segment(p, h.left, h.right) <= tol
                    }
                    None => false,
                }
            }
            Kind::Rect => {
                let (lo, hi) = self.bounds();
                dist_to_box_outline(p, lo, hi) <= reach
            }
            Kind::Ellipse => {
                let (lo, hi) = self.bounds();
                dist_to_ellipse_outline(p, lo, hi) <= reach
            }
        }
    }

    pub fn handles(&self) -> Vec<(Handle, Pt)> {
        if self.kind.is_linear() {
            return vec![(Handle::A, self.a), (Handle::B, self.b)];
        }
        let (lo, hi) = self.bounds();
        let mx = (lo.x + hi.x) * 0.5;
        let my = (lo.y + hi.y) * 0.5;
        Handle::BOX
            .iter()
            .map(|&h| {
                let p = match h {
                    Handle::NW => Pt::new(lo.x, lo.y),
                    Handle::N => Pt::new(mx, lo.y),
                    Handle::NE => Pt::new(hi.x, lo.y),
                    Handle::E => Pt::new(hi.x, my),
                    Handle::SE => Pt::new(hi.x, hi.y),
                    Handle::S => Pt::new(mx, hi.y),
                    Handle::SW => Pt::new(lo.x, hi.y),
                    Handle::W => Pt::new(lo.x, my),
                    Handle::A | Handle::B => unreachable!(),
                };
                (h, p)
            })
            .collect()
    }

    /// Handle within `radius` of `p`, nearest first.
    pub fn handle_at(&self, p: Pt, radius: f32) -> Option<Handle> {
        self.handles()
            .into_iter()
            .map(|(h, q)| (h, q.dist(p)))
            .filter(|&(_, d)| d <= radius)
            .min_by(|x, y| x.1.total_cmp(&y.1))
            .map(|(h, _)| h)
    }

    pub fn translate(&mut self, d: Pt) {
        self.a = self.a.add(d);
        self.b = self.b.add(d);
    }

    /// Moves handle `h` to `p`. Boxes must have been normalised when the drag
    /// started; they may flip while dragging, which `bounds()` absorbs.
    /// `constrain` (Shift) snaps lines to 45 degrees and keeps corner drags square.
    pub fn drag_handle(&mut self, h: Handle, p: Pt, constrain: bool) {
        match h {
            Handle::A => self.a = if constrain { snap_45(self.b, p) } else { p },
            Handle::B => self.b = if constrain { snap_45(self.a, p) } else { p },
            Handle::NW => self.a = if constrain { square(self.b, p) } else { p },
            Handle::SE => self.b = if constrain { square(self.a, p) } else { p },
            Handle::NE => {
                let fixed = Pt::new(self.a.x, self.b.y);
                let q = if constrain { square(fixed, p) } else { p };
                self.a.y = q.y;
                self.b.x = q.x;
            }
            Handle::SW => {
                let fixed = Pt::new(self.b.x, self.a.y);
                let q = if constrain { square(fixed, p) } else { p };
                self.a.x = q.x;
                self.b.y = q.y;
            }
            Handle::N => self.a.y = p.y,
            Handle::S => self.b.y = p.y,
            Handle::W => self.a.x = p.x,
            Handle::E => self.b.x = p.x,
        }
    }

    /// Sets the end point while the shape is being drawn.
    pub fn set_end(&mut self, p: Pt, constrain: bool) {
        self.b = match (constrain, self.kind.is_linear()) {
            (false, _) => p,
            (true, true) => snap_45(self.a, p),
            (true, false) => square(self.a, p),
        };
    }
}

/// Closest point to `p` on the ray from `anchor` along the nearest multiple of 45 degrees.
pub fn snap_45(anchor: Pt, p: Pt) -> Pt {
    let d = p.sub(anchor);
    if d.len() < 1e-6 {
        return p;
    }
    let step = std::f32::consts::FRAC_PI_4;
    let k = (d.y.atan2(d.x) / step).round() as i32;
    let (ux, uy) = match k.rem_euclid(8) {
        0 => (1.0, 0.0),
        1 => (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2),
        2 => (0.0, 1.0),
        3 => (-std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2),
        4 => (-1.0, 0.0),
        5 => (-std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2),
        6 => (0.0, -1.0),
        _ => (std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2),
    };
    let u = Pt::new(ux, uy);
    anchor.add(u.mul(d.dot(u).max(0.0)))
}

/// `p` moved so that the box from `anchor` to it is a square (the larger side wins).
pub fn square(anchor: Pt, p: Pt) -> Pt {
    let d = p.sub(anchor);
    let side = d.x.abs().max(d.y.abs());
    let sx = if d.x < 0.0 { -1.0 } else { 1.0 };
    let sy = if d.y < 0.0 { -1.0 } else { 1.0 };
    Pt::new(anchor.x + sx * side, anchor.y + sy * side)
}

const MAX_UNDO: usize = 200;

/// The shapes drawn on one capture, with undo/redo as whole-list snapshots
/// (shape lists are tiny, so snapshots are simpler than inverse operations).
#[derive(Default)]
pub struct Doc {
    pub shapes: Vec<Shape>,
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
}

impl Doc {
    /// Records `before` as one undo step if the shapes changed since then.
    pub fn commit(&mut self, before: Vec<Shape>) -> bool {
        if before == self.shapes {
            return false;
        }
        self.undo.push(before);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        true
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut self.shapes, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut self.shapes, next));
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Topmost shape under `p`.
    pub fn shape_at(&self, p: Pt, tol: f32) -> Option<usize> {
        self.shapes.iter().rposition(|s| s.hit(p, tol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Style = Style {
        color: 0xFF3B30,
        width: 4.0,
    };

    fn shape(kind: Kind, a: (f32, f32), b: (f32, f32)) -> Shape {
        Shape {
            kind,
            a: Pt::new(a.0, a.1),
            b: Pt::new(b.0, b.1),
            style: RED,
        }
    }

    fn close(p: Pt, x: f32, y: f32) -> bool {
        (p.x - x).abs() < 1e-3 && (p.y - y).abs() < 1e-3
    }

    #[test]
    fn kind_names_round_trip() {
        for k in Kind::ALL {
            assert_eq!(Kind::from_name(k.name()), Some(k));
        }
        assert_eq!(Kind::from_name("star"), None);
    }

    #[test]
    fn line_hit_respects_width_and_tolerance() {
        let s = shape(Kind::Line, (10.0, 10.0), (110.0, 10.0));
        assert!(s.hit(Pt::new(60.0, 10.0), 0.0));
        assert!(s.hit(Pt::new(60.0, 12.0), 0.0)); // within half the 4 px stroke
        assert!(!s.hit(Pt::new(60.0, 15.0), 0.0));
        assert!(s.hit(Pt::new(60.0, 15.0), 3.5));
        assert!(!s.hit(Pt::new(120.0, 10.0), 3.0)); // past the end cap
    }

    #[test]
    fn arrow_head_is_hit_and_shaft_stops_inside_it() {
        let s = shape(Kind::Arrow, (0.0, 0.0), (100.0, 0.0));
        let h = arrow_head(s.a, s.b, s.style.width).unwrap();
        assert!(close(h.tip, 100.0, 0.0));
        // Head is wider than the stroke, so points beside the tip area hit.
        let beside = Pt::new(90.0, 4.0);
        assert!(s.hit(beside, 0.0));
        assert!(!s.hit(Pt::new(60.0, 4.0), 0.0));
        // The shaft (with its round cap) ends inside the triangle.
        assert!(h.shaft_end.x < h.tip.x);
        assert!(h.shaft_end.x + s.style.width * 0.5 < h.tip.x);
        assert!(h.shaft_end.x > h.left.x);
    }

    #[test]
    fn tiny_arrow_head_stays_within_the_arrow() {
        let h = arrow_head(Pt::new(0.0, 0.0), Pt::new(8.0, 0.0), 12.0).unwrap();
        assert!(h.left.x >= 0.0 - 1e-3);
        assert!(arrow_head(Pt::new(5.0, 5.0), Pt::new(5.0, 5.0), 4.0).is_none());
    }

    /// Distance from `p` to the infinite line through a and b.
    fn dist_to_line(p: Pt, a: Pt, b: Pt) -> f32 {
        let d = b.sub(a);
        (cross(a, b, p) / d.len()).abs()
    }

    #[test]
    fn thick_short_arrow_keeps_its_cap_inside_the_head() {
        let (a, b, w) = (Pt::new(0.0, 0.0), Pt::new(20.0, 0.0), 24.0);
        let h = arrow_head(a, b, w).unwrap();
        assert!(h.left.y - h.right.y >= 1.5 * w - 1e-3, "head wider than the shaft");
        assert!(h.shaft);
        // The round cap (radius w/2 around shaft_end) stays within both head edges.
        assert!(dist_to_line(h.shaft_end, h.tip, h.left) >= w * 0.5 - 1e-3);
        assert!(dist_to_line(h.shaft_end, h.tip, h.right) >= w * 0.5 - 1e-3);
    }

    #[test]
    fn very_short_arrow_is_just_a_triangle() {
        let (a, b) = (Pt::new(10.0, 10.0), Pt::new(15.0, 10.0));
        let h = arrow_head(a, b, 12.0).unwrap();
        assert!(!h.shaft);
        assert!(close(h.shaft_end, 10.0, 10.0));
        assert!((h.left.x - 10.0).abs() < 1e-3, "head base at the start point");
    }

    #[test]
    fn ellipse_distance_is_accurate() {
        // Circle of radius 50 centred on (100, 50): 30 px from the centre is 20 from the outline.
        let d = dist_to_ellipse_outline(Pt::new(130.0, 50.0), Pt::new(50.0, 0.0), Pt::new(150.0, 100.0));
        assert!((d - 20.0).abs() < 0.05, "{d}");
        // Eccentric ellipse (rx 100, ry 10): (80, 0) from the centre is ~5.95 from the outline.
        let d = dist_to_ellipse_outline(Pt::new(180.0, 10.0), Pt::new(0.0, 0.0), Pt::new(200.0, 20.0));
        assert!((d - 5.95).abs() < 0.1, "{d}");
        let mut s = shape(Kind::Ellipse, (0.0, 0.0), (200.0, 20.0));
        s.style.width = 12.0;
        assert!(s.hit(Pt::new(180.0, 10.0), 0.5), "painted stroke pixel selects the shape");
    }

    #[test]
    fn snapping_puts_edges_on_the_pixel_grid() {
        let mut r = shape(Kind::Rect, (10.5, 20.5), (100.5, 50.5)); // width 4: even
        r.snap_to_pixels();
        assert!(close(r.a, 10.0, 20.0) && close(r.b, 100.0, 50.0));
        r.style.width = 3.0; // odd: pixel centres
        r.a = Pt::new(10.2, 20.7);
        r.snap_to_pixels();
        assert!(close(r.a, 10.5, 20.5));
        assert!(close(r.b, 100.5, 49.5), "size kept whole: 90 x 29");
        let mut l = shape(Kind::Line, (0.0, 10.2), (50.0, 10.2));
        l.style.width = 2.6;
        l.snap_to_pixels();
        assert_eq!(l.style.width, 3.0);
        assert!(close(l.a, 0.0, 10.5) && close(l.b, 50.0, 10.5));
        let mut d = shape(Kind::Arrow, (0.3, 0.3), (40.7, 30.2));
        d.snap_to_pixels();
        assert!(close(d.a, 0.3, 0.3) && close(d.b, 40.7, 30.2), "diagonals are left alone");
    }

    #[test]
    fn toggling_width_parity_does_not_walk_the_shape() {
        let mut r = shape(Kind::Rect, (101.0, 201.0), (141.0, 241.0));
        let mut l = shape(Kind::Line, (10.0, 50.0), (90.0, 50.0));
        for _ in 0..3 {
            for w in [7.0, 4.0] {
                r.style.width = w;
                r.snap_to_pixels();
                l.style.width = w;
                l.snap_to_pixels();
            }
        }
        assert!(close(r.a, 101.0, 201.0) && close(r.b, 141.0, 241.0), "{:?}", r);
        assert!(close(l.a, 10.0, 50.0) && close(l.b, 90.0, 50.0), "{:?}", l);
    }

    #[test]
    fn shift_square_stays_square_after_snapping() {
        // Even width: anchor on a pixel edge, cursor on a pixel centre, dragged up-right.
        let mut r = shape(Kind::Rect, (100.0, 200.0), (100.0, 200.0));
        r.set_end(Pt::new(140.5, 170.5), true);
        r.snap_to_pixels();
        let (lo, hi) = r.bounds();
        assert_eq!(hi.x - lo.x, hi.y - lo.y, "{:?}", r);
    }

    #[test]
    fn flat_ellipse_distance_is_accurate() {
        // rx 1000, ry 1: 485 px from the centre along the long axis is ~0.87 from the outline.
        let d = dist_to_ellipse_outline(Pt::new(1485.0, 1.0), Pt::new(0.0, 0.0), Pt::new(2000.0, 2.0));
        assert!((d - 0.87).abs() < 0.05, "{d}");
        let d = dist_to_ellipse_outline(Pt::new(7680.0 / 2.0 + 3000.0, 1.0), Pt::new(0.0, 0.0), Pt::new(7680.0, 2.0));
        assert!(d < 1.5, "{d}");
    }

    #[test]
    fn invisible_arrows_and_ellipses_are_refused() {
        let on = |s: Shape| s.touches_image(100.0, 100.0);
        // Tip 15 px short of the left edge: nothing painted on the image.
        assert!(!on(shape(Kind::Arrow, (-60.0, 50.0), (-15.0, 50.0))));
        // Vertical arrow 20 px to the left: the head reaches x = -8.45 only.
        assert!(!on(shape(Kind::Arrow, (-20.0, 10.0), (-20.0, 90.0))));
        // Its head does reach in when 8 px away.
        assert!(on(shape(Kind::Arrow, (-8.0, 10.0), (-8.0, 90.0))));
        // A thick ellipse whose stroke just covers the image corners counts
        // (the corners sit inside the outline, 5.2 px from it, stroke 6 px).
        let mut wide = shape(Kind::Ellipse, (200.0 - 280.5, 50.0 - 79.0), (200.0 + 280.5, 50.0 + 79.0));
        wide.style.width = 12.0;
        assert!(wide.touches_image(400.0, 100.0));
        // Circle off the top-left corner whose box overlaps the image.
        assert!(!on(shape(Kind::Ellipse, (-65.0, -65.0), (5.0, 5.0))));
        assert!(on(shape(Kind::Ellipse, (-30.0, -30.0), (30.0, 30.0))));
    }

    #[test]
    fn shapes_off_the_image_are_detected() {
        let on = |s: Shape| s.touches_image(100.0, 100.0);
        assert!(!on(shape(Kind::Line, (-50.0, -50.0), (-10.0, -10.0))));
        assert!(on(shape(Kind::Line, (-50.0, 50.0), (150.0, 50.0))));
        let mut corner = shape(Kind::Line, (-10.0, 5.0), (5.0, -10.0));
        corner.style.width = 2.0;
        assert!(!on(corner), "passes outside the corner");
        assert!(!on(shape(Kind::Rect, (-20.0, -20.0), (120.0, 120.0))), "frames the image from outside");
        assert!(on(shape(Kind::Rect, (50.0, 50.0), (150.0, 150.0))));
        assert!(!on(shape(Kind::Rect, (200.0, 200.0), (300.0, 300.0))));
        assert!(!on(shape(Kind::Ellipse, (-50.0, -50.0), (150.0, 150.0))), "circle around the image");
        assert!(on(shape(Kind::Ellipse, (0.0, 0.0), (100.0, 100.0))));
        assert!(on(shape(Kind::Arrow, (-40.0, 50.0), (3.0, 50.0))), "an arrow pointing in from the margin counts");
    }

    #[test]
    fn rect_hit_is_outline_only() {
        let s = shape(Kind::Rect, (100.0, 100.0), (0.0, 0.0)); // reversed corners
        assert!(s.hit(Pt::new(0.0, 50.0), 0.0));
        assert!(s.hit(Pt::new(50.0, 101.5), 0.0));
        assert!(s.hit(Pt::new(97.0, 50.0), 1.5)); // just inside the right edge
        assert!(!s.hit(Pt::new(50.0, 50.0), 3.0)); // interior
        assert!(!s.hit(Pt::new(50.0, 110.0), 3.0)); // outside
    }

    #[test]
    fn ellipse_hit_is_outline_only() {
        let s = shape(Kind::Ellipse, (0.0, 0.0), (200.0, 100.0));
        assert!(s.hit(Pt::new(100.0, 0.0), 0.0)); // top
        assert!(s.hit(Pt::new(200.0, 50.0), 0.0)); // right
        let diag = Pt::new(100.0 + 100.0 * 0.70710677, 50.0 + 50.0 * 0.70710677);
        assert!(s.hit(diag, 0.5));
        assert!(!s.hit(Pt::new(100.0, 50.0), 3.0)); // centre
        assert!(!s.hit(Pt::new(200.0, 0.0), 3.0)); // bounding-box corner
        assert!(!s.hit(Pt::new(100.0, -10.0), 3.0));
    }

    #[test]
    fn flat_ellipse_is_hit_like_a_line() {
        let s = shape(Kind::Ellipse, (0.0, 50.0), (100.0, 50.5));
        assert!(s.hit(Pt::new(50.0, 51.0), 1.0));
        assert!(!s.hit(Pt::new(50.0, 60.0), 1.0));
    }

    #[test]
    fn topmost_shape_wins() {
        let mut d = Doc::default();
        d.shapes.push(shape(Kind::Line, (0.0, 0.0), (100.0, 0.0)));
        d.shapes.push(shape(Kind::Line, (50.0, -50.0), (50.0, 50.0)));
        assert_eq!(d.shape_at(Pt::new(50.0, 0.0), 2.0), Some(1));
        assert_eq!(d.shape_at(Pt::new(10.0, 0.0), 2.0), Some(0));
        assert_eq!(d.shape_at(Pt::new(10.0, 30.0), 2.0), None);
    }

    #[test]
    fn handles_for_lines_and_boxes() {
        let l = shape(Kind::Arrow, (1.0, 2.0), (30.0, 40.0));
        assert_eq!(l.handles().len(), 2);
        assert_eq!(l.handle_at(Pt::new(29.0, 39.0), 3.0), Some(Handle::B));
        assert_eq!(l.handle_at(Pt::new(15.0, 20.0), 3.0), None);
        let r = shape(Kind::Rect, (100.0, 80.0), (0.0, 0.0));
        let hs = r.handles();
        assert_eq!(hs.len(), 8);
        assert!(hs.contains(&(Handle::NW, Pt::new(0.0, 0.0))));
        assert!(hs.contains(&(Handle::E, Pt::new(100.0, 40.0))));
        assert_eq!(r.handle_at(Pt::new(51.0, 79.0), 4.0), Some(Handle::S));
    }

    #[test]
    fn dragging_box_handles_moves_edges() {
        let mut r = shape(Kind::Rect, (100.0, 80.0), (0.0, 0.0));
        r.normalize();
        assert!(close(r.a, 0.0, 0.0) && close(r.b, 100.0, 80.0));
        r.drag_handle(Handle::E, Pt::new(150.0, 999.0), false);
        assert!(close(r.b, 150.0, 80.0));
        r.drag_handle(Handle::N, Pt::new(-5.0, 10.0), false);
        assert!(close(r.a, 0.0, 10.0));
        r.drag_handle(Handle::NE, Pt::new(120.0, 20.0), false);
        assert!(close(r.a, 0.0, 20.0) && close(r.b, 120.0, 80.0));
        r.drag_handle(Handle::SW, Pt::new(10.0, 60.0), false);
        assert!(close(r.a, 10.0, 20.0) && close(r.b, 120.0, 60.0));
        // Dragging past the opposite edge flips the box instead of breaking it.
        r.drag_handle(Handle::W, Pt::new(200.0, 0.0), false);
        let (lo, hi) = r.bounds();
        assert!(close(lo, 120.0, 20.0) && close(hi, 200.0, 60.0));
    }

    #[test]
    fn constrained_corner_drag_keeps_a_square() {
        let mut r = shape(Kind::Ellipse, (0.0, 0.0), (50.0, 50.0));
        r.normalize();
        r.drag_handle(Handle::SE, Pt::new(80.0, 30.0), true);
        assert!(close(r.b, 80.0, 80.0));
        r.drag_handle(Handle::NE, Pt::new(40.0, 10.0), true);
        // fixed corner is (0, 80): 40 wide, 70 tall -> 70 square
        assert!(close(r.a, 0.0, 10.0) && close(r.b, 70.0, 80.0));
    }

    #[test]
    fn snap_45_projects_onto_the_nearest_direction() {
        let o = Pt::new(10.0, 10.0);
        assert!(close(snap_45(o, Pt::new(110.0, 14.0)), 110.0, 10.0));
        assert!(close(snap_45(o, Pt::new(12.0, -90.0)), 10.0, -90.0));
        let d = snap_45(o, Pt::new(60.0, 64.0));
        assert!((d.x - d.y).abs() < 1e-3 && d.x > 10.0);
        let w = snap_45(o, Pt::new(-90.0, 8.0));
        assert!(close(w, -90.0, 10.0));
        assert!(close(snap_45(o, o), 10.0, 10.0));
    }

    #[test]
    fn square_keeps_the_drag_direction() {
        let o = Pt::new(0.0, 0.0);
        assert!(close(square(o, Pt::new(30.0, -10.0)), 30.0, -30.0));
        assert!(close(square(o, Pt::new(-5.0, 20.0)), -20.0, 20.0));
    }

    #[test]
    fn set_end_constrains_while_drawing() {
        let mut l = shape(Kind::Line, (0.0, 0.0), (0.0, 0.0));
        l.set_end(Pt::new(100.0, 3.0), true);
        assert!(close(l.b, 100.0, 0.0));
        let mut r = shape(Kind::Rect, (0.0, 0.0), (0.0, 0.0));
        r.set_end(Pt::new(40.0, 25.0), true);
        assert!(close(r.b, 40.0, 40.0));
        r.set_end(Pt::new(40.0, 25.0), false);
        assert!(close(r.b, 40.0, 25.0));
    }

    #[test]
    fn degenerate_shapes_are_detected() {
        assert!(shape(Kind::Line, (0.0, 0.0), (1.0, 1.0)).is_degenerate(3.0));
        assert!(!shape(Kind::Line, (0.0, 0.0), (5.0, 0.0)).is_degenerate(3.0));
        assert!(!shape(Kind::Rect, (0.0, 0.0), (50.0, 1.0)).is_degenerate(3.0));
        assert!(shape(Kind::Ellipse, (0.0, 0.0), (2.0, 2.0)).is_degenerate(3.0));
    }

    #[test]
    fn undo_redo_snapshots() {
        let mut d = Doc::default();
        assert!(!d.can_undo() && !d.can_redo());
        let before = d.shapes.clone();
        d.shapes.push(shape(Kind::Line, (0.0, 0.0), (10.0, 0.0)));
        assert!(d.commit(before));
        let before = d.shapes.clone();
        d.shapes[0].style.color = 0x007AFF;
        assert!(d.commit(before));
        // An unchanged commit records nothing.
        let before = d.shapes.clone();
        assert!(!d.commit(before));

        assert!(d.undo());
        assert_eq!(d.shapes[0].style.color, 0xFF3B30);
        assert!(d.undo());
        assert!(d.shapes.is_empty());
        assert!(!d.undo());
        assert!(d.redo());
        assert_eq!(d.shapes.len(), 1);
        // A new edit drops the redo branch.
        let before = d.shapes.clone();
        d.shapes.clear();
        assert!(d.commit(before));
        assert!(!d.can_redo());
    }

    #[test]
    fn undo_history_is_bounded() {
        let mut d = Doc::default();
        for i in 0..(MAX_UNDO + 20) {
            let before = d.shapes.clone();
            d.shapes.push(shape(Kind::Line, (0.0, i as f32), (10.0, i as f32)));
            d.commit(before);
        }
        let mut n = 0;
        while d.undo() {
            n += 1;
        }
        assert_eq!(n, MAX_UNDO);
        assert_eq!(d.shapes.len(), 20);
    }
}
