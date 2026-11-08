//! Configuration, loaded from `soundsight.toml` next to the executable.
//! Every field has a default, so a partial (or missing) file is fine.

use serde::Deserialize;
use std::fmt::Write as _;

/// How sounds are drawn.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayStyle {
    /// Bars on the screen edges where the sound's direction points.
    #[default]
    Edges,
    /// Left/right edges show the side; separate bars at the top/bottom show
    /// whether the sound is in front or behind.
    EdgesFrontBack,
    /// A small ring around the crosshair with arcs pointing at sounds.
    Circle,
}

impl OverlayStyle {
    pub const ALL: [OverlayStyle; 3] = [
        OverlayStyle::Edges,
        OverlayStyle::EdgesFrontBack,
        OverlayStyle::Circle,
    ];

    pub fn key(self) -> &'static str {
        match self {
            OverlayStyle::Edges => "edges",
            OverlayStyle::EdgesFrontBack => "edges_front_back",
            OverlayStyle::Circle => "circle",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OverlayStyle::Edges => "Screen edges",
            OverlayStyle::EdgesFrontBack => "Screen edges + front/behind bars",
            OverlayStyle::Circle => "Circle around crosshair",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub overlay: Overlay,
    pub detection: Detection,
    pub heuristics: Heuristics,
    pub speakers: SpeakerAngles,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// e.g. "F10", "Ctrl+Shift+S", "Alt+F9".
    pub hotkey: String,
    pub start_visible: bool,
    /// Maximum overlay redraw rate. Redraws only happen when something changed.
    pub refresh_hz: u32,
    /// Register the capture thread with MMCSS ("Games" task). Off by default; see README.
    pub mmcss: bool,
    /// While audio plays, read it every 20 ms instead of on every 10 ms engine event:
    /// half the wakeups, bars may appear up to 10 ms later.
    pub power_saving: bool,
}

impl Default for General {
    fn default() -> Self {
        General {
            hotkey: "F10".into(),
            start_visible: true,
            refresh_hz: 60,
            mmcss: false,
            power_saving: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    pub style: OverlayStyle,
    /// Circle style: inner radius of the ring in physical pixels.
    pub circle_radius: u32,
    /// Circle style: opacity of the faint guide ring (0 = no guide ring).
    pub circle_guide_opacity: f32,
    /// Bar thickness in physical pixels for the loudest sounds (also the thickness of
    /// the edge windows).
    pub thickness: u32,
    /// Quietest visible sound gets this fraction of `thickness` (0..1). Bars grow
    /// thicker as sounds get louder/closer.
    pub min_thickness_frac: f32,
    /// Bar length in pixels at full intensity.
    pub bar_length: u32,
    /// Fraction of `bar_length` shown for the quietest visible sound (0..1).
    pub min_length_frac: f32,
    /// Opacity at full intensity (0..1).
    pub opacity: f32,
    /// Fraction of `opacity` used for the quietest visible sound (0..1).
    pub min_opacity_frac: f32,
    /// "#RRGGBB"
    pub color: String,
    /// Bins below this intensity (0..1) are not drawn.
    pub min_intensity: f32,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay {
            style: OverlayStyle::Edges,
            circle_radius: 70,
            circle_guide_opacity: 0.15,
            thickness: 12,
            min_thickness_frac: 0.4,
            bar_length: 240,
            min_length_frac: 0.3,
            opacity: 0.9,
            min_opacity_frac: 0.35,
            color: "#FF3B30".into(),
            min_intensity: 0.04,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Detection {
    /// Analysis block length in ms (energy is computed per block).
    pub block_ms: f32,
    pub bandpass: bool,
    pub band_low_hz: f32,
    pub band_high_hz: f32,
    /// Band energy (dBFS) that maps to intensity 0 ...
    pub min_db: f32,
    /// ... and to intensity 1.
    pub max_db: f32,
    /// A block must exceed the adaptive noise floor by this many dB to count.
    pub snr_db: f32,
    /// Noise floor time constant when the signal is louder than the floor (slow).
    pub floor_rise_s: f32,
    /// Noise floor time constant when the signal is quieter than the floor (fast).
    pub floor_fall_s: f32,
    /// Bar fade-out time constant in ms.
    pub decay_ms: f32,
    /// Subtract the quietest channel from all channels before finding directions.
    /// Removes sounds that play equally everywhere (your own footsteps/gun).
    pub subtract_common: bool,
    /// A second, simultaneous sound is shown only if it has at least this fraction
    /// of the strongest sound's energy (0..1).
    pub cluster_min_frac: f32,
    /// On 5.1/7.1 devices, detect stereo-only content (e.g. CS2 through Voicemeeter)
    /// and map FL/FR to hard left/right.
    pub auto_stereo: bool,
    /// Hide your own gunshots: loud, front-heavy, left/right-balanced sounds start a
    /// short hold during which only clearly directional side/rear sounds are shown.
    pub ignore_own_gunshots: bool,
    /// How long (ms) that hold lasts after each own shot.
    pub own_shot_hold_ms: f32,
}

impl Default for Detection {
    fn default() -> Self {
        Detection {
            block_ms: 10.0,
            bandpass: true,
            band_low_hz: 200.0,
            band_high_hz: 4000.0,
            min_db: -60.0,
            max_db: -18.0,
            snr_db: 6.0,
            floor_rise_s: 3.0,
            floor_fall_s: 0.3,
            decay_ms: 250.0,
            subtract_common: true,
            cluster_min_frac: 0.25,
            auto_stereo: true,
            ignore_own_gunshots: true,
            own_shot_hold_ms: 150.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Heuristics {
    /// Master switch for the "probably my own sound / music" suppression.
    pub enabled: bool,
    /// Suppress blocks where the centre channel carries more than this share of
    /// the directional energy (surround layouts only).
    pub max_center_ratio: f32,
    /// Suppress blocks whose energy vector is less focused than this (0..1).
    /// Catches centre-panned stereo and diffuse/ambient sound.
    pub min_focus: f32,
}

impl Default for Heuristics {
    fn default() -> Self {
        Heuristics {
            enabled: true,
            max_center_ratio: 0.75,
            min_focus: 0.15,
        }
    }
}

/// Speaker angles in degrees from straight ahead (positive values; left speakers
/// use the negated angle).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SpeakerAngles {
    pub front: f32,
    pub side: f32,
    pub back: f32,
    /// Used for the single surround pair of 5.1 / quad layouts.
    pub surround_pair: f32,
    /// Used for plain stereo (L/R). 90 puts hard-left sounds on the left screen edge.
    pub stereo: f32,
}

impl Default for SpeakerAngles {
    fn default() -> Self {
        SpeakerAngles {
            front: 30.0,
            side: 90.0,
            back: 150.0,
            surround_pair: 110.0,
            stereo: 90.0,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        c.sanitize();
        Ok(c)
    }

    /// Clamp values into ranges the rest of the program can rely on.
    pub fn sanitize(&mut self) {
        let g = &mut self.general;
        g.refresh_hz = g.refresh_hz.clamp(1, 240);
        let o = &mut self.overlay;
        o.thickness = o.thickness.clamp(1, 64);
        o.circle_radius = o.circle_radius.clamp(10, 1000);
        o.circle_guide_opacity = o.circle_guide_opacity.clamp(0.0, 1.0);
        o.bar_length = o.bar_length.clamp(4, 4000);
        o.min_length_frac = o.min_length_frac.clamp(0.0, 1.0);
        o.min_thickness_frac = o.min_thickness_frac.clamp(0.0, 1.0);
        o.opacity = o.opacity.clamp(0.0, 1.0);
        o.min_opacity_frac = o.min_opacity_frac.clamp(0.0, 1.0);
        o.min_intensity = o.min_intensity.clamp(0.0, 1.0);
        let d = &mut self.detection;
        d.block_ms = d.block_ms.clamp(2.0, 50.0);
        d.band_low_hz = d.band_low_hz.clamp(10.0, 20_000.0);
        d.band_high_hz = d.band_high_hz.clamp(d.band_low_hz + 1.0, 22_000.0);
        if d.max_db <= d.min_db + 1.0 {
            d.max_db = d.min_db + 1.0;
        }
        d.snr_db = d.snr_db.clamp(0.0, 40.0);
        d.floor_rise_s = d.floor_rise_s.max(0.01);
        d.floor_fall_s = d.floor_fall_s.max(0.01);
        d.decay_ms = d.decay_ms.clamp(10.0, 5000.0);
        d.cluster_min_frac = d.cluster_min_frac.clamp(0.0, 1.0);
        d.own_shot_hold_ms = d.own_shot_hold_ms.clamp(0.0, 2000.0);
        let h = &mut self.heuristics;
        h.max_center_ratio = h.max_center_ratio.clamp(0.0, 1.0);
        h.min_focus = h.min_focus.clamp(0.0, 1.0);
    }

    /// Parse `overlay.color`; falls back to red on malformed input.
    pub fn color_rgb(&self) -> (u8, u8, u8) {
        parse_hex_color(&self.overlay.color).unwrap_or((0xFF, 0x3B, 0x30))
    }
}

impl Config {
    /// Write the config as commented TOML. This is also the file created on first run,
    /// so the defaults have exactly one source of truth (`Default` impls).
    pub fn to_toml(&self) -> String {
        let (g, o, d, h, sp) = (
            &self.general,
            &self.overlay,
            &self.detection,
            &self.heuristics,
            &self.speakers,
        );
        let mut s = String::with_capacity(4096);
        let q = |v: &str| format!("{v:?}");
        let _ = write!(
            s,
            r#"# SoundSight-RS configuration. Most settings are easier to change in the
# Settings window (tray icon -> Settings). Changes made there are saved here.
# Every key is optional; missing keys use defaults.

[general]
# Show/hide hotkey (RegisterHotKey, no keyboard hook). e.g. "F10", "Ctrl+Shift+S".
hotkey = {hotkey}
start_visible = {sv}
# Upper bound on overlay redraws per second. Nothing is redrawn while nothing changes.
refresh_hz = {rhz}
# Register the capture thread with MMCSS ("Games" task). Off by default (see README).
mmcss = {mmcss}
# Read audio every 20 ms instead of every 10 ms while it plays: half the wakeups,
# bars may appear up to 10 ms later.
power_saving = {ps}

[overlay]
# "edges", "edges_front_back" (sides + separate front/behind bars) or "circle".
style = {style}
# Circle style: ring radius in pixels, and opacity of the faint guide ring (0 = none).
circle_radius = {cr}
circle_guide_opacity = {cg:?}
# Bar thickness in pixels for the loudest (closest) sounds; quiet ones get min_thickness_frac of it.
thickness = {th}
min_thickness_frac = {mtf:?}
# Bar length in pixels for the loudest sounds; quiet ones get min_length_frac of it.
# (Circle style: arc length scales the same way, relative to the circle.)
bar_length = {bl}
min_length_frac = {mlf:?}
# Opacity (0..1) for the loudest sounds; quiet ones get min_opacity_frac of it.
opacity = {op:?}
min_opacity_frac = {mof:?}
color = {color}
# Faded bars below this intensity (0..1) are not drawn.
min_intensity = {mi:?}

[detection]
# Energy is measured over blocks of this many milliseconds.
block_ms = {bms:?}
# Band-pass (two biquads) to focus on footsteps / gunshots / reloads.
bandpass = {bp}
band_low_hz = {blo:?}
band_high_hz = {bhi:?}
# Band energy in dBFS mapped to intensity 0 and 1. Lower min_db = more sensitive.
min_db = {mindb:?}
max_db = {maxdb:?}
# A sound must be this many dB above the adaptive noise floor to show.
snr_db = {snr:?}
# Noise floor follows rising levels slowly and falling levels quickly (seconds).
floor_rise_s = {fr:?}
floor_fall_s = {ff:?}
# Bars fade out with this time constant (ms).
decay_ms = {dec:?}
# "Filter my own sounds": subtract the level shared by all channels.
subtract_common = {sc}
# Show a second simultaneous sound only if it has this fraction of the strongest one's energy.
cluster_min_frac = {cmf:?}
# On a 5.1/7.1 device, detect stereo-only content and show FL/FR as hard left/right.
auto_stereo = {ast}
# Hide your own gunshots (loud, front-heavy, balanced left/right). After each own shot,
# for own_shot_hold_ms only clearly directional side/rear sounds are shown.
ignore_own_gunshots = {iog}
own_shot_hold_ms = {osh:?}

[heuristics]
# Also hide centre-heavy / directionless sounds. Imperfect; disable if it hides enemies.
enabled = {he}
max_center_ratio = {mcr:?}
min_focus = {mf:?}

[speakers]
# Speaker angles in degrees from straight ahead; left speakers use the negative value.
front = {sf:?}
side = {ss:?}
back = {sb:?}
# Angle of the single surround pair in 5.1 / quad layouts.
surround_pair = {spp:?}
# Angle used for plain stereo L/R.
stereo = {sst:?}
"#,
            hotkey = q(&g.hotkey),
            sv = g.start_visible,
            rhz = g.refresh_hz,
            mmcss = g.mmcss,
            ps = g.power_saving,
            style = q(o.style.key()),
            cr = o.circle_radius,
            cg = o.circle_guide_opacity,
            th = o.thickness,
            mtf = o.min_thickness_frac,
            bl = o.bar_length,
            mlf = o.min_length_frac,
            op = o.opacity,
            mof = o.min_opacity_frac,
            color = q(&o.color),
            mi = o.min_intensity,
            bms = d.block_ms,
            bp = d.bandpass,
            blo = d.band_low_hz,
            bhi = d.band_high_hz,
            mindb = d.min_db,
            maxdb = d.max_db,
            snr = d.snr_db,
            fr = d.floor_rise_s,
            ff = d.floor_fall_s,
            dec = d.decay_ms,
            sc = d.subtract_common,
            cmf = d.cluster_min_frac,
            ast = d.auto_stereo,
            iog = d.ignore_own_gunshots,
            osh = d.own_shot_hold_ms,
            he = h.enabled,
            mcr = h.max_center_ratio,
            mf = h.min_focus,
            sf = sp.front,
            ss = sp.side,
            sb = sp.back,
            spp = sp.surround_pair,
            sst = sp.stereo,
        );
        s
    }
}

pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_round_trips() {
        assert_eq!(
            Config::parse(&Config::default().to_toml()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn modified_config_round_trips() {
        let mut c = Config::default();
        c.general.hotkey = "Ctrl+Shift+F9".into();
        c.overlay.style = OverlayStyle::Circle;
        c.overlay.circle_radius = 120;
        c.overlay.opacity = 0.55;
        c.overlay.color = "#00FF8C".into();
        c.detection.min_db = -72.5;
        c.detection.decay_ms = 400.0;
        c.detection.subtract_common = false;
        c.heuristics.enabled = false;
        assert_eq!(Config::parse(&c.to_toml()).unwrap(), c);
        for st in OverlayStyle::ALL {
            c.overlay.style = st;
            assert_eq!(Config::parse(&c.to_toml()).unwrap().overlay.style, st);
        }
    }

    #[test]
    fn old_files_without_style_still_load() {
        let c = Config::parse("[overlay]\nthickness = 8\n").unwrap();
        assert_eq!(c.overlay.style, OverlayStyle::Edges);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let c = Config::parse("[overlay]\nthickness = 10\n").unwrap();
        assert_eq!(c.overlay.thickness, 10);
        assert_eq!(c.general.hotkey, "F10");
    }

    #[test]
    fn unknown_key_is_an_error() {
        assert!(Config::parse("[overlay]\nthicknes = 10\n").is_err());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_hex_color("#00ff80"), Some((0, 255, 128)));
        assert_eq!(parse_hex_color("00ff80"), None);
        assert_eq!(parse_hex_color("#00ff8"), None);
    }

    #[test]
    fn sanitize_clamps() {
        let c =
            Config::parse("[general]\nrefresh_hz = 0\n[detection]\nmin_db = -10\nmax_db = -20\n")
                .unwrap();
        assert_eq!(c.general.refresh_hz, 1);
        assert!(c.detection.max_db > c.detection.min_db);
    }
}
