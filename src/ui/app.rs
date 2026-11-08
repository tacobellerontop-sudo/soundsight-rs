//! Application wiring and the UI thread's message loop.
//!
//! The UI thread blocks in `MsgWaitForMultipleObjectsEx` on its message queue plus
//! one high-resolution waitable timer (used only to enforce the refresh cap). With no
//! sound it receives no messages and uses no CPU.
//!
//! Redraw flow: capture thread changes bins -> posts WM_APP_REDRAW (at most one
//! outstanding) -> if a frame was drawn less than 1/refresh_hz ago, arm the timer for
//! the remainder, else draw now. Bursts of updates therefore coalesce into one frame.
//!
//! Settings: the settings window reports control changes to `on_settings_control`;
//! the new config applies immediately (overlay restyle/rebuild, analyser parameters,
//! hotkey), a short preview pattern is shown, and the file is saved after a short delay.

use super::overlay::{self, Overlay};
use super::settings::{self, SettingsWindow, WM_APP_SETTINGS_CLOSED};
use super::tray::{self, Tray, WM_APP_TRAY};
use super::util::{make_icon, set_startup, startup_enabled, wide, CONTROLLER_CLASS};
use crate::audio::capture::{CaptureThread, WM_APP_REDRAW, WM_APP_STREAM};
use crate::config::{parse_hex_color, Config};
use crate::dsp::analyzer::BINS;
use crate::dsp::direction::angle_to_bins;
use crate::hotkey;
use crate::state::Shared;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

const HOTKEY_ID: i32 = 1;
/// Controller timers.
const TIMER_SAVE: usize = 1;
const TIMER_PREVIEW: usize = 2;
const SAVE_DELAY_MS: u32 = 600;
const PREVIEW_MS: u64 = 2000;

/// The settings window, if open (for IsDialogMessage in the message loop).
static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

struct App {
    hinst: HINSTANCE,
    hwnd: HWND,
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    config_path: PathBuf,
    capture: CaptureThread,
    overlay: Overlay,
    tray: Tray,
    settings: Option<SettingsWindow>,
    /// Config the current overlay windows were built with (to detect rebuilds).
    overlay_cfg: Config,
    preview_until: Option<Instant>,
    save_pending: bool,
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
            let _ = std::fs::write(path, Config::default().to_toml());
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
        settings::register_class(hinst, icon, shared.clone(), hwnd)?;
        // Keep the Run entry pointing at this exe if it was moved.
        if startup_enabled() {
            set_startup(true);
        }

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
            settings: None,
            overlay_cfg: cfg.clone(),
            preview_until: None,
            save_pending: false,
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
        // `SoundSight.exe --settings` opens the settings window right away.
        if std::env::args().any(|a| a.eq_ignore_ascii_case("--settings")) {
            with_app(|a| a.open_settings());
        }
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
                // Tab / arrow-key navigation between the settings controls.
                let sw = SETTINGS_HWND.load(Relaxed);
                if sw != 0 && IsDialogMessageW(HWND(sw as _), &msg).as_bool() {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl App {
    fn register_hotkey(&mut self, cfg: &Config) -> bool {
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
                    if let Some(s) = &self.settings {
                        s.set_hotkey_note("");
                    }
                    return true;
                }
                let msg = format!("{} is already used by another program.", cfg.general.hotkey);
                match &self.settings {
                    Some(s) => s.set_hotkey_note(&msg),
                    None => self.tray.balloon(
                        "SoundSight hotkey",
                        &format!("{msg} Pick another in Settings."),
                        true,
                    ),
                }
            }
            Err(e) => self.tray.balloon("SoundSight hotkey", &e, true),
        }
        false
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
        let wanted = self.overlay.is_visible() || self.settings.is_some();
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
        if let Some(s) = &self.settings {
            s.set_visible_checkbox(v);
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
        let bins = self.current_bins();
        if self.overlay.is_visible() {
            self.overlay.render(&bins);
        }
        self.last_frame = Instant::now();
    }

    /// Live bins, plus the preview pattern for a moment after a settings change.
    fn current_bins(&mut self) -> [u8; BINS] {
        let mut bins = [0u8; BINS];
        self.shared.read_bins(&mut bins);
        if let Some(until) = self.preview_until {
            if Instant::now() < until {
                // Front-right, behind-left and a quieter one on the left.
                for (deg, v) in [(30.0, 235u8), (-135.0, 170), (-80.0, 90)] {
                    let (lo, _, _) = angle_to_bins(deg, BINS);
                    bins[lo] = bins[lo].max(v);
                }
            } else {
                self.preview_until = None;
            }
        }
        bins
    }

