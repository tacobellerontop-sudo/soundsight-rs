//! The overlay: layered windows on the primary monitor, either four thin strips along
//! the screen edges or one small square window around the crosshair (circle style).
//!
//! Every window is `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE
//! | WS_EX_TOOLWINDOW`: per-pixel alpha, click-through, always on top, never takes
//! focus, not in the taskbar/Alt-Tab. It is an ordinary desktop window composited by
//! DWM; nothing touches the game's swapchain.
//!
//! Why GDI + UpdateLayeredWindow with a DIB section (and not Direct2D):
//! * We draw only bars and arcs. Writing pixels straight into the DIB section's memory
//!   takes microseconds; there are no GDI drawing calls at all.
//! * Direct2D/Direct3D would load the D3D11/DXGI stack (several MB of RAM, a GPU
//!   device context and driver threads in our process) and submit GPU work, which
//!   is exactly the cost we want to avoid next to a game.
//! * Small windows instead of one full-screen window: a full-screen 32-bit surface is
//!   8 MB at 1080p and 33 MB at 4K, and DWM would have to blend a full-screen layer.
//!   Edge strips are ~250 KB in total, the circle window ~70 KB.
//! * Only the changed rectangle is pushed (`UpdateLayeredWindowIndirect` with a dirty
//!   rect): a 240 px bar on a 2560 px strip uploads ~10x less than the whole strip.
//! * The windows are hidden while there is nothing to draw (after a short linger), so
//!   when it is quiet no overlay window covers the game at all and Windows can use
//!   its fastest presentation path (independent flip) for the game.

use super::util::{wide, LAYERED_STRIP_CLASS};
use crate::config::{Config, OverlayStyle};
use crate::dsp::analyzer::BINS;
use crate::render::{paint_circle, paint_frame, Bounds, CircleLayout, EdgeLayout, Style};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// One layered window and the DIB section that holds its pixels.
struct Surface {
    hwnd: HWND,
    memdc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    len: usize,
    pos: POINT,
    size: SIZE,
}

impl Surface {
    fn new(hinst: HINSTANCE, x: i32, y: i32, w: i32, h: i32) -> Result<Surface> {
        let (w, h) = (w.max(1), h.max(1));
        let class = wide(LAYERED_STRIP_CLASS);
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(hinst),
                None,
            )?;
            let screen = GetDC(None);
            let memdc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = DeleteDC(memdc);
                        let _ = DestroyWindow(hwnd);
                        return Err(e);
                    }
                };
            let old = SelectObject(memdc, bitmap.into());
            // DIB sections are zero-initialised: fully transparent.
            let s = Surface {
                hwnd,
                memdc,
                bitmap,
                old,
                bits: bits as *mut u32,
                len: (w * h) as usize,
                pos: POINT { x, y },
                size: SIZE { cx: w, cy: h },
            };
            s.push(None);
            Ok(s)
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn pixels(&self) -> &mut [u32] {
        // The DIB memory is owned by `bitmap` and only touched from the UI thread.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }

    /// Push pixels to DWM: only `dirty` if given, else the whole surface.
    fn push(&self, dirty: Option<Bounds>) {
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let rect = dirty.map(|b| RECT {
            left: b.x0,
            top: b.y0,
            right: b.x1,
            bottom: b.y1,
        });
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: &self.pos,
            psize: &self.size,
            hdcSrc: self.memdc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: rect.as_ref().map_or(std::ptr::null(), |r| r as *const RECT),
        };
        unsafe {
            let _ = UpdateLayeredWindowIndirect(self.hwnd, &info);
        }
    }

    fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            SelectObject(self.memdc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memdc);
        }
    }
}

// One instance per app: the size difference between variants doesn't matter.
#[allow(clippy::large_enum_variant)]
enum Kind {
    Edges {
        layout: EdgeLayout,
        strips: [Surface; 4],
        painted: [Option<Bounds>; 4],
    },
    Circle {
        layout: CircleLayout,
        window: Surface,
        content: bool,
    },
}

pub struct Overlay {
    kind: Kind,
    style: Style,
    /// The user wants the overlay (hotkey / checkbox).
    visible: bool,
    /// The windows are actually on screen (only while there is something to draw).
    shown: bool,
}

unsafe extern "system" fn strip_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // Belt and braces: WS_EX_TRANSPARENT already makes layered windows click-through.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

