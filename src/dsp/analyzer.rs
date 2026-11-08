//! Block analyser: interleaved samples in, radar bins out.
//!
//! Pipeline per sample:   x -> [HP 200 Hz] -> [LP 4 kHz] -> accumulate x^2 per channel
//! Pipeline per block (~10 ms):
//!   1. mean-square energy per channel (band-passed; also full-band for the meter)
//!   2. subtract a per-channel adaptive noise floor (slow rise / fast fall), so
//!      constant sounds (music, ambience, wind) fade out of the picture
//!   3. reject the block unless it is `snr_db` above the floor
//!   4. common-mode removal: subtract the quietest active channel from all of them.
//!      Your own footsteps/gunfire play in every channel at about the same level; an
//!      enemy adds energy to one or two channels on top. What remains is the enemy.
//!   5. group adjacent speakers into sources (strongest channel + its two
//!      neighbours), so two sounds from different sides show as two bars
//!   6. energy-vector direction + loudness (dBFS -> 0..1 intensity) per source
//!   7. optional heuristics (centre-heavy / unfocused -> suppress)
//!   8. deposit into a circular histogram of `BINS` directions; bins decay exponentially
//!
//! Own-gunshot gate: your own gun is not placed in 3D. It plays mostly in the front
//! speakers (or both stereo channels), roughly balanced left/right, and is the loudest
//! regular sound. Gun samples have stereo width, so block by block it is slightly
//! louder on one side, then the other, which would make bars flicker ("burrr"). A block
//! that is near the recent loudness peak, front-heavy and balanced starts a short hold
//! (renewed on every shot) during which only clearly directional, non-frontal sounds
//! are shown.
//!
//! On surround devices it also notices when only FL/FR ever carry sound (a stereo
//! game played through a 7.1 device) and then maps FL/FR to hard left/right.
//!
//! No heap allocation: every buffer is a fixed-size array inside the struct.

use super::biquad::Biquad;
use super::direction::{angle_to_bins, estimate, DirectionEstimate};
use super::layout::{Layout, Speaker, MAX_CHANNELS};
use crate::config::Config;

/// Number of radar directions (10 degrees each).
pub const BINS: usize = 36;

/// Frames processed per inner-loop run. Formats that need repacking use a stack
/// buffer of `CHUNK * MAX_CHANNELS` floats (8 KB), never the heap.
const CHUNK: usize = 256;

/// The band-pass for all channels at once: one coefficient set (every channel uses
/// the same filter), per-channel state in fixed lanes so the compiler can keep a whole
/// frame in SIMD registers (2 lanes for stereo, 6 for 5.1, 8 for 7.1).
#[derive(Clone, Copy)]
struct FilterBank {
    hp: [f32; 5],
    lp: [f32; 5],
    hz1: [f32; MAX_CHANNELS],
    hz2: [f32; MAX_CHANNELS],
    lz1: [f32; MAX_CHANNELS],
    lz2: [f32; MAX_CHANNELS],
}

impl FilterBank {
    fn new(sr: f32, low: f32, high: f32) -> FilterBank {
        FilterBank {
            hp: Biquad::highpass(sr, low).coefs(),
            lp: Biquad::lowpass(sr, high).coefs(),
            hz1: [0.0; MAX_CHANNELS],
            hz2: [0.0; MAX_CHANNELS],
            lz1: [0.0; MAX_CHANNELS],
            lz2: [0.0; MAX_CHANNELS],
        }
    }

    fn reset(&mut self) {
        self.hz1 = [0.0; MAX_CHANNELS];
        self.hz2 = [0.0; MAX_CHANNELS];
        self.lz1 = [0.0; MAX_CHANNELS];
        self.lz2 = [0.0; MAX_CHANNELS];
    }

    /// Zero tiny states so silence never decays into (slow) denormal floats.
    fn flush_denormals(&mut self) {
        for z in [&mut self.hz1, &mut self.hz2, &mut self.lz1, &mut self.lz2] {
            for v in z.iter_mut() {
                if v.abs() < 1e-15 {
                    *v = 0.0;
                }
            }
        }
    }
}

