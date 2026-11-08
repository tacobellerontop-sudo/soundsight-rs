//! Configuration, loaded from `soundsight.toml` next to the executable.
//! Every field has a default, so a partial (or missing) file is fine.

use serde::Deserialize;

/// Template written next to the exe on first run. A unit test checks that it
/// parses to exactly `Config::default()`, so the two cannot drift apart.
pub const DEFAULT_TOML: &str = include_str!("../soundsight.default.toml");

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
}

impl Default for General {
    fn default() -> Self {
        General { hotkey: "F10".into(), start_visible: true, refresh_hz: 60, mmcss: false }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    /// Bar thickness in physical pixels (also the thickness of the edge windows).
    pub thickness: u32,
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
            thickness: 6,
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
        Heuristics { enabled: true, max_center_ratio: 0.6, min_focus: 0.15 }
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
        SpeakerAngles { front: 30.0, side: 90.0, back: 150.0, surround_pair: 110.0, stereo: 90.0 }
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
        o.bar_length = o.bar_length.clamp(4, 4000);
        o.min_length_frac = o.min_length_frac.clamp(0.0, 1.0);
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
        let h = &mut self.heuristics;
        h.max_center_ratio = h.max_center_ratio.clamp(0.0, 1.0);
        h.min_focus = h.min_focus.clamp(0.0, 1.0);
    }

    /// Parse `overlay.color`; falls back to red on malformed input.
    pub fn color_rgb(&self) -> (u8, u8, u8) {
        parse_hex_color(&self.overlay.color).unwrap_or((0xFF, 0x3B, 0x30))
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
    fn template_matches_defaults() {
        assert_eq!(Config::parse(DEFAULT_TOML).unwrap(), Config::default());
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
        let c = Config::parse("[general]\nrefresh_hz = 0\n[detection]\nmin_db = -10\nmax_db = -20\n").unwrap();
        assert_eq!(c.general.refresh_hz, 1);
        assert!(c.detection.max_db > c.detection.min_db);
    }
}