pub fn register_class(hinst: HINSTANCE) -> Result<()> {
    let class = wide(LAYERED_STRIP_CLASS);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(strip_wndproc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    Ok(())
}

impl Overlay {
    pub fn new(hinst: HINSTANCE, cfg: &Config) -> Result<Overlay> {
        // Primary monitor in physical pixels (the process is per-monitor DPI aware).
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let thickness = cfg.overlay.thickness as i32;
        let kind = if cfg.overlay.style == OverlayStyle::Circle {
            let layout = CircleLayout::new(cfg.overlay.circle_radius as i32, thickness);
            let s = layout.size;
            let window = Surface::new(hinst, (w - s) / 2, (h - s) / 2, s, s)?;
            Kind::Circle {
                layout,
                window,
                content: false,
            }
        } else {
            let layout = EdgeLayout::new(w, h, thickness);
            let mk = |e: usize| {
                let (x, y, sw, sh) = layout.strip_rect(e);
                Surface::new(hinst, x, y, sw, sh)
            };
            let strips = [mk(0)?, mk(1)?, mk(2)?, mk(3)?];
            Kind::Edges {
                layout,
                strips,
                painted: [None; 4],
            }
        };
        let mut ov = Overlay {
            kind,
            style: Style::from_config(cfg),
            visible: false,
            shown: false,
        };
        ov.render(&[0; BINS]); // circle: draws the guide ring
        Ok(ov)
    }

    /// True if `cfg` needs different windows than `self` (style or geometry change).
    pub fn needs_rebuild(old: &Config, new: &Config) -> bool {
        let (a, b) = (&old.overlay, &new.overlay);
        let circle = |c: &Config| c.overlay.style == OverlayStyle::Circle;
        circle(old) != circle(new)
            || a.thickness != b.thickness
            || (circle(new) && a.circle_radius != b.circle_radius)
    }

    /// Change colours/sizes that don't need new windows.
    pub fn set_style(&mut self, cfg: &Config, bins: &[u8; BINS]) {
        self.style = Style::from_config(cfg);
        if let Kind::Circle { layout, .. } = &mut self.kind {
            // Guide ring colour/opacity may have changed.
            layout.invalidate();
        }
        self.render(bins);
    }

    /// The user wants the overlay on screen.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Something is drawn right now (or the circle's always-on guide ring).
    pub fn has_content(&self) -> bool {
        match &self.kind {
            Kind::Edges { painted, .. } => painted.iter().any(|p| p.is_some()),
            Kind::Circle { content, .. } => *content,
        }
    }

    fn surfaces(&self) -> &[Surface] {
        match &self.kind {
            Kind::Edges { strips, .. } => strips,
            Kind::Circle { window, .. } => std::slice::from_ref(window),
        }
    }

    fn set_shown(&mut self, on: bool) {
        if self.shown == on {
            return;
        }
        for s in self.surfaces() {
            if on {
                // Windows may have dropped a hidden layered window's pixels: push the
                // whole surface once, so later dirty-rect updates start from correct pixels.
                s.push(None);
                s.show();
            } else {
                s.hide();
            }
        }
        self.shown = on;
    }

    pub fn show(&mut self) {
        self.visible = true;
        let content = self.has_content();
        self.set_shown(content);
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.set_shown(false);
    }

    /// Called after the overlay has had no content for a while: take the windows off
    /// the screen so nothing covers the game. They come back with the next sound.
    pub fn hide_idle(&mut self) {
        if !self.has_content() {
            self.set_shown(false);
        }
    }

    /// Repaint from quantised bins; only changed regions are pushed to DWM.
    /// Returns whether anything is drawn now.
    pub fn render(&mut self, bins: &[u8; BINS]) -> bool {
        match &mut self.kind {
            Kind::Edges {
                layout,
                strips,
                painted,
            } => {
                let [a, b, c, d] = strips;
                let mut bufs: [&mut [u32]; 4] = [a.pixels(), b.pixels(), c.pixels(), d.pixels()];
                let dirty = paint_frame(layout, &self.style, bins, &mut bufs, painted);
                for (s, d) in strips.iter().zip(dirty) {
                    if d.is_some() {
                        s.push(d);
                    }
                }
            }
            Kind::Circle {
                layout,
                window,
                content,
            } => {
                if let Some(d) = paint_circle(layout, &self.style, bins, window.pixels()) {
                    window.push(Some(d));
                }
                *content = bins
                    .iter()
                    .any(|&q| q as f32 / 255.0 >= self.style.min_intensity)
                    || self.style.guide_opacity > 0.0;
            }
        }
        let content = self.has_content();
        // Content appeared: put the windows (back) on screen. Pixels are pushed first,
        // so nothing stale flashes. Hiding again is delayed by the app (see hide_idle).
        if self.visible && content {
            self.set_shown(true);
        }
        content
    }
}