/// The hot loop. `data` holds whole frames of exactly `L` interleaved channels.
/// Accumulates band-passed (`BP`) energy and, if `MET`, full-band energy per lane.
#[inline(always)]
fn kernel<const L: usize, const BP: bool, const MET: bool>(
    bank: &mut FilterBank,
    acc_band: &mut [f32; MAX_CHANNELS],
    acc_full: &mut [f32; MAX_CHANNELS],
    data: &[f32],
) {
    let [hb0, hb1, hb2, ha1, ha2] = bank.hp;
    let [lb0, lb1, lb2, la1, la2] = bank.lp;
    // Work on local copies so the state stays in registers for the whole run.
    let mut hz1: [f32; L] = std::array::from_fn(|c| bank.hz1[c]);
    let mut hz2: [f32; L] = std::array::from_fn(|c| bank.hz2[c]);
    let mut lz1: [f32; L] = std::array::from_fn(|c| bank.lz1[c]);
    let mut lz2: [f32; L] = std::array::from_fn(|c| bank.lz2[c]);
    let mut ab = [0.0f32; L];
    let mut af = [0.0f32; L];
    for frame in data.chunks_exact(L) {
        for c in 0..L {
            let x = frame[c];
            if MET {
                af[c] += x * x;
            }
            let y = if BP {
                // Two transposed direct form II biquads: 200 Hz HP then 4 kHz LP.
                let y1 = hb0 * x + hz1[c];
                hz1[c] = hb1 * x - ha1 * y1 + hz2[c];
                hz2[c] = hb2 * x - ha2 * y1;
                let y2 = lb0 * y1 + lz1[c];
                lz1[c] = lb1 * y1 - la1 * y2 + lz2[c];
                lz2[c] = lb2 * y1 - la2 * y2;
                y2
            } else {
                x
            };
            ab[c] += y * y;
        }
    }
    bank.hz1[..L].copy_from_slice(&hz1);
    bank.hz2[..L].copy_from_slice(&hz2);
    bank.lz1[..L].copy_from_slice(&lz1);
    bank.lz2[..L].copy_from_slice(&lz2);
    for c in 0..L {
        acc_band[c] += ab[c];
        acc_full[c] += af[c];
    }
}

/// Bins/meters below this are treated as zero (also stops endless tiny decays).
const ZERO: f32 = 1.0 / 512.0;

/// Interleaved sample formats we accept from WASAPI.
pub trait Sample: Copy {
    fn to_f32(self) -> f32;
    /// Zero-copy view for the common case (shared-mode mix format is f32).
    fn as_f32(_s: &[Self]) -> Option<&[f32]> {
        None
    }
}
impl Sample for f32 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self
    }
    fn as_f32(s: &[f32]) -> Option<&[f32]> {
        Some(s)
    }
}
impl Sample for i16 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32 * (1.0 / 32768.0)
    }
}
impl Sample for i32 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self as f32 * (1.0 / 2_147_483_648.0)
    }
}

/// What the last accepted block looked like (for tests and debugging).
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub dir: DirectionEstimate,
    pub intensity: f32,
}

pub struct Analyzer {
    layout: Layout,
    stride: usize,
    n: usize,

    bandpass: bool,
    bank: FilterBank,
    /// Compute the full-band per-channel meters (only needed by the speaker test).
    meters_on: bool,
    /// Bins fading below this are dropped (they are not drawn anyway), so fades end
    /// sooner and invisible changes never trigger a redraw.
    min_intensity: f32,

    acc_band: [f32; MAX_CHANNELS],
    acc_full: [f32; MAX_CHANNELS],
    count: usize,
    block_frames: usize,
    block_dt: f32,

    floor: [f32; MAX_CHANNELS],
    floor_rise_a: f32,
    floor_fall_a: f32,
    snr_lin: f32,
    min_db: f32,
    max_db: f32,

    heur_enabled: bool,
    max_center_ratio: f32,
    min_focus: f32,

    subtract_common: bool,
    cluster_min_frac: f32,
    /// Active channels sorted by angle (a ring), for finding neighbours. Two rings:
    /// the real layout and the stereo fallback (FL/FR only).
    ring: [usize; MAX_CHANNELS],
    ring_len: usize,
    stereo_ring: [usize; MAX_CHANNELS],
    stereo_ring_len: usize,
    stereo_vectors: [(f32, f32); MAX_CHANNELS],

    /// Own-gunshot gate.
    own_filter: bool,
    fl: Option<usize>,
    fr: Option<usize>,
    center: Option<usize>,
    loud_ref_db: f32,
    loud_ref_floor_db: f32,
    loud_decay_db: f32,
    own_hold_blocks: u32,
    own_hold_left: u32,

    /// Stereo-content-on-surround-device detection.
    auto_stereo: bool,
    stereo_content: bool,
    front_avg: f32,
    rest_avg: f32,
    active_time: f32,
    mode_a: f32,

    decay_tau_s: f32,
    decay_per_block: f32,
    bins: [f32; BINS],

    /// Full-band mean square per channel with peak-hold style release, for the speaker-test meter.
    meters: [f32; MAX_CHANNELS],

    /// Strongest source found in the last block (for tests and debugging).
    pub last_event: Option<Event>,
    /// Number of sources found in the last block.
    pub last_event_count: usize,
}

/// Own-gunshot gate: a block within this many dB of the recent loudness peak counts as "loud".
const OWN_BELOW_PEAK_DB: f32 = 12.0;
/// ... and at least this share of its energy must be in FL+FR+C ...
const OWN_MIN_FRONTNESS: f32 = 0.6;
/// ... and |FL-FR| / (FL+FR) must be at most this (roughly balanced left/right).
const OWN_MAX_BALANCE: f32 = 0.5;
/// The loudness peak tracker falls by this many dB per second ...
const OWN_PEAK_DECAY_DB_PER_S: f32 = 0.5;
/// ... but never below `max_db - this`.
const OWN_PEAK_FLOOR_BELOW_MAX_DB: f32 = 12.0;
/// While the gate holds, a sound is shown only if it carries this share of the block's
/// energy and comes from outside +-this angle around straight ahead.
const OWN_HOLD_MIN_SHARE: f32 = 0.5;
const OWN_HOLD_MIN_ANGLE: f32 = 60.0;

