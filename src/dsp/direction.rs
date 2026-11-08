//! Direction math: per-channel energies -> direction angle via an energy-weighted
//! vector sum of the speaker unit vectors (a Gerzon "energy vector").
//!
//! Why energy (squared amplitude) weights: game engines pan with constant-power
//! laws, so a source's energy is split between neighbouring speakers in proportion
//! to how close it is to each. The energy vector recovers the panning direction
//! for pairwise-panned sources exactly at the speakers and approximately between them.

use super::layout::MAX_CHANNELS;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionEstimate {
    /// Degrees, 0 = front, positive = clockwise/right, range (-180, 180].
    pub angle_deg: f32,
    /// Length of the energy vector divided by total energy, 0..1.
    /// 1 = all energy from one direction; ~0 = energy evenly spread (diffuse or
    /// centre-of-head, e.g. equal L/R on a stereo layout).
    pub focus: f32,
    /// Sum of the energies that carry direction (LFE/unknown excluded).
    pub total: f32,
}

/// Energy-weighted vector sum. `vectors[i]` is the unit vector (x = right, y = front)
/// of channel `i`; (0,0) for channels without direction, which are excluded.
/// Returns `None` when there is no directional energy.
pub fn estimate(energies: &[f32], vectors: &[(f32, f32)]) -> Option<DirectionEstimate> {
    let n = energies.len().min(vectors.len()).min(MAX_CHANNELS);
    let (mut x, mut y, mut total) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        let (vx, vy) = vectors[i];
        if vx == 0.0 && vy == 0.0 {
            continue;
        }
        let e = energies[i].max(0.0);
        x += e * vx;
        y += e * vy;
        total += e;
    }
    if total <= f32::MIN_POSITIVE {
        return None;
    }
    let len = (x * x + y * y).sqrt();
    // atan2(x, y): measured from the +y (front) axis towards +x (right) = clockwise.
    let angle = if len <= total * 1e-6 {
        0.0
    } else {
        normalize_deg(x.atan2(y).to_degrees())
    };
    Some(DirectionEstimate {
        angle_deg: angle,
        focus: (len / total).min(1.0),
        total,
    })
}

/// Wrap an angle into (-180, 180].
pub fn normalize_deg(mut a: f32) -> f32 {
    a %= 360.0;
    if a > 180.0 {
        a -= 360.0;
    } else if a <= -180.0 {
        a += 360.0;
    }
    a
}

/// Centre angle of radar bin `i` out of `bins` (bin 0 is centred on 0 deg = front).
pub fn bin_center_deg(i: usize, bins: usize) -> f32 {
    normalize_deg(i as f32 * 360.0 / bins as f32)
}

/// Split an angle between its two nearest bins: returns (lower bin, upper bin, weight of upper).
pub fn angle_to_bins(angle_deg: f32, bins: usize) -> (usize, usize, f32) {
    let pos = (angle_deg.rem_euclid(360.0)) * bins as f32 / 360.0;
    let lo = (pos.floor() as usize) % bins;
    let hi = (lo + 1) % bins;
    (lo, hi, pos - pos.floor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SpeakerAngles;
    use crate::dsp::layout::Layout;

    fn l71() -> Layout {
        Layout::from_mask(8, 0x63F, &SpeakerAngles::default())
    }

    /// 7.1 channel order: FL FR C LFE BL BR SL SR
    fn est71(e: [f32; 8]) -> DirectionEstimate {
        estimate(&e, &l71().vectors).expect("has energy")
    }

    fn close(a: f32, b: f32) -> bool {
        let d = normalize_deg(a - b).abs();
        d < 0.5
    }

    #[test]
    fn single_speakers_give_their_angle() {
        let cases = [
            (0, -30.0),
            (1, 30.0),
            (2, 0.0),
            (4, -150.0),
            (5, 150.0),
            (6, -90.0),
            (7, 90.0),
        ];
        for (ch, want) in cases {
            let mut e = [0.0; 8];
            e[ch] = 1.0;
            let d = est71(e);
            assert!(
                close(d.angle_deg, want),
                "channel {ch}: got {} want {want}",
                d.angle_deg
            );
            assert!((d.focus - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn pairs_give_midpoints() {
        // FL+FR -> front
        assert!(close(
            est71([1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]).angle_deg,
            0.0
        ));
        // FR+SR -> 60
        assert!(close(
            est71([0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0]).angle_deg,
            60.0
        ));
        // BL+BR -> directly behind
        assert!(close(
            est71([0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0]).angle_deg,
            180.0
        ));
        // SL+BL -> -120
        assert!(close(
            est71([0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0]).angle_deg,
            -120.0
        ));
    }

    #[test]
    fn unequal_pair_leans_toward_louder() {
        let d = est71([0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        assert!(d.angle_deg > 30.0 && d.angle_deg < 60.0, "{}", d.angle_deg);
    }

    #[test]
    fn lfe_is_ignored() {
        let d = est71([0.0, 1.0, 0.0, 50.0, 0.0, 0.0, 0.0, 0.0]);
        assert!(close(d.angle_deg, 30.0));
        assert!((d.total - 1.0).abs() < 1e-6);
    }

    #[test]
    fn silence_gives_none() {
        assert!(estimate(&[0.0; 8], &l71().vectors).is_none());
    }

    #[test]
    fn diffuse_has_low_focus() {
        let d = est71([1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        assert!(d.focus < 0.2, "focus {}", d.focus);
    }

    #[test]
    fn stereo_left_right_only() {
        let l = Layout::from_mask(2, 0x3, &SpeakerAngles::default());
        let left = estimate(&[1.0, 0.0], &l.vectors).unwrap();
        let right = estimate(&[0.0, 1.0], &l.vectors).unwrap();
        let mid = estimate(&[1.0, 1.0], &l.vectors).unwrap();
        assert!(close(left.angle_deg, -90.0));
        assert!(close(right.angle_deg, 90.0));
        assert!(close(mid.angle_deg, 0.0));
        assert!(mid.focus < 1e-3, "centre-panned stereo has no direction");
    }

    #[test]
    fn five_one_rear_center_points_back() {
        // 5.1 side layout: FL FR C LFE SL SR, surround pair at +-110.
        let l = Layout::from_mask(6, 0x60F, &SpeakerAngles::default());
        let d = estimate(&[0.0, 0.0, 0.0, 0.0, 1.0, 1.0], &l.vectors).unwrap();
        assert!(close(d.angle_deg, 180.0), "{}", d.angle_deg);
        let sl = estimate(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], &l.vectors).unwrap();
        assert!(close(sl.angle_deg, -110.0));
    }

    #[test]
    fn bins() {
        assert_eq!(angle_to_bins(0.0, 36), (0, 1, 0.0));
        let (lo, hi, w) = angle_to_bins(-5.0, 36);
        assert_eq!((lo, hi), (35, 0));
        assert!((w - 0.5).abs() < 1e-4);
        assert!(close(bin_center_deg(18, 36), 180.0));
        assert!(close(bin_center_deg(27, 36), -90.0));
    }

    #[test]
    fn normalize() {
        assert_eq!(normalize_deg(190.0), -170.0);
        assert_eq!(normalize_deg(-180.0), 180.0);
        assert_eq!(normalize_deg(540.0), 180.0);
    }
}
