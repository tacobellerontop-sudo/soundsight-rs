//! Channel layout detection: WAVEFORMATEXTENSIBLE channel mask -> speaker positions/angles.
//!
//! Angle convention used everywhere in this crate: degrees, 0 = straight ahead,
//! positive = clockwise (to the player's right), range (-180, 180].

use crate::config::SpeakerAngles;

/// Maximum number of channels we analyse. Extra channels (e.g. height channels
/// of a 7.1.4 mix) are ignored. Keeps every per-channel buffer a fixed-size array.
pub const MAX_CHANNELS: usize = 8;

/// Speaker positions as defined by the WAVEFORMATEXTENSIBLE `dwChannelMask` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    BackLeft,
    BackRight,
    FrontLeftOfCenter,
    FrontRightOfCenter,
    BackCenter,
    SideLeft,
    SideRight,
    /// Height or otherwise unsupported position; ignored for direction.
    Other,
}

impl Speaker {
    pub fn label(self) -> &'static str {
        match self {
            Speaker::FrontLeft => "FL",
            Speaker::FrontRight => "FR",
            Speaker::FrontCenter => "C",
            Speaker::Lfe => "LFE",
            Speaker::BackLeft => "BL",
            Speaker::BackRight => "BR",
            Speaker::FrontLeftOfCenter => "FLC",
            Speaker::FrontRightOfCenter => "FRC",
            Speaker::BackCenter => "BC",
            Speaker::SideLeft => "SL",
            Speaker::SideRight => "SR",
            Speaker::Other => "?",
        }
    }

    fn from_mask_bit(bit: u32) -> Speaker {
        match bit {
            0 => Speaker::FrontLeft,
            1 => Speaker::FrontRight,
            2 => Speaker::FrontCenter,
            3 => Speaker::Lfe,
            4 => Speaker::BackLeft,
            5 => Speaker::BackRight,
            6 => Speaker::FrontLeftOfCenter,
            7 => Speaker::FrontRightOfCenter,
            8 => Speaker::BackCenter,
            9 => Speaker::SideLeft,
            10 => Speaker::SideRight,
            _ => Speaker::Other,
        }
    }
}

/// A resolved layout: which speaker each interleaved channel feeds, and the
/// unit vector (x = right, y = front) used for the direction vector sum.
#[derive(Clone, Debug)]
pub struct Layout {
    pub channels: usize,
    pub mask: u32,
    pub speakers: [Speaker; MAX_CHANNELS],
    /// Unit vectors per channel; (0,0) for channels that carry no direction (LFE, unknown).
    pub vectors: [(f32, f32); MAX_CHANNELS],
    /// True for the channel(s) treated as "center" by the center-heavy heuristic.
    pub is_center: [bool; MAX_CHANNELS],
    pub name: &'static str,
}

/// Default channel mask Windows uses when a format has no explicit mask.
pub fn default_mask(channels: usize) -> u32 {
    match channels {
        1 => 0x4,   // FC
        2 => 0x3,   // FL FR
        4 => 0x33,  // FL FR BL BR (quad)
        6 => 0x60F, // FL FR FC LFE SL SR (5.1 "surround", Windows default)
        8 => 0x63F, // FL FR FC LFE BL BR SL SR (7.1)
        n => (1u32 << n.min(31)) - 1,
    }
}

