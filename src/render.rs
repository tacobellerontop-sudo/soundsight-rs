//! Platform-independent bar rendering into the four edge-strip pixel buffers.
//!
//! The overlay is four thin windows (top, right, bottom, left) instead of one
//! full-screen window. Each strip's pixels are premultiplied BGRA `u32`s exactly as
//! `UpdateLayeredWindow` wants them, so the Win32 side only blits.
//!
//! Direction -> screen position: cast a ray from the screen centre at the sound's
//! angle (0 = up/front, clockwise) and take the point where it leaves the screen.
//! Front = top edge, behind = bottom edge, left/right = side edges. The bar is
//! centred there and runs *along the perimeter*, so long bars wrap around corners.
//!
//! "Edges + front/behind" splits each sound into its side component (left/right edge,
//! higher up = more in front) and its front/back component (top bar = in front,
//! bottom bar = behind, further left/right = more to that side).
//!
//! The circle style paints arcs on a small ring around the screen centre.

use crate::config::{Config, OverlayStyle};
use crate::dsp::analyzer::BINS;
use crate::dsp::direction::bin_center_deg;

pub const TOP: usize = 0;
pub const RIGHT: usize = 1;
pub const BOTTOM: usize = 2;
pub const LEFT: usize = 3;

/// Geometry of the screen and the four strips.
/// Top/bottom strips span the full width; left/right strips sit between them so no
/// pixel is covered twice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeLayout {
    pub w: i32,
    pub h: i32,
    pub t: i32,
}

/// Strip rectangle in screen coordinates: (x, y, width, height).
pub type Rect = (i32, i32, i32, i32);

impl EdgeLayout {
    pub fn new(w: i32, h: i32, thickness: i32) -> EdgeLayout {
        let w = w.max(8);
        let h = h.max(8);
        let t = thickness.clamp(1, (h / 2 - 1).max(1));
        EdgeLayout { w, h, t }
    }

    pub fn strip_rect(&self, edge: usize) -> Rect {
        let (w, h, t) = (self.w, self.h, self.t);
        match edge {
            TOP => (0, 0, w, t),
            RIGHT => (w - t, t, t, h - 2 * t),
            BOTTOM => (0, h - t, w, t),
            _ => (0, t, t, h - 2 * t),
        }
    }

    pub fn strip_len(&self, edge: usize) -> usize {
        let r = self.strip_rect(edge);
        (r.2.max(0) * r.3.max(0)) as usize
    }

    fn perimeter(&self) -> f32 {
        2.0 * (self.w + self.h) as f32
    }

    /// Perimeter coordinate (clockwise from the top-left corner) where a ray from the
    /// centre at `angle_deg` meets the screen edge.
    pub fn angle_to_perimeter(&self, angle_deg: f32) -> f32 {
        let (w, h) = (self.w as f32, self.h as f32);
        let r = angle_deg.to_radians();
        let (dx, dy) = (r.sin(), -r.cos()); // screen y grows downwards
        let tx = if dx.abs() > 1e-9 {
            (w / 2.0) / dx.abs()
        } else {
            f32::INFINITY
        };
        let ty = if dy.abs() > 1e-9 {
            (h / 2.0) / dy.abs()
        } else {
            f32::INFINITY
        };
        let t = tx.min(ty);
        let px = (w / 2.0 + t * dx).clamp(0.0, w);
        let py = (h / 2.0 + t * dy).clamp(0.0, h);
        if ty <= tx {
            if dy < 0.0 {
                px // top
            } else {
                w + h + (w - px) // bottom (runs right-to-left)
            }
        } else if dx > 0.0 {
            w + py // right
        } else {
            2.0 * w + h + (h - py) // left (runs bottom-to-top)
        }
    }
}

