//! Application wiring and the UI thread's message loop.
//!
//! The UI thread blocks in `MsgWaitForMultipleObjectsEx` on its message queue plus
//! one high-resolution waitable timer (used only to enforce the refresh cap). With no
//! sound it receives no messages and uses no CPU.
//!
//! Redraw flow: capture thread changes bins -> posts WM_APP_REDRAW (at most one
//! outstanding) -> if a frame was drawn less than 1/refresh_hz ago, arm the timer for
//! the remainder, else draw now. Bursts of updates therefore coalesce into one frame.

use super::calibrate::{self, WM_APP_CALIB_CLOSED};
use super::overlay::{self, Overlay};
use super::tray::{self, Tray, WM_APP_TRAY};
use super::util::{make_icon, wide, CONTROLLER_CLASS};
use crate::audio::capture::{CaptureThread, WM_APP_REDRAW, WM_APP_STREAM};
use crate::config::{Config, DEFAULT_TOML};
use crate::dsp::analyzer::BINS;
use crate::hotkey;
use crate::state::Shared;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

const HOTKEY_ID: i32 = 1;

struct App {
    hinst: HINSTANCE,
    hwnd: HWND,
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    capture: CaptureThread,
    overlay: Overlay,
    tray: Tray,
    calib: Option<HWND>,
    hotkey_label: String,
    frame_interval: Duration,
    last_frame: Instant,
    timer: HANDLE,
    timer_armed: bool,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Reachable even while `APP` is borrowed (see WM_APP_REDRAW handling).
static SHARED: std::sync::OnceLock<Arc<Shared>> = std::sync::OnceLock::new();

/// Run `f` on the app state unless it is already borrowed (re-entrant message).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

pub fn run() {
    unsafe {
        // Single instance: two overlays would fight over the hotkey and double the cost.
        let _mutex = CreateMutexW(None, false, w!("Local\\SoundSightRS.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        // Physical pixels, so the strips line up with the real screen edges.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if let Err(e) = init() {
            let msg = wide(&format!("SoundSight failed to start:\n{e}"));
            MessageBoxW(None, PCWSTR(msg.as_ptr()), w!("SoundSight"), MB_ICONERROR);
            return;
        }
        message_loop();
        // Dropping the app joins the capture thread and destroys the windows.
        let app = APP.with(|a| a.borrow_mut().take());
        drop(app);
    }
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("soundsight.toml")))
        .unwrap_or_else(|| PathBuf::from("soundsight.toml"))
}

/// Load the config, creating the default file on first run. Errors fall back to defaults.
fn load_config(path: &PathBuf) -> (Config, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(s) => match Config::parse(&s) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("Using defaults. {e}"))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::write(path, DEFAULT_TOML);
            (Config::default(), None)
        }
        Err(e) => (
            Config::default(),
            Some(format!("Cannot read {}: {e}", path.display())),
        ),
    }
}

fn init() -> Result<()> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let config_path = config_path();
        let (cfg, cfg_err) = load_config(&config_path);

        let icon = make_icon(GetSystemMetrics(SM_CXSMICON))
            .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());

        // Controller: hidden top-level window (message-only windows miss broadcasts such
        // as TaskbarCreated and WM_DISPLAYCHANGE).
        let class = wide(CONTROLLER_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(controller_wndproc),
            hInstance: hinst,
            hIcon: icon,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            w!("SoundSight"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )?;

        overlay::register_class(hinst)?;
        let shared = Arc::new(Shared::new());
        shared.notify_hwnd.store(hwnd.0 as isize, SeqCst);
        let _ = SHARED.set(shared.clone());
        calibrate::register_class(hinst, icon, shared.clone(), hwnd)?;

        let mut overlay = Overlay::new(hinst, &cfg)?;
        let visible = cfg.general.start_visible;
        if visible {
            overlay.show();
        }
        shared.wanted.store(visible, SeqCst);

        let config = Arc::new(Mutex::new(cfg.clone()));
        let capture = CaptureThread::spawn(shared.clone(), config.clone())?;
        let tray = Tray::new(hwnd, icon, "SoundSight");

        let timer = CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
        .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))?;

        let mut app = App {
            hinst,
            hwnd,
            shared,
            config,
            config_path,
            capture,
            overlay,
            tray,
            calib: None,
            hotkey_label: String::new(),
            frame_interval: frame_interval(&cfg),
            last_frame: Instant::now(),
            timer,
            timer_armed: false,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        app.register_hotkey(&cfg);
        app.update_tip();
        if let Some(e) = cfg_err {
            app.tray.balloon("SoundSight config error", &e, true);
        }
        APP.with(|a| *a.borrow_mut() = Some(app));
        Ok(())
    }
}

