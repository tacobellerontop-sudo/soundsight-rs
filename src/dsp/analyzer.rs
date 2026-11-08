//! Block analyser: interleaved samples in, radar bins out.
//!
//! Pipeline per sample:   x -> [HP 200 Hz] -> [LP 4 kHz] -> accumulate x^2 per channel
//! Pipeline per block (~10 ms):
//!   1. mean-square energy per channel (band-passed; also full-band for the meter)
//!   2. subtract a per-channel adaptive noise floor (slow rise / fast fall), so
//!      constant sounds (music, ambience, wind) fade out of the picture
//!   3. reject the block unless it is `snr_db` above the floor
//!   4. energy-vector direction + loudness (dBFS -> 0..1 intensity)
//!   5. optional heuristics (centre-heavy / unfocused -> suppress)
//!   6. deposit into a circular histogram of `BINS` directions; bins decay exponentially
//!
//! No heap allocation: every buffer is a fixed-size array inside the struct.

use super::biquad::Biquad;
use super::direction::{angle_to_bins, estimate, DirectionEstimate};
use super::layout::{Layout, MAX_CHANNELS};
use crate::config::Config;

/// Number of radar directions (10 degrees each).
pub const BINS: usize = 36;

/// Bins/meters below this are treated as zero (also stops endless tiny decays).
const ZERO: f32 = 1.0 / 512.0;

/// Interleaved sample formats we accept from WASAPI.
pub trait Sample: Copy {
    fn to_f32(self) -> f32;
}
impl Sample for f32 {
    #[inline(always)]
    fn to_f32(self) -> f32 {
        self
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
    hp: [Biquad; MAX_CHANNELS],
    lp: [Biquad; MAX_CHANNELS],

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

    decay_tau_s: f32,
    decay_per_block: f32,
    bins: [f32; BINS],

    /// Full-band mean square per channel with peak-hold style release, for the calibration meter.
    meters: [f32; MAX_CHANNELS],

    pub last_event: Option<Event>,
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
        Analyzer {
            stride: layout.channels.max(1),
            n: layout.analysed_channels(),
            bandpass: d.bandpass,
            hp: [Biquad::highpass(sr, d.band_low_hz); MAX_CHANNELS],
            lp: [Biquad::lowpass(sr, d.band_high_hz); MAX_CHANNELS],
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
            decay_tau_s: tau,
            decay_per_block: (-block_dt / tau).exp(),
            bins: [0.0; BINS],
            meters: [0.0; MAX_CHANNELS],
            last_event: None,
            layout,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn block_seconds(&self) -> f32 {
        self.block_dt
    }

    /// Feed interleaved frames. `data.len()` should be a multiple of the channel count;
    /// a trailing partial frame is ignored. Returns the number of blocks completed.
    #[inline]
    pub fn process<S: Sample>(&mut self, data: &[S]) -> usize {
        let mut blocks = 0;
        let n = self.n;
        for frame in data.chunks_exact(self.stride) {
            let frame = &frame[..n];
            if self.bandpass {
                for c in 0..n {
                    let x = frame[c].to_f32();
                    self.acc_full[c] += x * x;
                    let y = self.lp[c].process(self.hp[c].process(x));
                    self.acc_band[c] += y * y;
                }
            } else {
                for c in 0..n {
                    let x = frame[c].to_f32();
                    let e = x * x;
                    self.acc_full[c] += e;
                    self.acc_band[c] += e;
                }
            }
            self.count += 1;
            if self.count >= self.block_frames {
                self.finish_block();
                blocks += 1;
            }
        }
        blocks
    }

    /// Account for `frames` frames of digital silence (WASAPI AUDCLNT_BUFFERFLAGS_SILENT)
    /// without touching the sample data.
    pub fn process_silence(&mut self, mut frames: usize) -> usize {
        let mut blocks = 0;
        for c in 0..self.n {
            self.hp[c].reset();
            self.lp[c].reset();
        }
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
            self.hp[c].flush_denormals();
            self.lp[c].flush_denormals();

            self.meters[c] = full.max(self.meters[c] * meter_release);
            energy[c] = e;

            let floor = self.floor[c];
            eff[c] = (e - floor).max(0.0);
            let a = if e > floor { self.floor_rise_a } else { self.floor_fall_a };
            self.floor[c] = floor + a * (e - floor);

            let directional = self.layout.vectors[c] != (0.0, 0.0);
            if directional {
                total += e;
                floor_total += floor;
            }
        }

        self.decay_bins(self.decay_per_block);
        self.last_event = None;

        if total <= floor_total * self.snr_lin || total <= f32::MIN_POSITIVE {
            return;
        }
        let Some(dir) = estimate(&eff[..n], &self.layout.vectors[..n]) else { return };

        let db = energy_to_db(dir.total);
        let intensity = ((db - self.min_db) / (self.max_db - self.min_db)).clamp(0.0, 1.0);
        if intensity <= 0.0 {
            return;
        }

        if self.heur_enabled {
            let center: f32 = (0..n).filter(|&c| self.layout.is_center[c]).map(|c| eff[c]).sum();
            if center / dir.total > self.max_center_ratio || dir.focus < self.min_focus {
                return;
            }
        }

        self.last_event = Some(Event { dir, intensity });
        self.deposit(dir.angle_deg, intensity);
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
        for b in &mut self.bins {
            *b *= factor;
            if *b < ZERO {
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
        let (i, _) = a.bins().iter().enumerate().fold((0, 0.0f32), |acc, (i, &b)| if b > acc.1 { (i, b) } else { acc });
        bin_center_deg(i, BINS)
    }

    #[test]
    fn rear_right_burst_lights_rear_right() {
        let cfg = Config::default();
        let mut a = analyzer(8, 0x63F, &cfg);
        // FL FR C LFE BL BR SL SR: sound panned between BR (150) and SR (90) -> ~120.
        a.process(&tone(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.3], 100));
        let ev = a.last_event.expect("burst should be detected");
        assert!(normalize_deg(ev.dir.angle_deg - 120.0).abs() < 1.0, "{}", ev.dir.angle_deg);
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
        assert!(on < off - 0.5, "band-pass should strongly reduce 30 Hz: on={on} off={off}");
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
        assert!(a.last_event.is_none(), "steady sound should be absorbed by the noise floor");
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
        let mut b = analyzer(8, 0x63F, &cfg);
        b.process(&tone(&amps, 50));
        assert!(b.last_event.is_some());
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