    fn start_preview(&mut self) {
        self.preview_until = Some(Instant::now() + Duration::from_millis(PREVIEW_MS));
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_PREVIEW, PREVIEW_MS as u32 + 50, None);
        }
    }

    fn rebuild_overlay(&mut self) {
        let cfg = self.config.lock().map(|c| c.clone()).unwrap_or_default();
        self.overlay_cfg = cfg.clone();
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
        if let Some(s) = &self.settings {
            s.load(&cfg, startup_enabled());
        }
        self.tray.balloon("SoundSight", "Config reloaded.", false);
    }

    fn save_now(&mut self) {
        self.save_pending = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_SAVE);
        }
        let text = self.config.lock().map(|c| c.to_toml()).unwrap_or_default();
        if let Err(e) = std::fs::write(&self.config_path, text) {
            let msg = format!("Settings could not be saved to {}: {e}. Move SoundSight.exe to a folder you can write to.", self.config_path.display());
            self.tray.balloon("SoundSight", &msg, true);
        }
    }

    /// Apply a new config from the settings window.
    fn apply_settings(&mut self, new: Config, e: &settings::Effect) {
        if let Ok(mut c) = self.config.lock() {
            *c = new.clone();
        }
        if e.overlay {
            if Overlay::needs_rebuild(&self.overlay_cfg, &new) {
                if !e.dragging {
                    self.rebuild_overlay();
                }
            } else {
                let bins = self.current_bins();
                self.overlay.set_style(&new, &bins);
            }
            self.start_preview();
            self.draw_frame();
        }
        if e.detection {
            self.shared.params_version.fetch_add(1, Release);
            self.capture.wake();
        }
        if e.hotkey {
            self.register_hotkey(&new);
            self.update_tip();
        }
        if e.config_changed() {
            self.save_pending = true;
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_SAVE, SAVE_DELAY_MS, None);
            }
        }
    }

    fn settings_control(&mut self, id: i32, code: u32) {
        match id {
            settings::ID_CLOSE => return self.close_settings(),
            settings::ID_OPENCFG => {
                self.save_now();
                return self.edit_config();
            }
            settings::ID_DEFAULTS => {
                let mut d = Config::default();
                // Keep the hotkey: resetting it is surprising.
                d.general.hotkey = self
                    .config
                    .lock()
                    .map(|c| c.general.hotkey.clone())
                    .unwrap_or(d.general.hotkey);
                if let Some(s) = &self.settings {
                    s.load(&d, startup_enabled());
                }
                let e = settings::Effect {
                    overlay: true,
                    detection: true,
                    hotkey: true,
                    ..Default::default()
                };
                return self.apply_settings(d, &e);
            }
            _ => {}
        }
        let Some(win) = &self.settings else { return };
        let mut cfg = self.config.lock().map(|c| c.clone()).unwrap_or_default();
        let e = win.apply_control(id, code, &mut cfg);
        if let Some(v) = e.visible {
            self.set_visible(v);
        }
        if let Some(on) = e.startup {
            if !set_startup(on) {
                self.tray.balloon(
                    "SoundSight",
                    "Could not change the Windows startup setting.",
                    true,
                );
            }
        }
        if e.config_changed() {
            self.apply_settings(cfg, &e);
        }
    }

    fn open_settings(&mut self) {
        if let Some(s) = &self.settings {
            unsafe {
                let _ = ShowWindow(s.hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(s.hwnd);
            }
            return;
        }
        let cfg = self.config.lock().map(|c| c.clone()).unwrap_or_default();
        match SettingsWindow::open(self.hinst, &cfg, startup_enabled()) {
            Ok(win) => {
                win.set_visible_checkbox(self.overlay.is_visible());
                SETTINGS_HWND.store(win.hwnd.0 as isize, Relaxed);
                self.settings = Some(win);
                // Show what the current style looks like.
                self.start_preview();
                self.draw_frame();
                if !self.register_hotkey(&cfg) {
                    // note shown in the window
                }
            }
            Err(e) => self
                .tray
                .balloon("SoundSight", &format!("Cannot open settings: {e}"), true),
        }
        self.update_wanted();
    }

    fn close_settings(&mut self) {
        if let Some(s) = self.settings.take() {
            SETTINGS_HWND.store(0, Relaxed);
            s.destroy();
        }
        if self.save_pending {
            self.save_now();
        }
        self.update_wanted();
    }

    fn edit_config(&mut self) {
        if !self.config_path.exists() {
            self.save_now();
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

    fn command(&mut self, cmd: u32) {
        match cmd {
            tray::CMD_TOGGLE => self.set_visible(!self.overlay.is_visible()),
            tray::CMD_SETTINGS => self.open_settings(),
            tray::CMD_RELOAD => self.reload_config(),
            tray::CMD_QUIT => self.quit(),
            _ => {}
        }
    }

    fn quit(&mut self) {
        self.close_settings();
        if self.save_pending {
            self.save_now();
        }
        self.tray.remove();
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
            let _ = DestroyWindow(self.hwnd); // WM_DESTROY -> PostQuitMessage
        }
    }
}

/// Called by the settings window procedure for every control notification.
pub(super) fn on_settings_control(id: i32, code: u32) {
    if id == settings::ID_COLOR && code == BN_CLICKED {
        // The colour dialog runs a modal loop: don't hold the app borrow across it.
        let Some((owner, current)) = with_app(|a| {
            let c = a
                .config
                .lock()
                .map(|c| c.color_rgb())
                .unwrap_or((255, 59, 48));
            (a.settings.as_ref().map(|s| s.hwnd).unwrap_or_default(), c)
        }) else {
            return;
        };
        if let Some((r, g, b)) = pick_color(owner, current) {
            with_app(|a| {
                let mut cfg = a.config.lock().map(|c| c.clone()).unwrap_or_default();
                cfg.overlay.color = format!("#{r:02X}{g:02X}{b:02X}");
                debug_assert!(parse_hex_color(&cfg.overlay.color).is_some());
                if let Some(s) = &a.settings {
                    s.set_color((r, g, b));
                }
                let e = settings::Effect {
                    overlay: true,
                    ..Default::default()
                };
                a.apply_settings(cfg, &e);
            });
        }
        return;
    }
    with_app(|a| a.settings_control(id, code));
}

fn pick_color(owner: HWND, (r, g, b): (u8, u8, u8)) -> Option<(u8, u8, u8)> {
    static mut CUSTOM: [COLORREF; 16] = [COLORREF(0x00FFFFFF); 16];
    let mut cc = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: owner,
        rgbResult: COLORREF(((b as u32) << 16) | ((g as u32) << 8) | r as u32),
        // Only touched from the UI thread.
        lpCustColors: std::ptr::addr_of_mut!(CUSTOM) as *mut COLORREF,
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    if unsafe { ChooseColorW(&mut cc) }.as_bool() {
        let c = cc.rgbResult.0;
        Some((c as u8, (c >> 8) as u8, (c >> 16) as u8))
    } else {
        None
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
                // Stream started/stopped; the settings window polls the format itself.
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
                    let Some(visible) = with_app(|a| a.overlay.is_visible()) else {
                        return LRESULT(0);
                    };
                    let cmd = Tray::show_menu(hwnd, visible);
                    with_app(|a| a.command(cmd));
                } else if event == WM_LBUTTONDBLCLK || event == WM_LBUTTONUP {
                    with_app(|a| a.open_settings());
                }
                return LRESULT(0);
            }
            WM_APP_SETTINGS_CLOSED => {
                // Closed with the title-bar X (the window is already destroyed).
                with_app(|a| {
                    if a.settings.take().is_some() {
                        SETTINGS_HWND.store(0, Relaxed);
                    }
                    if a.save_pending {
                        a.save_now();
                    }
                    a.update_wanted();
                });
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == TIMER_SAVE => {
                with_app(|a| a.save_now());
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == TIMER_PREVIEW => {
                let _ = KillTimer(Some(hwnd), TIMER_PREVIEW);
                with_app(|a| {
                    // Thumb released while dragging a size slider: rebuild now.
                    let cfg = a.config.lock().map(|c| c.clone()).unwrap_or_default();
                    if Overlay::needs_rebuild(&a.overlay_cfg, &cfg) {
                        a.rebuild_overlay();
                    }
                    a.draw_frame();
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