/// Time constant for the stereo-content detector's averages.
const MODE_TAU_S: f32 = 2.0;
/// Seconds of active (non-silent) audio needed before switching to stereo mapping.
const MODE_MIN_ACTIVE_S: f32 = 1.5;

/// Channel indices sorted clockwise by angle; channels without direction are left out.
fn build_ring(vectors: &[(f32, f32)], n: usize) -> ([usize; MAX_CHANNELS], usize) {
    let mut ring = [0usize; MAX_CHANNELS];
    let mut len = 0;
    for c in 0..n {
        if vectors[c] != (0.0, 0.0) {
            ring[len] = c;
            len += 1;
        }
    }
    let angle = |c: usize| vectors[c].0.atan2(vectors[c].1);
    ring[..len].sort_unstable_by(|&a, &b| angle(a).total_cmp(&angle(b)));
    (ring, len)
}

impl Analyzer {
    pub fn new(layout: Layout, sample_rate: u32, cfg: &Config) -> Analyzer {
        let d = &cfg.detection;
        let sr = sample_rate.max(1) as f32;
        let block_frames = ((sr * d.block_ms / 1000.0).round() as usize).max(16);
        let block_dt = block_frames as f32 / sr;
        let coef = |tau: f32| 1.0 - (-block_dt / tau).exp();
        let floor0 = db_to_energy(d.min_db);
        let tau = d.decay_ms / 1000.0;
        let n = layout.analysed_channels();
        let (ring, ring_len) = build_ring(&layout.vectors, n);
        // Stereo fallback: FL hard left, FR hard right, everything else ignored.
        let mut stereo_vectors = [(0.0f32, 0.0f32); MAX_CHANNELS];
        let fl = layout.speakers[..n]
            .iter()
            .position(|&s| s == Speaker::FrontLeft);
        let fr = layout.speakers[..n]
            .iter()
            .position(|&s| s == Speaker::FrontRight);
        if let (Some(l), Some(r)) = (fl, fr) {
            stereo_vectors[l] = (-1.0, 0.0);
            stereo_vectors[r] = (1.0, 0.0);
        }
        let (stereo_ring, stereo_ring_len) = build_ring(&stereo_vectors, n);
        let center = layout.speakers[..n]
            .iter()
            .position(|&s| s == Speaker::FrontCenter);
        // Only meaningful when the layout has more than the front pair.
        let auto_stereo = d.auto_stereo && fl.is_some() && fr.is_some() && ring_len > 2;
        Analyzer {
            stride: layout.channels.max(1),
            n: layout.analysed_channels(),
            bandpass: d.bandpass,
            bank: FilterBank::new(sr, d.band_low_hz, d.band_high_hz),
            meters_on: true,
            min_intensity: cfg.overlay.min_intensity.max(ZERO),
            acc_band: [0.0; MAX_CHANNELS],
            acc_full: [0.0; MAX_CHANNELS],
            count: 0,
            block_frames,
            block_dt,
            floor: [floor0; MAX_CHANNELS],
            floor_rise_a: coef(d.floor_rise_s),
            floor_fall_a: coef(d.floor_fall_s),
            snr_lin: 10f32.powf(d.snr_db / 10.0),
            min_db: d.min_db,
            max_db: d.max_db,
            heur_enabled: cfg.heuristics.enabled,
            max_center_ratio: cfg.heuristics.max_center_ratio,
            min_focus: cfg.heuristics.min_focus,
            subtract_common: d.subtract_common,
            cluster_min_frac: d.cluster_min_frac,
            ring,
            ring_len,
            stereo_ring,
            stereo_ring_len,
            stereo_vectors,
            own_filter: d.ignore_own_gunshots,
            fl,
            fr,
            center,
            loud_ref_db: d.max_db,
            loud_ref_floor_db: d.max_db - OWN_PEAK_FLOOR_BELOW_MAX_DB,
            loud_decay_db: OWN_PEAK_DECAY_DB_PER_S * block_dt,
            own_hold_blocks: (d.own_shot_hold_ms / 1000.0 / block_dt).ceil() as u32,
            own_hold_left: 0,
            auto_stereo,
            stereo_content: false,
            front_avg: 0.0,
            rest_avg: 0.0,
            active_time: 0.0,
            mode_a: 1.0 - (-block_dt / MODE_TAU_S).exp(),
            decay_tau_s: tau,
            decay_per_block: (-block_dt / tau).exp(),
            bins: [0.0; BINS],
            meters: [0.0; MAX_CHANNELS],
            last_event: None,
            last_event_count: 0,
            layout,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn block_seconds(&self) -> f32 {
        self.block_dt
    }

    /// True while a surround device is carrying stereo-only content and FL/FR are
    /// being mapped to hard left/right.
    pub fn stereo_content(&self) -> bool {
        self.stereo_content
    }

    /// True while the own-gunshot gate is holding (shown in the speaker test).
    pub fn own_shot_active(&self) -> bool {
        self.own_hold_left > 0
    }

    /// Turn the speaker-test meters on/off (saves work while Settings is closed).
    pub fn set_meters(&mut self, on: bool) {
        if self.meters_on != on {
            self.meters_on = on;
            self.meters = [0.0; MAX_CHANNELS];
            self.acc_full = [0.0; MAX_CHANNELS];
        }
    }

    /// Feed interleaved frames. `data.len()` should be a multiple of the channel count;
    /// a trailing partial frame is ignored. Returns the number of blocks completed.
    pub fn process<S: Sample>(&mut self, data: &[S]) -> usize {
        let stride = self.stride;
        let frames = data.len() / stride;
        let (mut done, mut blocks) = (0, 0);
        while done < frames {
            let run = (frames - done)
                .min(self.block_frames - self.count)
                .min(CHUNK);
            let src = &data[done * stride..(done + run) * stride];
            match (S::as_f32(src), stride) {
                (Some(f), 1) => self.run_lanes::<1>(f),
                (Some(f), 2) => self.run_lanes::<2>(f),
                (Some(f), 4) => self.run_lanes::<4>(f),
                (Some(f), 6) => self.run_lanes::<6>(f),
                (Some(f), 8) => self.run_lanes::<8>(f),
                _ => {
                    // More than 8 channels or integer samples: repack into 8 lanes.
                    let mut buf = [0.0f32; CHUNK * MAX_CHANNELS];
                    let n = self.n;
                    for (dst, frame) in buf
                        .chunks_exact_mut(MAX_CHANNELS)
                        .zip(src.chunks_exact(stride))
                    {
                        for c in 0..n {
                            dst[c] = frame[c].to_f32();
                        }
                    }
                    self.run_lanes::<8>(&buf[..run * MAX_CHANNELS]);
                }
            }
            self.count += run;
            done += run;
            if self.count >= self.block_frames {
                self.finish_block();
                blocks += 1;
            }
        }
        blocks
    }

    fn run_lanes<const L: usize>(&mut self, data: &[f32]) {
        let (b, a, f) = (&mut self.bank, &mut self.acc_band, &mut self.acc_full);
        match (self.bandpass, self.meters_on) {
            (true, false) => kernel::<L, true, false>(b, a, f, data),
            (true, true) => kernel::<L, true, true>(b, a, f, data),
            (false, false) => kernel::<L, false, false>(b, a, f, data),
            (false, true) => kernel::<L, false, true>(b, a, f, data),
        }
    }

    /// Account for `frames` frames of digital silence (WASAPI AUDCLNT_BUFFERFLAGS_SILENT)
    /// without touching the sample data.
    pub fn process_silence(&mut self, mut frames: usize) -> usize {
        let mut blocks = 0;
        self.bank.reset();
        while frames > 0 {
            let take = frames.min(self.block_frames - self.count);
            self.count += take;
            frames -= take;
            if self.count >= self.block_frames {
                self.finish_block();
                blocks += 1;
            }
        }
        blocks
    }

    fn finish_block(&mut self) {
        let n = self.n;
        let inv = 1.0 / self.count.max(1) as f32;
        self.count = 0;

        let mut energy = [0.0f32; MAX_CHANNELS];
        let mut eff = [0.0f32; MAX_CHANNELS];
        let (mut total, mut floor_total) = (0.0f32, 0.0f32);
        let meter_release = self.decay_per_block;

        for c in 0..n {
            let e = self.acc_band[c] * inv;
            let full = self.acc_full[c] * inv;
            self.acc_band[c] = 0.0;
            self.acc_full[c] = 0.0;
            if self.meters_on {
                self.meters[c] = full.max(self.meters[c] * meter_release);
            }
            energy[c] = e;

            let floor = self.floor[c];
            eff[c] = (e - floor).max(0.0);
            let a = if e > floor {
                self.floor_rise_a
            } else {
                self.floor_fall_a
            };
            self.floor[c] = floor + a * (e - floor);

            let directional = self.layout.vectors[c] != (0.0, 0.0);
            if directional {
                total += e;
                floor_total += floor;
            }
        }

        self.bank.flush_denormals();
        self.decay_bins(self.decay_per_block);
        self.last_event = None;
        self.last_event_count = 0;

        self.own_hold_left = self.own_hold_left.saturating_sub(1);
        self.loud_ref_db = (self.loud_ref_db - self.loud_decay_db).max(self.loud_ref_floor_db);

        let active = total > floor_total * self.snr_lin && total > f32::MIN_POSITIVE;
        if self.auto_stereo && active {
            self.update_stereo_detector(&energy);
        }
        if !active {
            return;
        }
        if self.own_filter && self.is_own_shot(&eff) {
            self.own_hold_left = self.own_hold_blocks.max(1);
        }
        let holding = self.own_hold_left > 0;

        let (vectors, ring, ring_len) = if self.stereo_content {
            (self.stereo_vectors, self.stereo_ring, self.stereo_ring_len)
        } else {
            (self.layout.vectors, self.ring, self.ring_len)
        };
        let ring = &ring[..ring_len];
        if ring.is_empty() {
            return;
        }

        // Common-mode removal ("filter my own sounds").
        let mut resid = [0.0f32; MAX_CHANNELS];
        let common = if self.subtract_common {
            ring.iter().map(|&c| eff[c]).fold(f32::INFINITY, f32::min)
        } else {
            0.0
        };
        let mut peak = 0.0f32;
        let eff_total: f32 = ring.iter().map(|&c| eff[c]).sum();
        for &c in ring {
            resid[c] = (eff[c] - common).max(0.0);
            peak = peak.max(resid[c]);
        }
        if peak <= 0.0 {
            return;
        }

        // Greedy source grouping: strongest remaining channel + its ring neighbours.
        let mut used = [false; MAX_CHANNELS];
        for _ in 0..ring_len {
            let mut best = None;
            for (i, &c) in ring.iter().enumerate() {
                if !used[c]
                    && resid[c] > 0.0
                    && best.is_none_or(|(_, b): (usize, usize)| resid[c] > resid[b])
                {
                    best = Some((i, c));
                }
            }
            let Some((i, c)) = best else { break };
            if resid[c] < peak * self.cluster_min_frac {
                break;
            }
            let mut members = [c, usize::MAX, usize::MAX];
            used[c] = true;
            if ring_len > 1 {
                let left = ring[(i + ring_len - 1) % ring_len];
                let right = ring[(i + 1) % ring_len];
                for (k, nb) in [left, right].into_iter().enumerate() {
                    if !used[nb] && resid[nb] > 0.0 {
                        used[nb] = true;
                        members[k + 1] = nb;
                    }
                }
            }

            let mut e = [0.0f32; MAX_CHANNELS];
            for &m in members.iter().filter(|&&m| m != usize::MAX) {
                e[m] = resid[m];
            }
            let Some(dir) = estimate(&e[..n], &vectors[..n]) else {
                continue;
            };
            let db = energy_to_db(dir.total);
            let intensity = ((db - self.min_db) / (self.max_db - self.min_db)).clamp(0.0, 1.0);
            if intensity <= 0.0 {
                continue;
            }
            if holding
                && (dir.total < OWN_HOLD_MIN_SHARE * eff_total
                    || dir.angle_deg.abs() <= OWN_HOLD_MIN_ANGLE)
            {
                continue; // probably your own gun (or its echo)
            }
            if self.heur_enabled {
                let center: f32 = (0..n)
                    .filter(|&k| self.layout.is_center[k])
                    .map(|k| e[k])
                    .sum();
                if center / dir.total > self.max_center_ratio || dir.focus < self.min_focus {
                    continue;
                }
            }
            if self.last_event.is_none() {
                self.last_event = Some(Event { dir, intensity });
            }
            self.last_event_count += 1;
            self.deposit(dir.angle_deg, intensity);
        }
    }

    /// Loud, front-heavy and balanced left/right: probably your own gun.
    fn is_own_shot(&mut self, eff: &[f32; MAX_CHANNELS]) -> bool {
        let (Some(l), Some(r)) = (self.fl, self.fr) else {
            return false;
        };
        let total: f32 = (0..self.n)
            .filter(|&c| self.layout.vectors[c] != (0.0, 0.0))
            .map(|c| eff[c])
            .sum();
        if total <= f32::MIN_POSITIVE {
            return false;
        }
        let level = energy_to_db(total);
        self.loud_ref_db = self.loud_ref_db.max(level);
        let pair = eff[l] + eff[r];
        let front = pair + self.center.map_or(0.0, |c| eff[c]);
        let balance = (eff[l] - eff[r]).abs() / pair.max(1e-12);
        level >= self.loud_ref_db - OWN_BELOW_PEAK_DB
            && front >= OWN_MIN_FRONTNESS * total
            && balance <= OWN_MAX_BALANCE
    }

    /// Decide whether a surround device is carrying stereo-only content.
    fn update_stereo_detector(&mut self, energy: &[f32; MAX_CHANNELS]) {
        let (mut front, mut rest) = (0.0f32, 0.0f32);
        for c in 0..self.n {
            if self.stereo_vectors[c] != (0.0, 0.0) {
                front += energy[c];
            } else if self.layout.vectors[c] != (0.0, 0.0) {
                rest += energy[c];
            }
        }
        let a = self.mode_a;
        self.front_avg += a * (front - self.front_avg);
        self.rest_avg += a * (rest - self.rest_avg);
        self.active_time = (self.active_time + self.block_dt).min(60.0);

        if self.stereo_content {
            // Leave immediately when a clearly surround sound shows up.
            if rest > 0.3 * front || self.rest_avg > 0.1 * self.front_avg {
                self.stereo_content = false;
                self.active_time = 0.0;
            }
        } else if self.active_time >= MODE_MIN_ACTIVE_S && self.rest_avg < 0.03 * self.front_avg {
            self.stereo_content = true;
        }
    }

    fn deposit(&mut self, angle_deg: f32, intensity: f32) {
        // Flat-topped tent two bins wide: an angle exactly between two bins lights both fully.
        let (lo, hi, w) = angle_to_bins(angle_deg, BINS);
        let wl = (2.0 * (1.0 - w)).min(1.0);
        let wh = (2.0 * w).min(1.0);
        self.bins[lo] = self.bins[lo].max(intensity * wl);
        self.bins[hi] = self.bins[hi].max(intensity * wh);
    }

    fn decay_bins(&mut self, factor: f32) {
        let cut = self.min_intensity;
        for b in &mut self.bins {
            *b *= factor;
            if *b < cut {
                *b = 0.0;
            }
        }
    }

    /// Decay by wall-clock time. Used when WASAPI delivers no packets (loopback
    /// streams go quiet when nothing is playing), so bars still fade out.
    pub fn tick(&mut self, dt_s: f32) {
        let f = (-dt_s.max(0.0) / self.decay_tau_s).exp();
        self.decay_bins(f);
        for m in &mut self.meters[..self.n] {
            *m *= f;
            if *m < 1e-10 {
                *m = 0.0;
            }
        }
    }

    pub fn bins(&self) -> &[f32; BINS] {
        &self.bins
    }

    /// True when nothing is visible and nothing needs to decay.
    pub fn is_idle(&self) -> bool {
        self.bins.iter().all(|&b| b == 0.0) && self.meters[..self.n].iter().all(|&m| m == 0.0)
    }

    /// Quantised bins (0..=255) for lock-free hand-off to the UI thread.
    pub fn quantized_bins(&self, out: &mut [u8; BINS]) {
        for (o, &b) in out.iter_mut().zip(self.bins.iter()) {
            *o = (b * 255.0 + 0.5) as u8;
        }
    }

    /// Per-channel full-band level in dBFS, mapped -90..0 dB -> 0..=255.
    pub fn quantized_meters(&self, out: &mut [u8; MAX_CHANNELS]) {
        for c in 0..MAX_CHANNELS {
            out[c] = if c < self.n {
                let db = energy_to_db(self.meters[c]).clamp(-90.0, 0.0);
                ((db + 90.0) / 90.0 * 255.0 + 0.5) as u8
            } else {
                0
            };
        }
    }
}

#[inline]
fn energy_to_db(e: f32) -> f32 {
    10.0 * e.max(1e-12).log10()
}

#[inline]
fn db_to_energy(db: f32) -> f32 {
    10f32.powf(db / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::dsp::direction::{bin_center_deg, normalize_deg};

    const SR: u32 = 48_000;

    fn analyzer(channels: usize, mask: u32, cfg: &Config) -> Analyzer {
        Analyzer::new(Layout::from_mask(channels, mask, &cfg.speakers), SR, cfg)
    }

    /// `ms` of a 1 kHz tone with the given per-channel amplitudes.
    fn tone(amps: &[f32], ms: u32) -> Vec<f32> {
        let frames = (SR * ms / 1000) as usize;
        let mut v = Vec::with_capacity(frames * amps.len());
        for i in 0..frames {
            let s = (2.0 * core::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin();
            for a in amps {
                v.push(s * a);
            }
        }
        v
    }

    fn peak_bin_angle(a: &Analyzer) -> f32 {
        let (i, _) =
            a.bins().iter().enumerate().fold(
                (0, 0.0f32),
                |acc, (i, &b)| if b > acc.1 { (i, b) } else { acc },
            );
        bin_center_deg(i, BINS)
    }

    #[test]
    fn rear_right_burst_lights_rear_right() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        // FL FR C LFE BL BR SL SR: sound panned between BR (150) and SR (90) -> ~120.
        a.process(&tone(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.3], 100));
        let ev = a.last_event.expect("burst should be detected");
        assert!(
            normalize_deg(ev.dir.angle_deg - 120.0).abs() < 1.0,
            "{}",
            ev.dir.angle_deg
        );
        assert!(normalize_deg(peak_bin_angle(&a) - 120.0).abs() <= 10.0);
        assert!(ev.intensity > 0.5);
    }

    #[test]
    fn low_frequency_rumble_is_attenuated_by_bandpass() {
        let rumble = |bandpass: bool| {
            let mut cfg = Config::default();
            cfg.detection.bandpass = bandpass;
            let mut a = analyzer(2, 0x3, &cfg);
            let mut v = Vec::new();
            for i in 0..SR as usize / 10 {
                let s = 0.3 * (2.0 * core::f32::consts::PI * 30.0 * i as f32 / SR as f32).sin();
                v.extend_from_slice(&[s, 0.0]);
            }
            a.process(&v);
            a.bins().iter().cloned().fold(0.0, f32::max)
        };
        let (on, off) = (rumble(true), rumble(false));
        // 2nd-order HP at 200 Hz: 30 Hz is ~33 dB down = ~0.8 of the 42 dB intensity range.
        assert!(off > 0.95, "unfiltered rumble is loud: {off}");
        assert!(
            on < off - 0.5,
            "band-pass should strongly reduce 30 Hz: on={on} off={off}"
        );
    }

    #[test]
    fn constant_sound_fades_into_noise_floor() {
        let cfg = Config::default();
        let mut a = analyzer(2, 0x3, &cfg);
        // 8 s of steady hard-left tone ("music").
        let chunk = tone(&[0.2, 0.0], 100);
        for _ in 0..80 {
            a.process(&chunk);
        }
        assert!(
            a.last_event.is_none(),
            "steady sound should be absorbed by the noise floor"
        );
        // A new transient on the right is still detected.
        a.process(&tone(&[0.2, 0.4], 50));
        let ev = a.last_event.expect("transient over music");
        assert!(ev.dir.angle_deg > 45.0, "{}", ev.dir.angle_deg);
    }

    #[test]
    fn center_heavy_sound_is_suppressed_only_with_heuristics() {
        let mut cfg = Config::default();
        let amps = [0.05, 0.05, 0.4, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut a = analyzer(8, 0x63F, &cfg);
        a.process(&tone(&amps, 50));
        assert!(a.last_event.is_none());

        cfg.heuristics.enabled = false;
        cfg.detection.ignore_own_gunshots = false;
        let mut b = analyzer(8, 0x63F, &cfg);
        b.process(&tone(&amps, 50));
        assert!(b.last_event.is_some());
    }

    #[test]
    fn own_sound_in_all_channels_does_not_hide_enemy() {
        // You walking: every directional channel at the same level. Enemy on your left
        // adds energy to SL only. (FL FR C LFE BL BR SL SR)
        let mut cfg = Config::default();
        let own = 0.2;
        let amps = [own, own, own, 0.0, own, own, 0.26, own];
        let mut a = analyzer(8, 0x63F, &cfg);
        a.process(&tone(&amps, 60));
        let ev = a.last_event.expect("enemy over own footsteps");
        assert!(
            normalize_deg(ev.dir.angle_deg + 90.0).abs() < 1.0,
            "{}",
            ev.dir.angle_deg
        );
        assert_eq!(
            a.last_event_count, 1,
            "only the enemy, no bars for your own sound"
        );

        // Without common-mode removal your own sound shows up as extra phantom bars.
        cfg.detection.subtract_common = false;
        let mut b = analyzer(8, 0x63F, &cfg);
        b.process(&tone(&amps, 60));
        assert!(
            b.last_event_count > 1,
            "own sound should leak through without subtraction"
        );
    }

    #[test]
    fn own_sound_alone_shows_nothing() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        a.process(&tone(&[0.2, 0.2, 0.2, 0.0, 0.2, 0.2, 0.2, 0.2], 60));
        assert!(a.bins().iter().all(|&b| b == 0.0));
    }

    #[test]
    fn two_simultaneous_sources_give_two_bars() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        // FR (30) and BL (-150) at once.
        a.process(&tone(&[0.0, 0.3, 0.0, 0.0, 0.25, 0.0, 0.0, 0.0], 60));
        assert_eq!(a.last_event_count, 2);
        let lit = |deg: f32| {
            let (lo, hi, _) = angle_to_bins(deg, BINS);
            a.bins()[lo].max(a.bins()[hi]) > 0.3
        };
        assert!(lit(30.0) && lit(-150.0), "{:?}", a.bins());
        assert!(!lit(-60.0), "no phantom source in between");
    }

    #[test]
    fn stereo_centered_own_sound_plus_enemy_left() {
        let cfg = Config::default();
        let mut a = analyzer(2, 0x3, &cfg);
        // Own footsteps (quiet) in both, enemy adds a bit on the left.
        a.process(&tone(&[0.026, 0.02], 60));
        let ev = a.last_event.expect("left excess is shown");
        assert!(normalize_deg(ev.dir.angle_deg + 90.0).abs() < 1.0);
    }

    #[test]
    fn stereo_game_through_7_1_device_is_detected() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        // Stereo game on a 7.1 device: only FL/FR ever carry sound.
        let alternating = [
            tone(&[0.3, 0.05, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 100),
            tone(&[0.05, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 100),
        ];
        for i in 0..30 {
            a.process(&alternating[i % 2]);
        }
        assert!(a.stereo_content(), "should switch to stereo mapping");
        a.process(&tone(&[0.4, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 60));
        let ev = a.last_event.unwrap();
        assert!(
            normalize_deg(ev.dir.angle_deg + 90.0).abs() < 1.0,
            "FL-only = hard left, got {}",
            ev.dir.angle_deg
        );

        // A real surround sound switches straight back.
        a.process(&tone(&[0.0, 0.0, 0.0, 0.0, 0.4, 0.4, 0.0, 0.0], 30));
        assert!(!a.stereo_content());
    }

    #[test]
    fn real_7_1_game_stays_surround() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        for i in 0..40 {
            let amps = if i % 4 == 3 {
                [0.0, 0.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.3]
            } else {
                [0.3, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            };
            a.process(&tone(&amps, 100));
        }
        assert!(!a.stereo_content());
    }

    /// Own gun: loud, front-heavy, with stereo width that flips sides block to block.
    fn own_gun_burst(channels: usize, shots: usize) -> Vec<f32> {
        let mut v = Vec::new();
        for k in 0..shots * 6 {
            // 6 blocks per shot: 3 loud blocks with alternating width, then a quieter tail.
            let (big, small) = if k % 6 < 3 { (0.5, 0.38) } else { (0.08, 0.06) };
            let (l, r) = if k % 2 == 0 {
                (big, small)
            } else {
                (small, big)
            };
            let amps: Vec<f32> = match channels {
                2 => vec![l, r],
                // FL FR C LFE BL BR SL SR: front + a little diffuse echo in the rear/sides.
                _ => vec![l, r, (l + r) / 2.0, 0.0, 0.03, 0.025, 0.035, 0.03],
            };
            v.extend(tone(&amps, 10));
        }
        v
    }

    #[test]
    fn own_gunshots_do_not_flicker_bars_stereo() {
        let mut cfg = Config::default();
        let mut a = analyzer(2, 0x3, &cfg);
        a.process(&own_gun_burst(2, 8));
        assert!(
            a.bins().iter().all(|&b| b < 0.05),
            "own spray should not light bars: {:?}",
            a.bins()
        );

        cfg.detection.ignore_own_gunshots = false;
        let mut b = analyzer(2, 0x3, &cfg);
        b.process(&own_gun_burst(2, 8));
        assert!(
            b.bins().iter().any(|&x| x > 0.3),
            "without the gate the flicker shows"
        );
    }

    #[test]
    fn own_gunshots_do_not_flicker_bars_7_1() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        a.process(&own_gun_burst(8, 8));
        assert!(a.bins().iter().all(|&b| b < 0.05), "{:?}", a.bins());
        assert!(a.own_shot_active());
    }

    #[test]
    fn enemy_shot_from_the_side_is_not_mistaken_for_own() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        // Loud gunshot from the left side (SL), nothing in front.
        a.process(&tone(&[0.05, 0.0, 0.0, 0.0, 0.05, 0.0, 0.5, 0.0], 40));
        let ev = a.last_event.expect("enemy shot shown");
        assert!(ev.dir.angle_deg < -60.0, "{}", ev.dir.angle_deg);
        assert!(!a.own_shot_active());
    }

    #[test]
    fn enemy_from_the_side_still_shows_while_spraying() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        a.process(&own_gun_burst(8, 3));
        // Keep spraying, and an enemy fires from the right side at the same time.
        let mut both = Vec::new();
        for k in 0..12 {
            let (l, r) = if k % 2 == 0 { (0.5, 0.38) } else { (0.38, 0.5) };
            both.extend(tone(&[l, r, (l + r) / 2.0, 0.0, 0.0, 0.0, 0.0, 1.2], 10));
        }
        a.process(&both);
        let (lo, hi, _) = angle_to_bins(90.0, BINS);
        assert!(a.bins()[lo].max(a.bins()[hi]) > 0.3, "{:?}", a.bins());
        let (flo, fhi, _) = angle_to_bins(0.0, BINS);
        assert!(
            a.bins()[flo].max(a.bins()[fhi]) < 0.05,
            "own gun stays hidden"
        );
    }

