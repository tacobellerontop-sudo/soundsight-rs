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
//! * `UpdateLayeredWindow` is called only for windows whose pixels changed.

use super::util::{wide, LAYERED_STRIP_CLASS};
use crate::config::{Config, OverlayStyle};
use crate::dsp::analyzer::BINS;
use crate::render::{paint_circle, paint_frame, CircleLayout, EdgeLayout, Style};
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
            s.push();
            Ok(s)
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn pixels(&self) -> &mut [u32] {
        // The DIB memory is owned by `bitmap` and only touched from the UI thread.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }

    fn push(&self) {
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&self.pos),
                Some(&self.size),
                Some(self.memdc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
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

enum Kind {
    Edges {
        layout: EdgeLayout,
        strips: [Surface; 4],
        has_content: [bool; 4],
    },
    Circle {
        layout: CircleLayout,
        window: Surface,
    },
}

pub struct Overlay {
    kind: Kind,
    style: Style,
    visible: bool,
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
            Kind::Circle { layout, window }
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
                has_content: [false; 4],
            }
        };
        let mut ov = Overlay {
            kind,
            style: Style::from_config(cfg),
            visible: false,
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

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    fn surfaces(&self) -> &[Surface] {
        match &self.kind {
            Kind::Edges { strips, .. } => strips,
            Kind::Circle { window, .. } => std::slice::from_ref(window),
        }
    }

    pub fn show(&mut self) {
        for s in self.surfaces() {
            s.show();
        }
        self.visible = true;
    }

    pub fn hide(&mut self) {
        for s in self.surfaces() {
            s.hide();
        }
        self.visible = false;
    }

    /// Repaint from quantised bins; only changed windows are pushed to DWM.
    pub fn render(&mut self, bins: &[u8; BINS]) {
        match &mut self.kind {
            Kind::Edges {
                layout,
                strips,
                has_content,
            } => {
                let [a, b, c, d] = strips;
                let mut bufs: [&mut [u32]; 4] = [a.pixels(), b.pixels(), c.pixels(), d.pixels()];
                let changed = paint_frame(layout, &self.style, bins, &mut bufs, has_content);
                for (s, c) in strips.iter().zip(changed) {
                    if c {
                        s.push();
                    }
                }
            }
            Kind::Circle { layout, window } => {
                if paint_circle(layout, &self.style, bins, window.pixels()) {
                    window.push();
                }
            }
        }
    }
}
