//! Calibration / speaker test window: live per-channel level meters.
//!
//! It plays nothing. Use Windows' own speaker test (Sound settings -> your device ->
//! Configure / Test) or a surround test video, and check that each speaker lights the
//! matching bar. Capture keeps running while this window is open, even if the
//! overlay is hidden.

use super::util::{wide, CALIBRATION_CLASS};
use crate::config::SpeakerAngles;
use crate::dsp::layout::{Layout, MAX_CHANNELS};
use crate::state::Shared;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, OnceLock};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the controller window when the calibration window is destroyed.
pub const WM_APP_CALIB_CLOSED: u32 = WM_APP + 4;

const TIMER_ID: usize = 1;
const W: i32 = 460;
const ROW_H: i32 = 26;
const TOP: i32 = 58;

struct Ctx {
    shared: Arc<Shared>,
    controller: isize,
}
// Set once; read by the window procedure (which cannot capture state).
static CTX: OnceLock<Ctx> = OnceLock::new();

pub fn register_class(
    hinst: HINSTANCE,
    icon: HICON,
    shared: Arc<Shared>,
    controller: HWND,
) -> Result<()> {
    let _ = CTX.set(Ctx {
        shared,
        controller: controller.0 as isize,
    });
    let class = wide(CALIBRATION_CLASS);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hIcon: icon,
        hIconSm: icon,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    Ok(())
}

pub fn open(hinst: HINSTANCE) -> Result<HWND> {
    let class = wide(CALIBRATION_CLASS);
    let title = wide("SoundSight - speaker calibration");
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: W,
        bottom: TOP + ROW_H * MAX_CHANNELS as i32 + 70,
    };
    unsafe {
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE(0));
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        // ~20 Hz is plenty for a meter; this window only exists while calibrating.
        SetTimer(Some(hwnd), TIMER_ID, 50, None);
        Ok(hwnd)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                if let Some(c) = CTX.get() {
                    let _ = PostMessageW(
                        Some(HWND(c.controller as _)),
                        WM_APP_CALIB_CLOSED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn text(dc: HDC, x: i32, y: i32, s: &str) {
    let w: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        let _ = TextOutW(dc, x, y, &w);
    }
}

fn fill(dc: HDC, l: i32, t: i32, r: i32, b: i32, rgb: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(rgb));
        FillRect(
            dc,
            &RECT {
                left: l,
                top: t,
                right: r,
                bottom: b,
            },
            brush,
        );
        let _ = DeleteObject(brush.into());
    }
}

/// COLORREF is 0x00BBGGRR.
const fn bgr(r: u32, g: u32, b: u32) -> u32 {
    (b << 16) | (g << 8) | r
}

unsafe fn paint(hwnd: HWND) {
    let Some(ctx) = CTX.get() else { return };
    let sh = &ctx.shared;
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (cw, chh) = (rc.right, rc.bottom);

        // Double buffer to avoid flicker.
        let dc = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, cw, chh);
        let old = SelectObject(dc, bmp.into());
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let old_font = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(bgr(230, 230, 230)));
        fill(dc, 0, 0, cw, chh, bgr(24, 24, 28));

        let channels = sh.channels.load(Relaxed) as usize;
        let mask = sh.channel_mask.load(Relaxed);
        let rate = sh.sample_rate.load(Relaxed);
        if sh.capturing.load(Relaxed) && channels > 0 {
            let layout = Layout::from_mask(channels, mask, &SpeakerAngles::default());
            text(
                dc,
                12,
                10,
                &format!(
                    "Default output: {} ({} ch, mask 0x{:X}), {} Hz",
                    layout.name, channels, mask, rate
                ),
            );
            text(
                dc,
                12,
                30,
                if layout.has_rear() && sh.stereo_content.load(Relaxed) {
                    "Stereo-only content detected: FL/FR shown as hard left/right."
                } else if layout.has_rear() {
                    "Front/back separation available."
                } else {
                    "No rear channels: only left/right can be shown (see README: Voicemeeter 7.1)."
                },
            );

            let mut meters = [0u8; MAX_CHANNELS];
            sh.read_meters(&mut meters);
            let (x0, x1) = (60, cw - 70);
            for c in 0..layout.analysed_channels() {
                let y = TOP + c as i32 * ROW_H;
                let spk = layout.speakers[c];
                text(dc, 12, y + 3, spk.label());
                fill(dc, x0, y + 2, x1, y + ROW_H - 6, bgr(50, 50, 58));
                // meter byte: -90..0 dB -> 0..255; show -60..0 dB.
                let db = meters[c] as f32 / 255.0 * 90.0 - 90.0;
                let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let len = ((x1 - x0) as f32 * frac) as i32;
                let color = if layout.vectors[c] == (0.0, 0.0) {
                    bgr(120, 120, 130)
                } else {
                    bgr(255, 59, 48)
                };
                if len > 0 {
                    fill(dc, x0, y + 2, x0 + len, y + ROW_H - 6, color);
                }
                text(
                    dc,
                    x1 + 8,
                    y + 3,
                    &if db <= -89.0 {
                        "  -inf".to_string()
                    } else {
                        format!("{db:6.1} dB")
                    },
                );
            }
            let y = TOP + MAX_CHANNELS as i32 * ROW_H + 6;
            text(
                dc,
                12,
                y,
                "Play a speaker test (Windows Sound settings > Configure/Test) or a",
            );
            text(
                dc,
                12,
                y + 18,
                "surround test video: each speaker should light only its own bar.",
            );
            text(
                dc,
                12,
                y + 36,
                "Grey = LFE/unused (ignored for direction). This window plays nothing.",
            );
        } else {
            text(
                dc,
                12,
                10,
                "No capture stream. Is an output device enabled?",
            );
            text(
                dc,
                12,
                30,
                "Capture restarts automatically when a device becomes available.",
            );
        }

        let _ = BitBlt(hdc, 0, 0, cw, chh, Some(dc), 0, 0, SRCCOPY);
        SelectObject(dc, old_font);
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        let _ = EndPaint(hwnd, &ps);
    }
}