impl Layout {
    /// Build a layout from a channel count and WAVEFORMATEXTENSIBLE channel mask
    /// (pass 0 if the format had no mask).
    pub fn from_mask(channels: usize, mask: u32, angles: &SpeakerAngles) -> Layout {
        let mask = if mask == 0 { default_mask(channels) } else { mask };
        let mut speakers = [Speaker::Other; MAX_CHANNELS];

        // Channels are interleaved in ascending mask-bit order.
        let mut ch = 0usize;
        for bit in 0..32u32 {
            if ch >= channels.min(MAX_CHANNELS) {
                break;
            }
            if mask & (1 << bit) != 0 {
                speakers[ch] = Speaker::from_mask_bit(bit);
                ch += 1;
            }
        }

        let has = |s: Speaker| speakers[..channels.min(MAX_CHANNELS)].contains(&s);
        let has_side = has(Speaker::SideLeft) || has(Speaker::SideRight);
        let has_back = has(Speaker::BackLeft) || has(Speaker::BackRight);
        let stereo_only = channels == 2
            && speakers[0] == Speaker::FrontLeft
            && speakers[1] == Speaker::FrontRight;
        // With only ONE surround pair (5.1 / quad) that pair sits behind the listener's
        // ears (ITU 5.1: +-110 deg). Using +-90 would make a sound directly behind
        // cancel out to a zero vector.
        let single_surround_pair = has_side != has_back;

        let mut vectors = [(0.0f32, 0.0f32); MAX_CHANNELS];
        let mut is_center = [false; MAX_CHANNELS];
        for i in 0..channels.min(MAX_CHANNELS) {
            let s = speakers[i];
            let deg = match s {
                Speaker::FrontLeft if stereo_only => Some(-angles.stereo),
                Speaker::FrontRight if stereo_only => Some(angles.stereo),
                Speaker::FrontLeft => Some(-angles.front),
                Speaker::FrontRight => Some(angles.front),
                Speaker::FrontCenter => Some(0.0),
                Speaker::FrontLeftOfCenter => Some(-angles.front / 2.0),
                Speaker::FrontRightOfCenter => Some(angles.front / 2.0),
                Speaker::SideLeft if single_surround_pair => Some(-angles.surround_pair),
                Speaker::SideRight if single_surround_pair => Some(angles.surround_pair),
                Speaker::BackLeft if single_surround_pair => Some(-angles.surround_pair),
                Speaker::BackRight if single_surround_pair => Some(angles.surround_pair),
                Speaker::SideLeft => Some(-angles.side),
                Speaker::SideRight => Some(angles.side),
                Speaker::BackLeft => Some(-angles.back),
                Speaker::BackRight => Some(angles.back),
                Speaker::BackCenter => Some(180.0),
                Speaker::Lfe | Speaker::Other => None,
            };
            if let Some(d) = deg {
                let r = d.to_radians();
                vectors[i] = (r.sin(), r.cos());
            }
            is_center[i] = s == Speaker::FrontCenter;
        }

        let name = match (channels, mask) {
            (1, _) => "mono",
            (2, 0x3) => "stereo",
            (4, 0x33) | (4, 0x603) => "quad",
            (6, 0x3F) | (6, 0x60F) => "5.1",
            (8, 0x63F) => "7.1",
            (8, 0xFF) => "7.1 (wide)",
            _ => "custom",
        };

        Layout { channels, mask, speakers, vectors, is_center, name }
    }

    /// Number of channels that are actually analysed.
    pub fn analysed_channels(&self) -> usize {
        self.channels.min(MAX_CHANNELS)
    }

    /// True if the layout can tell front from back at all.
    pub fn has_rear(&self) -> bool {
        self.vectors[..self.analysed_channels()].iter().any(|v| v.1 < -0.01)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn angles() -> SpeakerAngles {
        SpeakerAngles::default()
    }

    #[test]
    fn seven_one_order() {
        let l = Layout::from_mask(8, 0x63F, &angles());
        let labels: Vec<_> = l.speakers.iter().map(|s| s.label()).collect();
        assert_eq!(labels, ["FL", "FR", "C", "LFE", "BL", "BR", "SL", "SR"]);
        assert_eq!(l.name, "7.1");
        assert_eq!(l.vectors[3], (0.0, 0.0), "LFE must carry no direction");
        assert!(l.is_center[2]);
        assert!(l.has_rear());
    }

    #[test]
    fn zero_mask_uses_default() {
        let l = Layout::from_mask(6, 0, &angles());
        assert_eq!(l.mask, 0x60F);
        assert_eq!(l.name, "5.1");
    }

    #[test]
    fn stereo_has_no_rear() {
        let l = Layout::from_mask(2, 0x3, &angles());
        assert!(!l.has_rear());
        // Stereo L is mapped to -stereo (default -90) so hard-left shows on the left edge.
        assert!((l.vectors[0].0 + 1.0).abs() < 1e-6);
    }

    #[test]
    fn more_than_eight_channels_is_truncated() {
        // 7.1.4: 7.1 + four top speakers (bits 12, 14, 15, 17).
        let l = Layout::from_mask(12, 0x63F | (1 << 12) | (1 << 14) | (1 << 15) | (1 << 17), &angles());
        assert_eq!(l.analysed_channels(), 8);
        assert_eq!(l.speakers[7], Speaker::SideRight);
    }
}