/// Visual parameters derived from the config.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub rgb: (u8, u8, u8),
    pub opacity: f32,
    pub min_opacity_frac: f32,
    pub bar_length: f32,
    pub min_length_frac: f32,
    pub min_intensity: f32,
    pub min_thickness_frac: f32,
    pub mode: OverlayStyle,
    pub guide_opacity: f32,
}

/// Bars whose side or front/back component is below this (|sin| or |cos|) are not
/// drawn in the front/behind style: a sound straight ahead gets no side bar, a sound
/// exactly to the side gets no front/behind bar.
const SPLIT_MIN_COMPONENT: f32 = 0.3;
/// How far from the centre of an edge the split-style bars may move (fraction of half the edge).
const SPLIT_SPREAD: f32 = 0.75;
/// Circle style: arc span in degrees for the loudest sounds.
pub const CIRCLE_MAX_ARC_DEG: f32 = 60.0;

impl Style {
    pub fn from_config(c: &Config) -> Style {
        let o = &c.overlay;
        Style {
            rgb: c.color_rgb(),
            opacity: o.opacity,
            min_opacity_frac: o.min_opacity_frac,
            bar_length: o.bar_length as f32,
            min_length_frac: o.min_length_frac,
            min_intensity: o.min_intensity,
            min_thickness_frac: o.min_thickness_frac,
            mode: o.style,
            guide_opacity: o.circle_guide_opacity,
        }
    }

    /// Premultiplied BGRA pixel for an intensity in 0..1.
    pub fn pixel(&self, intensity: f32) -> u32 {
        let a = self.opacity * (self.min_opacity_frac + (1.0 - self.min_opacity_frac) * intensity);
        let a8 = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        let pm = |c: u8| (c as u32 * a8 + 127) / 255;
        (a8 << 24) | (pm(self.rgb.0) << 16) | (pm(self.rgb.1) << 8) | pm(self.rgb.2)
    }

    /// Bar thickness in pixels for a strip of maximum thickness `t`: thin when quiet/far,
    /// full strip thickness when loud/close.
    pub fn thickness(&self, intensity: f32, t: i32) -> i32 {
        let f = self.min_thickness_frac + (1.0 - self.min_thickness_frac) * intensity;
        ((t as f32 * f).round() as i32).clamp(1, t)
    }

    pub fn length(&self, intensity: f32) -> f32 {
        self.bar_length * (self.min_length_frac + (1.0 - self.min_length_frac) * intensity)
    }
}

/// Paint one frame. `bufs[edge]` must have `layout.strip_len(edge)` pixels.
/// `has_content` tracks which strips were non-empty last frame (cleared only if needed).
/// Returns which strips changed and must be pushed with `UpdateLayeredWindow`.
pub fn paint_frame(
    layout: &EdgeLayout,
    style: &Style,
    bins: &[u8; BINS],
    bufs: &mut [&mut [u32]; 4],
    has_content: &mut [bool; 4],
) -> [bool; 4] {
    let mut drawn = [false; 4];
    for (e, buf) in bufs.iter_mut().enumerate() {
        if has_content[e] {
            buf.fill(0);
        }
    }

    let (w, h) = (layout.w as f32, layout.h as f32);
    let mut bar_at = |bufs: &mut [&mut [u32]; 4], center: f32, intensity: f32| {
        if intensity < style.min_intensity {
            return;
        }
        let half = style.length(intensity) / 2.0;
        let bar = (style.pixel(intensity), style.thickness(intensity, layout.t));
        paint_interval(layout, bufs, center - half, center + half, bar, &mut drawn);
    };

    for (i, &q) in bins.iter().enumerate() {
        let intensity = q as f32 / 255.0;
        if q == 0 || intensity < style.min_intensity {
            continue;
        }
        let deg = bin_center_deg(i, BINS);
        if style.mode != OverlayStyle::EdgesFrontBack {
            bar_at(bufs, layout.angle_to_perimeter(deg), intensity);
            continue;
        }
        let (sn, cs) = deg.to_radians().sin_cos();
        if sn.abs() >= SPLIT_MIN_COMPONENT {
            // Side bar; higher on the edge = more in front.
            let y = h / 2.0 - cs * (h / 2.0) * SPLIT_SPREAD;
            let s = if sn > 0.0 {
                w + y
            } else {
                2.0 * w + h + (h - y)
            };
            bar_at(bufs, s, intensity * sn.abs().sqrt());
        }
        if cs.abs() >= SPLIT_MIN_COMPONENT {
            // Front (top) / behind (bottom) bar; further left/right = more to that side.
            let x = w / 2.0 + sn * (w / 2.0) * SPLIT_SPREAD;
            let s = if cs > 0.0 { x } else { w + h + (w - x) };
            bar_at(bufs, s, intensity * cs.abs().sqrt());
        }
    }

    let mut changed = [false; 4];
    for e in 0..4 {
        changed[e] = has_content[e] || drawn[e];
        has_content[e] = drawn[e];
    }
    changed
}

