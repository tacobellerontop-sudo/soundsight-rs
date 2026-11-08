//! Minimal biquad IIR filter (RBJ Audio EQ Cookbook), transposed direct form II.
//!
//! A 200 Hz high-pass followed by a 4 kHz low-pass (both Butterworth, Q = 1/sqrt 2)
//! gives a band-pass with well defined edges at about 10 multiply-adds per sample.
//! That is far cheaper than an FFT for our purpose (we only need band energy, not a spectrum).

#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

const BUTTERWORTH_Q: f32 = core::f32::consts::FRAC_1_SQRT_2;

impl Biquad {
    /// Pass-through filter.
    pub fn identity() -> Self {
        Biquad {
            b0: 1.0,
            ..Default::default()
        }
    }

    pub fn highpass(sample_rate: f32, freq: f32) -> Self {
        let (cos_w, alpha) = Self::prewarp(sample_rate, freq);
        let a0 = 1.0 + alpha;
        Self::normalised(
            (1.0 + cos_w) / 2.0,
            -(1.0 + cos_w),
            (1.0 + cos_w) / 2.0,
            a0,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    pub fn lowpass(sample_rate: f32, freq: f32) -> Self {
        let (cos_w, alpha) = Self::prewarp(sample_rate, freq);
        let a0 = 1.0 + alpha;
        Self::normalised(
            (1.0 - cos_w) / 2.0,
            1.0 - cos_w,
            (1.0 - cos_w) / 2.0,
            a0,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn prewarp(sample_rate: f32, freq: f32) -> (f32, f32) {
        // Keep the corner strictly inside (0, Nyquist) so the filter stays stable.
        let f = freq.clamp(1.0, sample_rate * 0.49);
        let w = 2.0 * core::f32::consts::PI * f / sample_rate;
        (w.cos(), w.sin() / (2.0 * BUTTERWORTH_Q))
    }

    fn normalised(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Coefficients (b0, b1, b2, a1, a2), normalised so a0 = 1.
    pub fn coefs(&self) -> [f32; 5] {
        [self.b0, self.b1, self.b2, self.a1, self.a2]
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Zero tiny state values. Called once per analysis block so that the filter
    /// state never decays into denormal floats during silence (denormals can make
    /// x86 float math 10-100x slower).
    #[inline]
    pub fn flush_denormals(&mut self) {
        if self.z1.abs() < 1e-15 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-15 {
            self.z2 = 0.0;
        }
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state RMS gain of `filt` for a sine at `freq`.
    fn gain(mut filt: impl FnMut(f32) -> f32, sr: f32, freq: f32) -> f32 {
        let n = sr as usize; // 1 second
        let mut acc_in = 0.0f64;
        let mut acc_out = 0.0f64;
        for i in 0..n {
            let x = (2.0 * core::f32::consts::PI * freq * i as f32 / sr).sin();
            let y = filt(x);
            if i > n / 2 {
                acc_in += (x * x) as f64;
                acc_out += (y * y) as f64;
            }
        }
        (acc_out / acc_in).sqrt() as f32
    }

    #[test]
    fn bandpass_shape() {
        let sr = 48_000.0;
        let mk = || {
            let mut hp = Biquad::highpass(sr, 200.0);
            let mut lp = Biquad::lowpass(sr, 4000.0);
            move |x| lp.process(hp.process(x))
        };
        let g1k = gain(mk(), sr, 1000.0);
        let g200 = gain(mk(), sr, 200.0);
        let g40 = gain(mk(), sr, 40.0);
        let g15k = gain(mk(), sr, 15_000.0);
        assert!((g1k - 1.0).abs() < 0.05, "passband gain {g1k}");
        assert!(
            (g200 - core::f32::consts::FRAC_1_SQRT_2).abs() < 0.05,
            "-3 dB at corner, got {g200}"
        );
        assert!(g40 < 0.06, "40 Hz should be strongly attenuated, got {g40}");
        assert!(g15k < 0.1, "15 kHz should be attenuated, got {g15k}");
    }

    #[test]
    fn identity_passes() {
        let mut f = Biquad::identity();
        assert_eq!(f.process(0.25), 0.25);
    }
}
