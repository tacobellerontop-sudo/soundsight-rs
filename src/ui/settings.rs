//! Settings window: every common option as a normal control, plus the live
//! per-channel speaker meters (the old calibration window).
//!
//! Plain Win32 controls (comctl32 trackbar + hotkey control, comdlg32 colour picker),
//! no GUI framework. The window only exists while open; closing it frees everything.
//! Changes apply immediately and are saved to `soundsight.toml` automatically.

use super::util::{wide, SETTINGS_CLASS};
use crate::config::{Config, OverlayStyle, SpeakerAngles};
use crate::dsp::layout::{Layout, MAX_CHANNELS};
use crate::hotkey;
use crate::state::Shared;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering::*};
use std::sync::{Arc, OnceLock};
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the controller window when the settings window is destroyed.
pub const WM_APP_SETTINGS_CLOSED: u32 = WM_APP + 4;

const METER_TIMER: usize = 1;
// Not exported by the `windows` crate version we use.
const SS_LEFT: u32 = 0;
const TBM_GETPOS: u32 = WM_USER;

// Control IDs. A slider's value label uses `id + VALUE`.
const VALUE: i32 = 100;
pub const ID_SHOW: i32 = 101;
pub const ID_STYLE: i32 = 102;
const ID_STYLE_NOTE: i32 = 103;
pub const ID_COLOR: i32 = 104;
const ID_SWATCH: i32 = 105;
const ID_OPACITY: i32 = 110;
const ID_THICK: i32 = 111;
const ID_LENGTH: i32 = 112;
const ID_RADIUS: i32 = 113;
const ID_GUIDE: i32 = 114;
const ID_SENS: i32 = 120;
const ID_FADE: i32 = 121;
const ID_OWN: i32 = 122;
const ID_OWNGUN: i32 = 125;
const ID_CENTER: i32 = 123;
const ID_AUTOSTEREO: i32 = 124;
const ID_HOTKEY: i32 = 130;
const ID_HOTKEY_NOTE: i32 = 131;
pub const ID_STARTUP: i32 = 132;
pub const ID_DEFAULTS: i32 = 140;
pub const ID_OPENCFG: i32 = 141;
pub const ID_CLOSE: i32 = 142;

// Layout, in 96-DPI units (scaled at creation).
const CLIENT_W: i32 = 840;
const CLIENT_H: i32 = 648;
const LABEL_X: i32 = 16;
const CTRL_X: i32 = 150;
const CTRL_W: i32 = 210;
const VALUE_X: i32 = 366;
const RIGHT_X: i32 = 450;
const RIGHT_W: i32 = 374;
const PANEL_TOP: i32 = 44;
const PANEL_H: i32 = 440;
const METER_ROW: i32 = 26;

/// Set while controls are being filled in, so their notifications are ignored.
static LOADING: AtomicBool = AtomicBool::new(false);
/// Colour shown in the swatch (0x00BBGGRR) and its brush.
static SWATCH_RGB: AtomicU32 = AtomicU32::new(0);
static SWATCH_BRUSH: AtomicIsize = AtomicIsize::new(0);
/// Which stereo note is currently shown (avoids re-setting text 20x per second).
static NOTE_STATE: AtomicU8 = AtomicU8::new(255);
/// Window DPI scale in 1/96 units.
static DPI: AtomicU32 = AtomicU32::new(96);
/// Whether the own-gunshot filter is enabled (for the speaker-test indicator).
static OWN_GUN_ON: AtomicBool = AtomicBool::new(true);

struct Ctx {
    shared: Arc<Shared>,
    controller: isize,
}
static CTX: OnceLock<Ctx> = OnceLock::new();

fn sc(v: i32) -> i32 {
    v * DPI.load(Relaxed) as i32 / 96
}

/// What the app must do after a control changed.
#[derive(Default, Debug)]
pub struct Effect {
    /// Overlay look changed (colour, size, style...).
    pub overlay: bool,
    /// A slider thumb is still being dragged: defer window rebuilds until release.
    pub dragging: bool,
    /// Detection parameters changed.
    pub detection: bool,
    pub hotkey: bool,
    pub visible: Option<bool>,
    pub startup: Option<bool>,
}