/// Paint perimeter interval [a, b) (may extend past either end; wraps around).
fn paint_interval(
    l: &EdgeLayout,
    bufs: &mut [&mut [u32]; 4],
    a: f32,
    b: f32,
    bar: (u32, i32),
    drawn: &mut [bool; 4],
) {
    let p = l.perimeter();
    let len = (b - a).min(p);
    let a = a.rem_euclid(p);
    let b = a + len;
    paint_segment(l, bufs, a, b.min(p), bar, drawn);
    if b > p {
        paint_segment(l, bufs, 0.0, b - p, bar, drawn);
    }
}

/// Paint [a, b) with 0 <= a <= b <= perimeter. `bar` = (pixel, thickness); bars hug
/// the outer screen edge and grow inwards.
fn paint_segment(
    l: &EdgeLayout,
    bufs: &mut [&mut [u32]; 4],
    a: f32,
    b: f32,
    bar: (u32, i32),
    drawn: &mut [bool; 4],
) {
    let (px, th) = bar;
    let (w, h, t) = (l.w as f32, l.h as f32, l.t);
    // (edge, perimeter start, perimeter end)
    let edges = [
        (TOP, 0.0, w),
        (RIGHT, w, w + h),
        (BOTTOM, w + h, 2.0 * w + h),
        (LEFT, 2.0 * w + h, 2.0 * (w + h)),
    ];
    for (edge, e0, e1) in edges {
        let s0 = a.max(e0);
        let s1 = b.min(e1);
        if s1 <= s0 {
            continue;
        }
        // Convert to a range along the edge in screen pixels, increasing order.
        let (lo, hi) = match edge {
            TOP => (s0 - e0, s1 - e0),
            RIGHT => (s0 - e0, s1 - e0),
            BOTTOM => (w - (s1 - e0), w - (s0 - e0)),
            _ => (h - (s1 - e0), h - (s0 - e0)),
        };
        let (lo, hi) = (lo.round() as i32, hi.round() as i32);
        let rect = l.strip_rect(edge);
        let buf = &mut *bufs[edge];
        let (sw, sh) = (rect.2, rect.3);
        let painted = match edge {
            // Horizontal strips: lo..hi are x. Outer edge is row 0 (top) / last row (bottom).
            TOP => fill(buf, sw, sh, lo, hi, 0, th, px),
            BOTTOM => fill(buf, sw, sh, lo, hi, sh - th, sh, px),
            // Vertical strips: lo..hi are screen y; the strip starts at y = t.
            RIGHT => fill(buf, sw, sh, sw - th, sw, lo - t, hi - t, px),
            _ => fill(buf, sw, sh, 0, th, lo - t, hi - t, px),
        };
        drawn[edge] |= painted;
    }
}