    #[test]
    fn quiet_footsteps_ahead_are_not_treated_as_gunshots() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        a.process(&tone(&[0.02, 0.02, 0.03, 0.0, 0.0, 0.0, 0.0, 0.0], 60));
        assert!(!a.own_shot_active());
    }

    #[test]
    fn bins_decay_to_zero_and_go_idle() {
        let cfg = Config::default();
        let mut a = analyzer(2, 0x3, &cfg);
        a.process(&tone(&[0.3, 0.0], 50));
        assert!(!a.is_idle());
        for _ in 0..100 {
            a.tick(0.05); // 5 s of wall-clock time with no packets
        }
        assert!(a.is_idle());
    }

    #[test]
    fn silence_flag_counts_blocks() {
        let cfg = Config::default();
        let mut a = analyzer(2, 0x3, &cfg);
        assert_eq!(a.process_silence(4800), 10); // 100 ms of 10 ms blocks
        assert!(a.last_event.is_none());
    }

    #[test]
    fn int16_input_matches_float() {
        let cfg = Config::default();
        let f = tone(&[0.0, 0.3], 50);
        let i: Vec<i16> = f.iter().map(|&x| (x * 32767.0) as i16).collect();
        let mut a = analyzer(2, 0x3, &cfg);
        let mut b = analyzer(2, 0x3, &cfg);
        a.process(&f);
        b.process(&i);
        let (ea, eb) = (a.last_event.unwrap(), b.last_event.unwrap());
        assert!((ea.intensity - eb.intensity).abs() < 0.01);
        assert!((ea.dir.angle_deg - eb.dir.angle_deg).abs() < 0.5);
    }

    #[test]
    fn quantized_outputs() {
        let cfg = Config::default();
        let mut a = analyzer(2, 0x3, &cfg);
        a.process(&tone(&[0.5, 0.0], 50));
        let mut q = [0u8; BINS];
        a.quantized_bins(&mut q);
        assert!(q[27] > 100, "hard left = -90 deg = bin 27: {q:?}");
        let mut m = [0u8; MAX_CHANNELS];
        a.quantized_meters(&mut m);
        assert!(m[0] > 200 && m[1] == 0);
    }
}
