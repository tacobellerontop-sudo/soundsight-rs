//! System tray icon (Shell_NotifyIconW) and its context menu.

use super::util::copy_wide;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_TRAY: u32 = WM_APP + 3;

pub const CMD_TOGGLE: u32 = 1;
pub const CMD_CALIBRATE: u32 = 2;
pub const CMD_RELOAD: u32 = 3;
pub const CMD_EDIT_CONFIG: u32 = 4;
pub const CMD_QUIT: u32 = 5;

pub struct Tray {
    nid: NOTIFYICONDATAW,
    added: bool,
}

impl Tray {
    pub fn new(hwnd: HWND, icon: HICON, tip: &str) -> Tray {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        copy_wide(&mut nid.szTip, tip);
        let mut t = Tray { nid, added: false };
        t.add();
        t
    }

    /// (Re-)add the icon. Also called when Explorer restarts ("TaskbarCreated").
    pub fn add(&mut self) {
        self.nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        self.added = unsafe { Shell_NotifyIconW(NIM_ADD, &self.nid) }.as_bool();
    }

    pub fn set_tip(&mut self, tip: &str) {
        copy_wide(&mut self.nid.szTip, tip);
        self.nid.uFlags = NIF_TIP;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        }
    }

    /// Non-blocking notification (used for config / hotkey errors).
    pub fn balloon(&mut self, title: &str, text: &str, warning: bool) {
        let mut nid = self.nid;
        nid.uFlags = NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
            }
            self.added = false;
        }
    }

    /// Show the context menu at the cursor; returns the chosen command (0 = none).
    /// Runs a modal loop: callers must not hold borrows of app state across it.
    pub fn show_menu(hwnd: HWND, overlay_visible: bool, calibrating: bool) -> u32 {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return 0;
            };
            let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(
                menu,
                MF_STRING | check(overlay_visible),
                CMD_TOGGLE as usize,
                windows::core::w!("Show overlay"),
            );
            let _ = AppendMenuW(
                menu,
                MF_STRING | check(calibrating),
                CMD_CALIBRATE as usize,
                windows::core::w!("Calibrate / speaker test..."),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_EDIT_CONFIG as usize,
                windows::core::w!("Edit config"),
            );
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_RELOAD as usize,
                windows::core::w!("Reload config"),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_QUIT as usize,
                windows::core::w!("Quit"),
            );

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                Some(0),
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            cmd.0 as u32
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}
