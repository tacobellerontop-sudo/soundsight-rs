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

use crate::config::Config;
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
}

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
        }
    }

    /// Premultiplied BGRA pixel for an intensity in 0..1.
    pub fn pixel(&self, intensity: f32) -> u32 {
        let a = self.opacity * (self.min_opacity_frac + (1.0 - self.min_opacity_frac) * intensity);
        let a8 = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        let pm = |c: u8| (c as u32 * a8 + 127) / 255;
        (a8 << 24) | (pm(self.rgb.0) << 16) | (pm(self.rgb.1) << 8) | pm(self.rgb.2)
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

    for (i, &q) in bins.iter().enumerate() {
        let intensity = q as f32 / 255.0;
        if q == 0 || intensity < style.min_intensity {
            continue;
        }
        let center = layout.angle_to_perimeter(bin_center_deg(i, BINS));
        let half = style.length(intensity) / 2.0;
        let px = style.pixel(intensity);
        paint_interval(layout, bufs, center - half, center + half, px, &mut drawn);
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
    px: u32,
    drawn: &mut [bool; 4],
) {
    let p = l.perimeter();
    let len = (b - a).min(p);
    let a = a.rem_euclid(p);
    let b = a + len;
    paint_segment(l, bufs, a, b.min(p), px, drawn);
    if b > p {
        paint_segment(l, bufs, 0.0, b - p, px, drawn);
    }
}

/// Paint [a, b) with 0 <= a <= b <= perimeter.
fn paint_segment(
    l: &EdgeLayout,
    bufs: &mut [&mut [u32]; 4],
    a: f32,
    b: f32,
    px: u32,
    drawn: &mut [bool; 4],
) {
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
        let painted = match edge {
            // Horizontal strip: lo..hi are x; fill all t rows.
            TOP | BOTTOM => fill(buf, rect.2, rect.3, lo, hi, 0, rect.3, px),
            // Vertical strip: lo..hi are screen y; strip starts at y = t.
            _ => fill(buf, rect.2, rect.3, 0, rect.2, lo - t, hi - t, px),
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