fn frame_interval(cfg: &Config) -> Duration {
    Duration::from_secs_f64(1.0 / cfg.general.refresh_hz.max(1) as f64)
}

fn message_loop() {
    let timer = with_app(|a| a.timer).unwrap_or_default();
    let handles = [timer];
    let mut msg = MSG::default();
    unsafe {
        loop {
            let r = MsgWaitForMultipleObjectsEx(
                Some(&handles),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            if r == WAIT_OBJECT_0 {
                with_app(|a| {
                    a.timer_armed = false;
                    a.draw_frame();
                });
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl App {
    fn register_hotkey(&mut self, cfg: &Config) {
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
        }
        self.hotkey_label.clear();
        match hotkey::parse(&cfg.general.hotkey) {
            Ok((mods, vk)) => {
                if unsafe {
                    RegisterHotKey(Some(self.hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(mods), vk)
                }
                .is_ok()
                {
                    self.hotkey_label = cfg.general.hotkey.clone();
                } else {
                    let msg = format!("Hotkey {} is already used by another program. Use the tray menu, or pick another in the config.", cfg.general.hotkey);
                    self.tray.balloon("SoundSight hotkey", &msg, true);
                }
            }
            Err(e) => self.tray.balloon("SoundSight hotkey", &e, true),
        }
    }

    fn update_tip(&mut self) {
        let state = if self.overlay.is_visible() {
            "visible"
        } else {
            "hidden"
        };
        let tip = if self.hotkey_label.is_empty() {
            format!("SoundSight: {state}")
        } else {
            format!("SoundSight: {state} ({} toggles)", self.hotkey_label)
        };
        self.tray.set_tip(&tip);
    }

    /// Capture runs only while something displays its output.
    fn update_wanted(&mut self) {
        let wanted = self.overlay.is_visible() || self.calib.is_some();
        if self.shared.wanted.swap(wanted, SeqCst) != wanted {
            self.capture.wake();
        }
    }

    fn set_visible(&mut self, v: bool) {
        if v == self.overlay.is_visible() {
            return;
        }
        if v {
            self.overlay.render(&[0; BINS]);
            self.overlay.show();
        } else {
            // Clear first so no stale bars flash when shown again.
            self.overlay.render(&[0; BINS]);
            self.overlay.hide();
        }
        self.update_wanted();
        self.update_tip();
    }

    fn on_redraw_request(&mut self) {
        if self.timer_armed {
            return; // a frame is already scheduled; it will read the latest state
        }
        let since = self.last_frame.elapsed();
        if since >= self.frame_interval {
            self.draw_frame();
        } else {
            let wait = self.frame_interval - since;
            // Negative = relative time, in 100 ns units.
            let due = -((wait.as_nanos() / 100).max(1) as i64);
            unsafe {
                if SetWaitableTimer(self.timer, &due, 0, None, None, false).is_ok() {
                    self.timer_armed = true;
                } else {
                    self.draw_frame();
                }
            }
        }
    }

    fn draw_frame(&mut self) {
        // Clear the flag *before* reading so an update racing with us posts a new message.
        self.shared.redraw_pending.store(false, Release);
        let mut bins = [0u8; BINS];
        self.shared.read_bins(&mut bins);
        if self.overlay.is_visible() {
            self.overlay.render(&bins);
        }
        self.last_frame = Instant::now();
    }

    fn rebuild_overlay(&mut self) {
        let cfg = self.config.lock().map(|c| c.clone()).unwrap_or_default();
        let visible = self.overlay.is_visible();
        match Overlay::new(self.hinst, &cfg) {
            Ok(mut ov) => {
                if visible {
                    ov.show();
                }
                self.overlay = ov; // old strips are destroyed by Drop
            }
            Err(e) => {
                self.tray
                    .balloon("SoundSight", &format!("Overlay rebuild failed: {e}"), true)
            }
        }
    }

    fn reload_config(&mut self) {
        let (cfg, err) = load_config(&self.config_path);
        if let Some(e) = &err {
            self.tray.balloon("SoundSight config error", e, true);
            return; // keep the current, working config
        }
        if let Ok(mut c) = self.config.lock() {
            *c = cfg.clone();
        }
        self.frame_interval = frame_interval(&cfg);
        self.register_hotkey(&cfg);
        self.rebuild_overlay();
        self.capture.request_reinit();
        self.update_tip();
        self.tray.balloon("SoundSight", "Config reloaded.", false);
    }

    fn edit_config(&mut self) {
        if !self.config_path.exists() {
            let _ = std::fs::write(&self.config_path, DEFAULT_TOML);
        }
        let path = wide(&self.config_path.to_string_lossy());
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                w!("notepad.exe"),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    fn toggle_calibration(&mut self) {
        if let Some(h) = self.calib.take() {
            unsafe {
                let _ = DestroyWindow(h);
            }
        } else if let Ok(h) = calibrate::open(self.hinst) {
            self.calib = Some(h);
        }
        self.update_wanted();
    }

    fn command(&mut self, cmd: u32) {
        match cmd {
            tray::CMD_TOGGLE => self.set_visible(!self.overlay.is_visible()),
            tray::CMD_CALIBRATE => self.toggle_calibration(),
            tray::CMD_RELOAD => self.reload_config(),
            tray::CMD_EDIT_CONFIG => self.edit_config(),
            tray::CMD_QUIT => self.quit(),
            _ => {}
        }
    }

    fn quit(&mut self) {
        if let Some(h) = self.calib.take() {
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
        self.tray.remove();
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
            let _ = DestroyWindow(self.hwnd); // WM_DESTROY -> PostQuitMessage
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shared.notify_hwnd.store(0, SeqCst);
        unsafe {
            let _ = CloseHandle(self.timer);
        }
        // `capture` (joins the thread), `overlay` and `tray` drop after this.
    }
}

unsafe extern "system" fn controller_wndproc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            WM_APP_REDRAW => {
                if with_app(|a| a.on_redraw_request()).is_none() {
                    // Delivered re-entrantly (app state busy). Drop this frame but clear the
                    // flag so the capture thread posts again on the next change; otherwise
                    // the overlay would stop updating.
                    if let Some(sh) = SHARED.get() {
                        sh.redraw_pending.store(false, Release);
                    }
                }
                return LRESULT(0);
            }
            WM_APP_STREAM => {
                // Stream started/stopped; the calibration window polls the format itself.
                return LRESULT(0);
            }
            WM_HOTKEY if wp.0 as i32 == HOTKEY_ID => {
                with_app(|a| a.set_visible(!a.overlay.is_visible()));
                return LRESULT(0);
            }
            WM_APP_TRAY => {
                let event = (lp.0 & 0xFFFF) as u32;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    // Read state, release the borrow, then run the modal menu loop.
                    let Some((visible, calibrating)) =
                        with_app(|a| (a.overlay.is_visible(), a.calib.is_some()))
                    else {
                        return LRESULT(0);
                    };
                    let cmd = Tray::show_menu(hwnd, visible, calibrating);
                    with_app(|a| a.command(cmd));
                } else if event == WM_LBUTTONDBLCLK {
                    with_app(|a| a.set_visible(!a.overlay.is_visible()));
                }
                return LRESULT(0);
            }
            WM_APP_CALIB_CLOSED => {
                with_app(|a| {
                    a.calib = None;
                    a.update_wanted();
                });
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE => {
                // Resolution changed (e.g. the game switched modes): resize the strips.
                with_app(|a| a.rebuild_overlay());
                return LRESULT(0);
            }
            _ => {}
        }
        if msg != 0 && with_app(|a| a.taskbar_created == msg).unwrap_or(false) {
            with_app(|a| a.tray.add());
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}
