//! Small Win32 helpers.

use windows::core::Result;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const LAYERED_STRIP_CLASS: &str = "SoundSightStrip";
pub const CONTROLLER_CLASS: &str = "SoundSightController";
pub const SETTINGS_CLASS: &str = "SoundSightSettings";

const RUN_KEY: windows::core::PCWSTR =
    windows::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: windows::core::PCWSTR = windows::core::w!("SoundSight");

/// Is "Start with Windows" on (an HKCU Run entry exists)?
pub fn startup_enabled() -> bool {
    use windows::Win32::System::Registry::*;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    }
    .is_ok()
}

/// Add or remove the per-user Run entry pointing at this exe. No admin rights needed.
pub fn set_startup(enabled: bool) -> bool {
    use windows::Win32::System::Registry::*;
    unsafe {
        if !enabled {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return r.is_ok() || r == ERROR_FILE_NOT_FOUND;
        }
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let cmd = wide(&format!("\"{}\"", exe.display()));
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(cmd.as_ptr() as *const core::ffi::c_void),
            (cmd.len() * 2) as u32,
        )
        .is_ok()
    }
}

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy into a fixed UTF-16 buffer (truncating, always NUL terminated).
pub fn copy_wide(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16()) {
        *d = c;
        n += 1;
    }
    let end = n.min(dst.len().saturating_sub(1));
    dst[end] = 0;
}

/// Build the app icon at runtime (no resource file needed): four red edge bars and
/// a white centre dot, i.e. a tiny picture of the overlay.
pub fn make_icon(size: i32) -> Result<HICON> {
    let s = size.max(16);
    unsafe {
        let screen = GetDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: s,
                biHeight: -s,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        ReleaseDC(None, screen);
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (s * s) as usize);
        let bar = (s / 8).max(2);
        let (lo, hi) = (s / 4, s - s / 4);
        for y in 0..s {
            for x in 0..s {
                let edge = (y < bar || y >= s - bar) && (lo..hi).contains(&x)
                    || (x < bar || x >= s - bar) && (lo..hi).contains(&y);
                let c = s / 2;
                let dot = (x - c) * (x - c) + (y - c) * (y - c) <= (s / 8) * (s / 8);
                px[(y * s + x) as usize] = if edge {
                    0xFFFF_3B30
                } else if dot {
                    0xFFFF_FFFF
                } else {
                    0
                };
            }
        }
        let mask_bits = vec![0u8; ((s + 15) / 16 * 2 * s) as usize];
        let mask = CreateBitmap(s, s, 1, 1, Some(mask_bits.as_ptr() as _));
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}