/// Fill [x0,x1) x [y0,y1) (clipped) keeping the more opaque pixel. Returns true if anything was painted.
#[allow(clippy::too_many_arguments)]
fn fill(buf: &mut [u32], sw: i32, sh: i32, x0: i32, x1: i32, y0: i32, y1: i32, px: u32) -> bool {
    let (x0, x1) = (x0.clamp(0, sw), x1.clamp(0, sw));
    let (y0, y1) = (y0.clamp(0, sh), y1.clamp(0, sh));
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    let a = px >> 24;
    for y in y0..y1 {
        let row = &mut buf[(y * sw + x0) as usize..(y * sw + x1) as usize];
        for p in row {
            if (*p >> 24) < a {
                *p = px;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Circle style
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct RingPixel {
    idx: u32,
    /// Direction of this pixel, whole degrees clockwise from up (0..360).
    deg: u16,
    /// Distance outwards from the inner radius, in pixels.
    r: f32,
}

/// Precomputed geometry of the ring: which pixels of the square window lie on it,
/// with their angle and radius, so painting needs no trigonometry per pixel.
pub struct CircleLayout {
    /// Side length of the square window.
    pub size: i32,
    pub t: i32,
    pixels: Vec<RingPixel>,
    has_arcs: bool,
    painted_once: bool,
}

impl CircleLayout {
    pub fn new(radius: i32, thickness: i32) -> CircleLayout {
        let r0 = radius.max(4) as f32;
        let t = thickness.max(1);
        let size = 2 * (radius.max(4) + t + 2);
        let c = size as f32 / 2.0;
        let mut pixels = Vec::new();
        for y in 0..size {
            for x in 0..size {
                let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
                let r = (dx * dx + dy * dy).sqrt() - r0;
                if r < -1.0 || r > t as f32 + 1.0 {
                    continue;
                }
                let deg = dx.atan2(-dy).to_degrees().rem_euclid(360.0);
                pixels.push(RingPixel {
                    idx: (y * size + x) as u32,
                    deg: (deg as u16) % 360,
                    r,
                });
            }
        }
        CircleLayout {
            size,
            t,
            pixels,
            has_arcs: false,
            painted_once: false,
        }
    }

    pub fn len(&self) -> usize {
        (self.size * self.size) as usize
    }

    /// Force a full repaint on the next frame (e.g. colour changed).
    pub fn invalidate(&mut self) {
        self.painted_once = false;
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// Scale a premultiplied pixel by a coverage factor (anti-aliased edges).
#[inline]
fn scale_px(px: u32, cov: f32) -> u32 {
    if cov >= 1.0 {
        return px;
    }
    let k = (cov.max(0.0) * 256.0) as u32;
    let ch = |shift: u32| ((((px >> shift) & 0xFF) * k) >> 8) << shift;
    ch(24) | ch(16) | ch(8) | ch(0)
}

/// Paint the ring. Returns true if the window must be pushed.
pub fn paint_circle(
    layout: &mut CircleLayout,
    style: &Style,
    bins: &[u8; BINS],
    buf: &mut [u32],
) -> bool {
    // Per-degree arc colour and thickness (fixed-size, on the stack).
    let mut arc_px = [0u32; 360];
    let mut arc_th = [0f32; 360];
    let mut any = false;
    for (i, &q) in bins.iter().enumerate() {
        let intensity = q as f32 / 255.0;
        if q == 0 || intensity < style.min_intensity {
            continue;
        }
        any = true;
        let center = bin_center_deg(i, BINS).rem_euclid(360.0).round() as i32;
        let span = CIRCLE_MAX_ARC_DEG
            * (style.min_length_frac + (1.0 - style.min_length_frac) * intensity);
        let half = (span / 2.0).round() as i32;
        let px = style.pixel(intensity);
        let th = style.thickness(intensity, layout.t) as f32;
        for d in -half..=half {
            let k = (center + d).rem_euclid(360) as usize;
            if (arc_px[k] >> 24) < (px >> 24) {
                arc_px[k] = px;
            }
            arc_th[k] = arc_th[k].max(th);
        }
    }

    let changed = any || layout.has_arcs || !layout.painted_once;
    layout.has_arcs = any;
    layout.painted_once = true;
    if !changed {
        return false;
    }

    let guide = if style.guide_opacity > 0.0 {
        let a8 = (style.guide_opacity * 255.0 + 0.5) as u32;
        let pm = |c: u8| (c as u32 * a8 + 127) / 255;
        (a8 << 24) | (pm(style.rgb.0) << 16) | (pm(style.rgb.1) << 8) | pm(style.rgb.2)
    } else {
        0
    };
    for p in &layout.pixels {
        // Coverage of [0, th) along the radius, with half-pixel soft edges.
        let cover = |th: f32| ((th - p.r + 0.5).clamp(0.0, 1.0)) * ((p.r + 0.5).clamp(0.0, 1.0));
        let mut out = if guide != 0 {
            scale_px(guide, cover(1.5))
        } else {
            0
        };
        let k = p.deg as usize;
        if arc_px[k] != 0 {
            let a = scale_px(arc_px[k], cover(arc_th[k]));
            if (a >> 24) > (out >> 24) {
                out = a;
            }
        }
        buf[p.idx as usize] = out;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::direction::angle_to_bins;

    const L: EdgeLayout = EdgeLayout {
        w: 1920,
        h: 1080,
        t: 6,
    };

    #[test]
    fn cardinal_directions_hit_edge_centres() {
        assert!((L.angle_to_perimeter(0.0) - 960.0).abs() < 0.5); // top centre
        assert!((L.angle_to_perimeter(90.0) - (1920.0 + 540.0)).abs() < 0.5); // right centre
        assert!((L.angle_to_perimeter(180.0) - (1920.0 + 1080.0 + 960.0)).abs() < 0.5); // bottom centre
        assert!((L.angle_to_perimeter(-90.0) - (2.0 * 1920.0 + 1080.0 + 540.0)).abs() < 0.5);
        // left centre
    }

    #[test]
    fn corner_angle_hits_corner() {
        let corner = (960.0f32).atan2(540.0).to_degrees(); // ray to the top-right corner
        let s = L.angle_to_perimeter(corner);
        assert!((s - 1920.0).abs() < 1.0, "{s}");
    }

    #[test]
    fn perimeter_is_monotonic_clockwise() {
        let mut prev = L.angle_to_perimeter(-179.0);
        let start = prev;
        for d in -178..=180 {
            let s = L.angle_to_perimeter(d as f32);
            let unwrapped = if s < prev - 1000.0 { s + 6000.0 } else { s };
            assert!(
                unwrapped >= prev - 0.01,
                "not monotonic at {d}: {prev} -> {s}"
            );
            prev = unwrapped;
        }
        assert!(prev > start);
    }

    fn render(bins: &[u8; BINS]) -> ([Vec<u32>; 4], [bool; 4]) {
        let mut v: [Vec<u32>; 4] = std::array::from_fn(|e| vec![0u32; L.strip_len(e)]);
        let mut has = [false; 4];
        let changed = {
            let [a, b, c, d] = &mut v;
            let mut bufs: [&mut [u32]; 4] = [a, b, c, d];
            paint_frame(
                &L,
                &Style::from_config(&Config::default()),
                bins,
                &mut bufs,
                &mut has,
            )
        };
        (v, changed)
    }

    #[test]
    fn right_sound_paints_right_strip_middle() {
        let mut bins = [0u8; BINS];
        let (lo, _, _) = angle_to_bins(90.0, BINS);
        bins[lo] = 255;
        let (v, changed) = render(&bins);
        assert_eq!(changed, [false, true, false, false]);
        let r = L.strip_rect(RIGHT);
        let mid_row = (540 - L.t) as usize;
        assert_ne!(v[RIGHT][mid_row * r.2 as usize], 0);
        assert_eq!(v[RIGHT][0], 0, "far from the bar stays transparent");
    }

    #[test]
    fn front_sound_paints_top_centre_and_wraps_nothing() {
        let mut bins = [0u8; BINS];
        bins[0] = 255;
        let (v, _) = render(&bins);
        assert_ne!(v[TOP][960] >> 24, 0);
        assert_eq!(v[TOP][960 - 200], 0, "default full bar is 240 px long");
    }

    #[test]
    fn long_bar_wraps_around_corner() {
        let mut cfg = Config::default();
        cfg.overlay.bar_length = 800;
        let style = Style::from_config(&cfg);
        let mut v: [Vec<u32>; 4] = std::array::from_fn(|e| vec![0u32; L.strip_len(e)]);
        let mut has = [false; 4];
        let mut bins = [0u8; BINS];
        bins[(BINS as f32 * 60.6 / 360.0) as usize] = 255; // near the top-right corner
        let [a, b, c, d] = &mut v;
        let mut bufs: [&mut [u32]; 4] = [a, b, c, d];
        let changed = paint_frame(&L, &style, &bins, &mut bufs, &mut has);
        assert!(changed[TOP] && changed[RIGHT]);
    }

    #[test]
    fn clearing_reports_change_once() {
        let mut v: [Vec<u32>; 4] = std::array::from_fn(|e| vec![0u32; L.strip_len(e)]);
        let mut has = [false; 4];
        let style = Style::from_config(&Config::default());
        let mut bins = [0u8; BINS];
        bins[9] = 200;
        let [a, b, c, d] = &mut v;
        let mut bufs: [&mut [u32]; 4] = [a, b, c, d];
        assert!(paint_frame(&L, &style, &bins, &mut bufs, &mut has)[RIGHT]);
        bins[9] = 0;
        assert!(
            paint_frame(&L, &style, &bins, &mut bufs, &mut has)[RIGHT],
            "must push the cleared strip"
        );
        assert_eq!(
            paint_frame(&L, &style, &bins, &mut bufs, &mut has),
            [false; 4],
            "idle: nothing to push"
        );
        assert!(bufs[RIGHT].iter().all(|&p| p == 0));
    }

    #[test]
    fn louder_bars_are_thicker_and_hug_the_edge() {
        let style = Style::from_config(&Config::default());
        let t = 12;
        assert_eq!(style.thickness(1.0, t), 12);
        assert_eq!(style.thickness(0.0, t), 5); // 40% of 12, rounded
        let lay = EdgeLayout {
            w: 1920,
            h: 1080,
            t,
        };
        let mut v: [Vec<u32>; 4] = std::array::from_fn(|e| vec![0u32; lay.strip_len(e)]);
        let mut has = [false; 4];
        let mut bins = [0u8; BINS];
        bins[0] = 30; // quiet sound straight ahead -> thin bar on the top strip
        let [a, b, c, d] = &mut v;
        let mut bufs: [&mut [u32]; 4] = [a, b, c, d];
        paint_frame(&lay, &style, &bins, &mut bufs, &mut has);
        let col = 960usize;
        assert_ne!(bufs[TOP][col], 0, "outer row painted");
        assert_eq!(
            bufs[TOP][11 * 1920 + col],
            0,
            "inner row not painted for a quiet sound"
        );
    }

    fn render_style(style: OverlayStyle, bins: &[u8; BINS]) -> [Vec<u32>; 4] {
        let mut cfg = Config::default();
        cfg.overlay.style = style;
        let mut v: [Vec<u32>; 4] = std::array::from_fn(|e| vec![0u32; L.strip_len(e)]);
        let mut has = [false; 4];
        {
            let [a, b, c, d] = &mut v;
            let mut bufs: [&mut [u32]; 4] = [a, b, c, d];
            paint_frame(&L, &Style::from_config(&cfg), bins, &mut bufs, &mut has);
        }
        v
    }

    fn lit(buf: &[u32]) -> bool {
        buf.iter().any(|&p| p != 0)
    }

    #[test]
    fn front_back_style_behind_left_lights_left_and_bottom() {
        let mut bins = [0u8; BINS];
        bins[(BINS as f32 * (360.0 - 135.0) / 360.0) as usize] = 255; // -135 deg: behind-left
        let v = render_style(OverlayStyle::EdgesFrontBack, &bins);
        assert!(lit(&v[LEFT]) && lit(&v[BOTTOM]), "side + behind bars");
        assert!(!lit(&v[TOP]) && !lit(&v[RIGHT]));
        // The behind bar sits left of centre; the side bar sits below the middle.
        let first = v[BOTTOM].iter().position(|&p| p != 0).unwrap() as i32 % L.w;
        assert!(first < L.w / 2);
        let r = L.strip_rect(LEFT);
        let row = v[LEFT].iter().position(|&p| p != 0).unwrap() as i32 / r.2 + L.t;
        assert!(row > L.h / 2, "behind = lower on the side edge");
    }

    #[test]
    fn front_back_style_pure_side_and_pure_front() {
        let mut side = [0u8; BINS];
        side[9] = 255; // +90: right, neither in front nor behind
        let v = render_style(OverlayStyle::EdgesFrontBack, &side);
        assert!(lit(&v[RIGHT]) && !lit(&v[TOP]) && !lit(&v[BOTTOM]));

        let mut front = [0u8; BINS];
        front[0] = 255;
        let v = render_style(OverlayStyle::EdgesFrontBack, &front);
        assert!(lit(&v[TOP]) && !lit(&v[LEFT]) && !lit(&v[RIGHT]) && !lit(&v[BOTTOM]));
    }

    #[test]
    fn circle_arc_points_at_sound() {
        let mut cfg = Config::default();
        cfg.overlay.style = OverlayStyle::Circle;
        cfg.overlay.circle_guide_opacity = 0.0;
        let style = Style::from_config(&cfg);
        let mut lay = CircleLayout::new(70, 12);
        let mut buf = vec![0u32; lay.len()];
        let mut bins = [0u8; BINS];
        bins[9] = 255; // right
        assert!(paint_circle(&mut lay, &style, &bins, &mut buf));
        let s = lay.size;
        let c = s / 2;
        let at = |x: i32, y: i32| buf[(y * s + x) as usize];
        assert_ne!(at(c + 70 + 4, c), 0, "ring pixel to the right is lit");
        assert_eq!(at(c - 70 - 4, c), 0, "left side dark");
        assert_eq!(at(c, c - 70 - 4), 0, "top dark");
        assert_eq!(at(c, c), 0, "centre (crosshair) never painted");
        // Silence: one push to clear, then nothing.
        let zero = [0u8; BINS];
        assert!(paint_circle(&mut lay, &style, &zero, &mut buf));
        assert!(buf.iter().all(|&p| p == 0));
        assert!(!paint_circle(&mut lay, &style, &zero, &mut buf));
    }

    #[test]
    fn circle_guide_ring_drawn_once() {
        let style = Style::from_config(&Config::default());
        let mut lay = CircleLayout::new(50, 8);
        let mut buf = vec![0u32; lay.len()];
        let zero = [0u8; BINS];
        assert!(
            paint_circle(&mut lay, &style, &zero, &mut buf),
            "first frame draws the guide"
        );
        assert!(buf.iter().any(|&p| p != 0));
        assert!(!paint_circle(&mut lay, &style, &zero, &mut buf));
    }

    #[test]
    fn premultiplied_pixel() {
        let mut cfg = Config::default();
        cfg.overlay.color = "#FFFFFF".into();
        cfg.overlay.opacity = 1.0;
        let s = Style::from_config(&cfg);
        assert_eq!(s.pixel(1.0), 0xFFFF_FFFF);
        let p = s.pixel(0.0); // 35% opacity
        let a = p >> 24;
        assert_eq!((p >> 16) & 0xFF, a, "colour channels never exceed alpha");
    }
}