impl Effect {
    pub fn config_changed(&self) -> bool {
        self.overlay || self.detection || self.hotkey
    }
}

// Value <-> slider mappings.
fn sens_from_min_db(db: f32) -> i32 {
    ((-35.0 - db) * 2.0).round().clamp(0.0, 100.0) as i32
}
fn min_db_from_sens(s: i32) -> f32 {
    -35.0 - s as f32 / 2.0
}

pub struct SettingsWindow {
    pub hwnd: HWND,
    font: HFONT,
    bold: HFONT,
}

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
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_HOTKEY_CLASS | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }
    let class = wide(SETTINGS_CLASS);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hIcon: icon,
        hIconSm: icon,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        hbrBackground: unsafe { GetSysColorBrush(COLOR_BTNFACE) },
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(windows::core::Error::from_thread());
    }
    Ok(())
}

impl SettingsWindow {
    pub fn open(hinst: HINSTANCE, cfg: &Config, startup: bool) -> Result<SettingsWindow> {
        DPI.store(unsafe { GetDpiForSystem() }.max(96), Relaxed);
        NOTE_STATE.store(255, Relaxed);
        let class = wide(SETTINGS_CLASS);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let exstyle = WS_EX_CONTROLPARENT;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: sc(CLIENT_W),
            bottom: sc(CLIENT_H),
        };
        unsafe {
            let _ = AdjustWindowRectEx(&mut rc, style, false, exstyle);
            let hwnd = CreateWindowExW(
                exstyle,
                PCWSTR(class.as_ptr()),
                w!("SoundSight settings"),
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
            let font = CreateFontW(
                -sc(15),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            let bold = CreateFontW(
                -sc(15),
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            let win = SettingsWindow { hwnd, font, bold };
            win.build(hinst);
            win.load(cfg, startup);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            SetTimer(Some(hwnd), METER_TIMER, 50, None);
            Ok(win)
        }
    }

    // ---- control creation -------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn child(
        &self,
        hinst: HINSTANCE,
        class: PCWSTR,
        text: &str,
        style: u32,
        id: i32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> HWND {
        let t = wide(text);
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR(t.as_ptr()),
                WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
                sc(x),
                sc(y),
                sc(w),
                sc(h),
                Some(self.hwnd),
                Some(HMENU(id as isize as _)),
                Some(hinst),
                None,
            )
            .unwrap_or_default();
            SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(self.font.0 as usize)),
                Some(LPARAM(1)),
            );
            hwnd
        }
    }

    fn heading(&self, hinst: HINSTANCE, text: &str, x: i32, y: i32) {
        let h = self.child(hinst, w!("STATIC"), text, 0, -1, x, y, 300, 22);
        unsafe {
            SendMessageW(
                h,
                WM_SETFONT,
                Some(WPARAM(self.bold.0 as usize)),
                Some(LPARAM(1)),
            );
        }
    }

    fn label(&self, hinst: HINSTANCE, text: &str, id: i32, x: i32, y: i32, w: i32) {
        self.child(hinst, w!("STATIC"), text, SS_LEFT, id, x, y + 3, w, 20);
    }

    fn checkbox(&self, hinst: HINSTANCE, text: &str, id: i32, y: i32) {
        self.child(
            hinst,
            w!("BUTTON"),
            text,
            BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0,
            id,
            LABEL_X,
            y,
            400,
            24,
        );
    }

    fn slider(&self, hinst: HINSTANCE, text: &str, id: i32, y: i32, min: i32, max: i32) {
        self.label(hinst, text, -1, LABEL_X, y, CTRL_X - LABEL_X - 4);
        let tb = self.child(
            hinst,
            TRACKBAR_CLASSW,
            "",
            WS_TABSTOP.0 | TBS_HORZ | TBS_NOTICKS,
            id,
            CTRL_X - 6,
            y,
            CTRL_W + 12,
            26,
        );
        unsafe {
            SendMessageW(
                tb,
                TBM_SETRANGEMIN,
                Some(WPARAM(0)),
                Some(LPARAM(min as isize)),
            );
            SendMessageW(
                tb,
                TBM_SETRANGEMAX,
                Some(WPARAM(1)),
                Some(LPARAM(max as isize)),
            );
            SendMessageW(
                tb,
                TBM_SETPAGESIZE,
                Some(WPARAM(0)),
                Some(LPARAM(((max - min) / 10).max(1) as isize)),
            );
        }
        self.label(hinst, "", id + VALUE, VALUE_X + 6, y, 70);
    }

    fn button(&self, hinst: HINSTANCE, text: &str, id: i32, x: i32, y: i32, w: i32) {
        self.child(
            hinst,
            w!("BUTTON"),
            text,
            BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
            id,
            x,
            y,
            w,
            28,
        );
    }

    fn build(&self, hinst: HINSTANCE) {
        self.heading(hinst, "Overlay", LABEL_X, 14);
        self.checkbox(hinst, "Show overlay (or press the hotkey)", ID_SHOW, 40);

        self.label(hinst, "Style", -1, LABEL_X, 72, CTRL_X - LABEL_X);
        let combo = self.child(
            hinst,
            w!("COMBOBOX"),
            "",
            CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0,
            ID_STYLE,
            CTRL_X,
            70,
            CTRL_W + 60,
            200,
        );
        for st in OverlayStyle::ALL {
            let t = wide(st.label());
            unsafe {
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    Some(WPARAM(0)),
                    Some(LPARAM(t.as_ptr() as isize)),
                );
            }
        }
        self.child(
            hinst,
            w!("STATIC"),
            "",
            SS_LEFT,
            ID_STYLE_NOTE,
            CTRL_X,
            100,
            CTRL_W + 60,
            36,
        );

        self.label(hinst, "Colour", -1, LABEL_X, 140, CTRL_X - LABEL_X);
        self.button(hinst, "Change...", ID_COLOR, CTRL_X, 138, 96);
        self.child(
            hinst,
            w!("STATIC"),
            "",
            SS_LEFT,
            ID_SWATCH,
            CTRL_X + 106,
            140,
            40,
            24,
        );

        self.slider(hinst, "Opacity", ID_OPACITY, 172, 10, 100);
        self.slider(hinst, "Bar thickness", ID_THICK, 202, 2, 40);
        self.slider(hinst, "Bar length", ID_LENGTH, 232, 40, 800);
        self.slider(hinst, "Circle size", ID_RADIUS, 262, 30, 300);
        self.checkbox(hinst, "Faint guide ring (circle style)", ID_GUIDE, 292);

        self.heading(hinst, "Detection", LABEL_X, 328);
        self.slider(hinst, "Sensitivity", ID_SENS, 354, 0, 100);
        self.slider(hinst, "Fade time", ID_FADE, 384, 100, 1000);
        self.checkbox(hinst, "Filter my own sounds (footsteps)", ID_OWN, 414);
        self.checkbox(hinst, "Ignore my own gunshots", ID_OWNGUN, 442);
        self.checkbox(hinst, "Also hide centre-heavy sounds", ID_CENTER, 470);
        self.checkbox(
            hinst,
            "Detect stereo games on a 7.1 device (e.g. CS2)",
            ID_AUTOSTEREO,
            498,
        );

        self.heading(hinst, "General", LABEL_X, 534);
        self.label(
            hinst,
            "Show/hide hotkey",
            -1,
            LABEL_X,
            562,
            CTRL_X - LABEL_X,
        );
        self.child(
            hinst,
            HOTKEY_CLASSW,
            "",
            WS_TABSTOP.0 | WS_BORDER.0,
            ID_HOTKEY,
            CTRL_X,
            560,
            150,
            26,
        );
        self.label(hinst, "", ID_HOTKEY_NOTE, CTRL_X + 158, 562, 270);
        self.checkbox(hinst, "Start with Windows", ID_STARTUP, 594);

        self.heading(hinst, "Speaker test", RIGHT_X, 14);
        let by = PANEL_TOP + PANEL_H + 16;
        self.button(hinst, "Reset to defaults", ID_DEFAULTS, RIGHT_X, by, 130);
        self.button(
            hinst,
            "Open config file",
            ID_OPENCFG,
            RIGHT_X + 138,
            by,
            130,
        );
        self.button(
            hinst,
            "Close",
            ID_CLOSE,
            RIGHT_X + RIGHT_W - 96,
            by + 104,
            96,
        );
        let hint = "Changes apply immediately and are saved automatically.";
        self.label(hinst, hint, -1, RIGHT_X, by + 40, RIGHT_W);
    }

    // ---- control access -----------------------------------------------------

    fn item(&self, id: i32) -> HWND {
        unsafe { GetDlgItem(Some(self.hwnd), id).unwrap_or_default() }
    }

    fn send(&self, id: i32, msg: u32, wp: usize, lp: isize) -> isize {
        unsafe { SendMessageW(self.item(id), msg, Some(WPARAM(wp)), Some(LPARAM(lp))).0 }
    }

    fn checked(&self, id: i32) -> bool {
        self.send(id, BM_GETCHECK, 0, 0) == BST_CHECKED.0 as isize
    }

    fn set_checked(&self, id: i32, on: bool) {
        let v = if on { BST_CHECKED.0 } else { BST_UNCHECKED.0 };
        self.send(id, BM_SETCHECK, v as usize, 0);
    }

    fn slider_pos(&self, id: i32) -> i32 {
        self.send(id, TBM_GETPOS, 0, 0) as i32
    }

    fn set_slider(&self, id: i32, v: i32) {
        self.send(id, TBM_SETPOS, 1, v as isize);
    }

    fn set_text(&self, id: i32, s: &str) {
        let t = wide(s);
        unsafe {
            let _ = SetWindowTextW(self.item(id), PCWSTR(t.as_ptr()));
        }
    }

    fn enable(&self, id: i32, on: bool) {
        unsafe {
            let _ = EnableWindow(self.item(id), on);
        }
    }

    pub fn style(&self) -> OverlayStyle {
        let i = self.send(ID_STYLE, CB_GETCURSEL, 0, 0);
        OverlayStyle::ALL
            .get(i.max(0) as usize)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_hotkey_note(&self, s: &str) {
        self.set_text(ID_HOTKEY_NOTE, s);
    }

    /// Fill every control from `cfg`.
    pub fn load(&self, cfg: &Config, startup: bool) {
        LOADING.store(true, SeqCst);
        let (g, o, d, h) = (&cfg.general, &cfg.overlay, &cfg.detection, &cfg.heuristics);
        let style_idx = OverlayStyle::ALL
            .iter()
            .position(|&s| s == o.style)
            .unwrap_or(0);
        self.send(ID_STYLE, CB_SETCURSEL, style_idx, 0);
        self.set_slider(ID_OPACITY, (o.opacity * 100.0).round() as i32);
        self.set_slider(ID_THICK, o.thickness as i32);
        self.set_slider(ID_LENGTH, o.bar_length as i32);
        self.set_slider(ID_RADIUS, o.circle_radius as i32);
        self.set_checked(ID_GUIDE, o.circle_guide_opacity > 0.0);
        self.set_slider(ID_SENS, sens_from_min_db(d.min_db));
        self.set_slider(ID_FADE, d.decay_ms.round() as i32);
        self.set_checked(ID_OWN, d.subtract_common);
        self.set_checked(ID_OWNGUN, d.ignore_own_gunshots);
        OWN_GUN_ON.store(d.ignore_own_gunshots, Relaxed);
        self.set_checked(ID_CENTER, h.enabled);
        self.set_checked(ID_AUTOSTEREO, d.auto_stereo);
        self.set_checked(ID_STARTUP, startup);
        if let Ok((mods, vk)) = hotkey::parse(&g.hotkey) {
            let mut f = 0u32;
            if mods & hotkey::MOD_SHIFT != 0 {
                f |= HOTKEYF_SHIFT;
            }
            if mods & hotkey::MOD_CONTROL != 0 {
                f |= HOTKEYF_CONTROL;
            }
            if mods & hotkey::MOD_ALT != 0 {
                f |= HOTKEYF_ALT;
            }
            self.send(ID_HOTKEY, HKM_SETHOTKEY, ((f << 8) | vk) as usize, 0);
        }
        self.set_color(cfg.color_rgb());
        self.refresh_labels(cfg);
        LOADING.store(false, SeqCst);
    }

    pub fn set_visible_checkbox(&self, on: bool) {
        LOADING.store(true, SeqCst);
        self.set_checked(ID_SHOW, on);
        LOADING.store(false, SeqCst);
    }

    pub fn set_color(&self, (r, g, b): (u8, u8, u8)) {
        let bgr = ((b as u32) << 16) | ((g as u32) << 8) | r as u32;
        SWATCH_RGB.store(bgr, Relaxed);
        unsafe {
            let new = CreateSolidBrush(COLORREF(bgr));
            let old = SWATCH_BRUSH.swap(new.0 as isize, Relaxed);
            if old != 0 {
                let _ = DeleteObject(HGDIOBJ(old as _));
            }
            let _ = InvalidateRect(Some(self.item(ID_SWATCH)), None, true);
        }
    }

    fn refresh_labels(&self, cfg: &Config) {
        let o = &cfg.overlay;
        self.set_text(ID_OPACITY + VALUE, &format!("{:.0} %", o.opacity * 100.0));
        self.set_text(ID_THICK + VALUE, &format!("{} px", o.thickness));
        self.set_text(ID_LENGTH + VALUE, &format!("{} px", o.bar_length));
        self.set_text(ID_RADIUS + VALUE, &format!("{} px", o.circle_radius));
        self.set_text(
            ID_SENS + VALUE,
            &format!("{}", sens_from_min_db(cfg.detection.min_db)),
        );
        self.set_text(
            ID_FADE + VALUE,
            &format!("{:.0} ms", cfg.detection.decay_ms),
        );
        let circle = o.style == OverlayStyle::Circle;
        self.enable(ID_LENGTH, !circle);
        self.enable(ID_RADIUS, circle);
        self.enable(ID_GUIDE, circle);
        NOTE_STATE.store(255, Relaxed); // style may have changed: recompute note
    }

    /// Read the control `id` into `cfg`. `code` is the notification code.
    pub fn apply_control(&self, id: i32, code: u32, cfg: &mut Config) -> Effect {
        let mut e = Effect::default();
        if LOADING.load(SeqCst) {
            return e;
        }
        let o = &mut cfg.overlay;
        let d = &mut cfg.detection;
        match id {
            ID_SHOW => e.visible = Some(self.checked(ID_SHOW)),
            ID_STARTUP => e.startup = Some(self.checked(ID_STARTUP)),
            ID_STYLE if code == CBN_SELCHANGE => {
                o.style = self.style();
                e.overlay = true;
            }
            ID_OPACITY => {
                o.opacity = self.slider_pos(id) as f32 / 100.0;
                e.overlay = true;
            }
            ID_THICK => {
                o.thickness = self.slider_pos(id) as u32;
                e.overlay = true;
            }
            ID_LENGTH => {
                o.bar_length = self.slider_pos(id) as u32;
                e.overlay = true;
            }
            ID_RADIUS => {
                o.circle_radius = self.slider_pos(id) as u32;
                e.overlay = true;
            }
            ID_GUIDE => {
                o.circle_guide_opacity = if self.checked(id) { 0.15 } else { 0.0 };
                e.overlay = true;
            }
            ID_SENS => {
                d.min_db = min_db_from_sens(self.slider_pos(id));
                e.detection = true;
            }
            ID_FADE => {
                d.decay_ms = self.slider_pos(id) as f32;
                e.detection = true;
            }
            ID_OWN => {
                d.subtract_common = self.checked(id);
                e.detection = true;
            }
            ID_OWNGUN => {
                d.ignore_own_gunshots = self.checked(id);
                OWN_GUN_ON.store(d.ignore_own_gunshots, Relaxed);
                e.detection = true;
            }
            ID_AUTOSTEREO => {
                d.auto_stereo = self.checked(id);
                e.detection = true;
            }
            ID_CENTER => {
                cfg.heuristics.enabled = self.checked(id);
                e.detection = true;
            }
            ID_HOTKEY if code == EN_CHANGE => {
                let v = self.send(ID_HOTKEY, HKM_GETHOTKEY, 0, 0) as u32;
                let (vk, f) = (v & 0xFF, (v >> 8) & 0xFF);
                if vk != 0 {
                    let mut mods = 0;
                    if f & HOTKEYF_SHIFT != 0 {
                        mods |= hotkey::MOD_SHIFT;
                    }
                    if f & HOTKEYF_CONTROL != 0 {
                        mods |= hotkey::MOD_CONTROL;
                    }
                    if f & HOTKEYF_ALT != 0 {
                        mods |= hotkey::MOD_ALT;
                    }
                    match hotkey::format(mods, vk) {
                        Some(text) => {
                            cfg.general.hotkey = text;
                            e.hotkey = true;
                        }
                        None => self.set_hotkey_note("That key can't be used."),
                    }
                }
            }
            _ => {}
        }
        e.dragging = code == TB_THUMBTRACK;
        if e.config_changed() {
            self.refresh_labels(cfg);
        }
        e
    }

    pub fn destroy(&self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bold.into());
            let old = SWATCH_BRUSH.swap(0, Relaxed);
            if old != 0 {
                let _ = DeleteObject(HGDIOBJ(old as _));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = loword(wp.0) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                super::app::on_settings_control(id, code);
                LRESULT(0)
            }
            WM_HSCROLL => {
                // Trackbars notify the parent; lParam is the trackbar's HWND.
                let id = GetDlgCtrlID(HWND(lp.0 as _));
                super::app::on_settings_control(id, loword(wp.0));
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC if GetDlgCtrlID(HWND(lp.0 as _)) == ID_SWATCH => {
                LRESULT(SWATCH_BRUSH.load(Relaxed))
            }
            WM_TIMER => {
                update_note(hwnd);
                let r = panel_rect();
                let _ = InvalidateRect(Some(hwnd), Some(&r), false);
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), METER_TIMER);
                if let Some(c) = CTX.get() {
                    let _ = PostMessageW(
                        Some(HWND(c.controller as _)),
                        WM_APP_SETTINGS_CLOSED,
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

/// Tell the user when the chosen style can't show front/behind with their audio.
fn update_note(hwnd: HWND) {
    let Some(ctx) = CTX.get() else { return };
    let sh = &ctx.shared;
    let style = unsafe {
        let combo = GetDlgItem(Some(hwnd), ID_STYLE).unwrap_or_default();
        let i = SendMessageW(combo, CB_GETCURSEL, None, None).0;
        OverlayStyle::ALL
            .get(i.max(0) as usize)
            .copied()
            .unwrap_or_default()
    };
    let ch = sh.channels.load(Relaxed) as usize;
    let rear = ch > 0
        && Layout::from_mask(ch, sh.channel_mask.load(Relaxed), &SpeakerAngles::default())
            .has_rear();
    let state: u8 = if !sh.capturing.load(Relaxed) {
        0
    } else if !rear {
        1
    } else if sh.stereo_content.load(Relaxed) {
        2
    } else {
        3
    };
    let key = state * 4 + style as u8;
    if NOTE_STATE.swap(key, Relaxed) == key {
        return;
    }
    let text = match (state, style) {
        (1, _) => {
            "Stereo output: only left/right is possible, not front/behind (see README: 7.1 setup)."
        }
        (2, _) => "This game sends stereo only: front/behind can't be detected.",
        (3, OverlayStyle::EdgesFrontBack) => {
            "Top bar = in front, bottom bar = behind. Side bars show left/right."
        }
        (3, OverlayStyle::Circle) => "Arcs on the ring point at the sound. Top = in front.",
        (3, _) => "Top edge = in front, bottom edge = behind.",
        _ => "",
    };
    let t = wide(text);
    unsafe {
        if let Ok(note) = GetDlgItem(Some(hwnd), ID_STYLE_NOTE) {
            let _ = SetWindowTextW(note, PCWSTR(t.as_ptr()));
        }
    }
}

fn panel_rect() -> RECT {
    RECT {
        left: sc(RIGHT_X),
        top: sc(PANEL_TOP),
        right: sc(RIGHT_X + RIGHT_W),
        bottom: sc(PANEL_TOP + PANEL_H),
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

/// Paint the speaker-test panel (double buffered). Controls paint themselves.
unsafe fn paint(hwnd: HWND) {
    let Some(ctx) = CTX.get() else { return };
    let sh = &ctx.shared;
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let r = panel_rect();
        let (pw, ph) = (r.right - r.left, r.bottom - r.top);

        let dc = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, pw, ph);
        let old = SelectObject(dc, bmp.into());
        let font = HGDIOBJ(SendMessageW(hwnd, WM_GETFONT, None, None).0 as _);
        let child_font = GetDlgItem(Some(hwnd), ID_STYLE)
            .map(|c| HGDIOBJ(SendMessageW(c, WM_GETFONT, None, None).0 as _))
            .unwrap_or(font);
        let old_font = SelectObject(dc, child_font);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(bgr(230, 230, 230)));
        fill(dc, 0, 0, pw, ph, bgr(24, 24, 28));

        let pad = sc(12);
        let line = sc(20);
        let channels = sh.channels.load(Relaxed) as usize;
        let mask = sh.channel_mask.load(Relaxed);
        let rate = sh.sample_rate.load(Relaxed);
        if sh.capturing.load(Relaxed) && channels > 0 {
            let layout = Layout::from_mask(channels, mask, &SpeakerAngles::default());
            text(
                dc,
                pad,
                pad,
                &format!("Output: {} ({} ch), {} Hz", layout.name, channels, rate),
            );
            text(
                dc,
                pad,
                pad + line,
                if layout.has_rear() && sh.stereo_content.load(Relaxed) {
                    "Stereo-only game detected: FL/FR = left/right."
                } else if layout.has_rear() {
                    "Front/behind separation available."
                } else {
                    "Stereo device: left/right only."
                },
            );

            // Own-gunshot filter indicator: lights up while it is ignoring your shots.
            let holding = sh.own_shot.load(Relaxed);
            let (dot, msg) = if !OWN_GUN_ON.load(Relaxed) {
                (bgr(90, 90, 100), "Own-gunshot filter: off")
            } else if holding {
                (bgr(255, 200, 0), "Own-gunshot filter: ignoring your shot")
            } else {
                (
                    bgr(70, 160, 90),
                    "Own-gunshot filter: ready (lights up when you fire)",
                )
            };
            let d = sc(10);
            let ty = pad + line * 2 + (line - d) / 2;
            fill(dc, pad, ty, pad + d, ty + d, dot);
            text(dc, pad + d + sc(8), pad + line * 2, msg);

            let mut meters = [0u8; MAX_CHANNELS];
            sh.read_meters(&mut meters);
            let top = pad + line * 3 + sc(6);
            let (x0, x1) = (sc(48), pw - sc(78));
            let row = sc(METER_ROW);
            for c in 0..layout.analysed_channels() {
                let y = top + c as i32 * row;
                text(dc, pad, y + sc(2), layout.speakers[c].label());
                fill(dc, x0, y + sc(2), x1, y + row - sc(6), bgr(50, 50, 58));
                // meter byte: -90..0 dB -> 0..255; show -60..0 dB.
                let db = meters[c] as f32 / 255.0 * 90.0 - 90.0;
                let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let len = ((x1 - x0) as f32 * frac) as i32;
                let color = if layout.vectors[c] == (0.0, 0.0) {
                    bgr(120, 120, 130)
                } else {
                    SWATCH_RGB.load(Relaxed)
                };
                if len > 0 {
                    fill(dc, x0, y + sc(2), x0 + len, y + row - sc(6), color);
                }
                let label = if db <= -89.0 {
                    "-inf".to_string()
                } else {
                    format!("{db:.0} dB")
                };
                text(dc, x1 + sc(8), y + sc(2), &label);
            }
            let y = top + MAX_CHANNELS as i32 * row + sc(8);
            text(dc, pad, y, "Run Windows' speaker test (Sound settings >");
            text(
                dc,
                pad,
                y + line,
                "your device > Configure > Test). Each speaker",
            );
            text(
                dc,
                pad,
                y + 2 * line,
                "should light only its own bar. Grey = LFE.",
            );
        } else {
            text(dc, pad, pad, "No audio capture right now.");
            text(
                dc,
                pad,
                pad + line,
                "Is an output device enabled? Capture restarts",
            );
            text(
                dc,
                pad,
                pad + 2 * line,
                "automatically when one becomes available.",
            );
        }

        let _ = BitBlt(hdc, r.left, r.top, pw, ph, Some(dc), 0, 0, SRCCOPY);
        SelectObject(dc, old_font);
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        let _ = EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitivity_mapping_round_trips() {
        assert_eq!(sens_from_min_db(-60.0), 50);
        for s in 0..=100 {
            assert_eq!(sens_from_min_db(min_db_from_sens(s)), s);
        }
    }
}
