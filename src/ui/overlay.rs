//! The overlay: four thin layered windows along the primary monitor's edges.
//!
//! Each strip is `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE
//! | WS_EX_TOOLWINDOW`: per-pixel alpha, click-through, always on top, never takes
//! focus, not in the taskbar/Alt-Tab. It is an ordinary desktop window composited by
//! DWM; nothing touches the game's swapchain.
//!
//! Why GDI + UpdateLayeredWindow with a DIB section (and not Direct2D):
//! * We draw only axis-aligned solid bars. Writing pixels straight into the DIB
//!   section's memory is a few microseconds; no GDI drawing calls at all.
//! * Direct2D/Direct3D would load the D3D11/DXGI stack (several MB of RAM, a GPU
//!   device context and driver threads in our process) and submit GPU work, which
//!   is exactly the cost we want to avoid next to a game.
//! * Four strips instead of one full-screen window: a full-screen 32-bit surface is
//!   8 MB at 1080p and 33 MB at 4K, which alone would break the 20 MB RAM target, and
//!   DWM would have to blend a full-screen layer. Strips at 6 px thick are ~100 KB total.
//! * `UpdateLayeredWindow` is called only for strips whose pixels changed.

use super::util::{wide, LAYERED_STRIP_CLASS};
use crate::config::Config;
use crate::dsp::analyzer::BINS;
use crate::render::{paint_frame, EdgeLayout, Style};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

struct Strip {
    hwnd: HWND,
    memdc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    len: usize,
    pos: POINT,
    size: SIZE,
}

pub struct Overlay {
    strips: Vec<Strip>,
    layout: EdgeLayout,
    style: Style,
    has_content: [bool; 4],
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
        let layout = EdgeLayout::new(w, h, cfg.overlay.thickness as i32);
        let class = wide(LAYERED_STRIP_CLASS);
        let mut strips = Vec::with_capacity(4);

        for edge in 0..4 {
            let (x, y, sw, sh) = layout.strip_rect(edge);
            let (sw, sh) = (sw.max(1), sh.max(1));
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
                    sw,
                    sh,
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
                        biWidth: sw,
                        biHeight: -sh, // top-down rows
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let bitmap =
                    CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
                let old = SelectObject(memdc, bitmap.into());
                let len = (sw * sh) as usize;
                // DIB sections are zero-initialised: fully transparent.
                strips.push(Strip {
                    hwnd,
                    memdc,
                    bitmap,
                    old,
                    bits: bits as *mut u32,
                    len,
                    pos: POINT { x, y },
                    size: SIZE { cx: sw, cy: sh },
                });
            }
        }

        let ov = Overlay {
            strips,
            layout,
            style: Style::from_config(cfg),
            has_content: [false; 4],
            visible: false,
        };
        for i in 0..4 {
            ov.push(i);
        }
        Ok(ov)
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn show(&mut self) {
        for s in &self.strips {
            unsafe {
                let _ = ShowWindow(s.hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    s.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }
        self.visible = true;
    }

    pub fn hide(&mut self) {
        for s in &self.strips {
            unsafe {
                let _ = ShowWindow(s.hwnd, SW_HIDE);
            }
        }
        self.visible = false;
    }

    /// Repaint from quantised bins; only changed strips are pushed to DWM.
    pub fn render(&mut self, bins: &[u8; BINS]) {
        let changed = {
            let mut it = self
                .strips
                .iter()
                .map(|s| unsafe { std::slice::from_raw_parts_mut(s.bits, s.len) });
            let mut bufs: [&mut [u32]; 4] = [
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
            ];
            paint_frame(
                &self.layout,
                &self.style,
                bins,
                &mut bufs,
                &mut self.has_content,
            )
        };
        for (i, c) in changed.iter().enumerate() {
            if *c {
                self.push(i);
            }
        }
    }

    fn push(&self, i: usize) {
        let s = &self.strips[i];
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(
                s.hwnd,
                None,
                Some(&s.pos),
                Some(&s.size),
                Some(s.memdc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        for s in &self.strips {
            unsafe {
                let _ = DestroyWindow(s.hwnd);
                SelectObject(s.memdc, s.old);
                let _ = DeleteObject(s.bitmap.into());
                let _ = DeleteDC(s.memdc);
            }
        }
    }
}
