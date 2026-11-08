//! Lock-free hand-off between the capture thread and the UI thread.
//!
//! The capture thread writes quantised radar bins / meter levels into atomics and,
//! only if something changed and no redraw is already queued, posts one message to
//! the UI window. The UI thread never blocks the capture thread and vice versa.

use crate::dsp::analyzer::BINS;
use crate::dsp::layout::MAX_CHANNELS;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering::*};

pub struct Shared {
    pub bins: [AtomicU8; BINS],
    pub meters: [AtomicU8; MAX_CHANNELS],
    /// Stream format info for the settings window (speaker test).
    pub channels: AtomicU32,
    pub channel_mask: AtomicU32,
    pub sample_rate: AtomicU32,
    /// True while capture is running on a device.
    pub capturing: AtomicBool,
    /// Surround device carrying stereo-only content (FL/FR mapped to hard left/right).
    pub stereo_content: AtomicBool,
    /// Overlay visible or settings window open -> capture should run.
    pub wanted: AtomicBool,
    /// A redraw message is queued and not yet handled.
    pub redraw_pending: AtomicBool,
    /// HWND (as isize) that receives redraw messages; 0 = nobody.
    pub notify_hwnd: AtomicIsize,
    /// Set by the UI to ask the capture thread to re-read the config / re-open the device.
    pub reinit_requested: AtomicBool,
    /// Bumped by the UI when detection settings change; the capture thread rebuilds
    /// its analyser on the next wakeup.
    pub params_version: AtomicU32,
    pub quit: AtomicBool,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            bins: std::array::from_fn(|_| AtomicU8::new(0)),
            meters: std::array::from_fn(|_| AtomicU8::new(0)),
            channels: AtomicU32::new(0),
            channel_mask: AtomicU32::new(0),
            sample_rate: AtomicU32::new(0),
            capturing: AtomicBool::new(false),
            stereo_content: AtomicBool::new(false),
            wanted: AtomicBool::new(true),
            redraw_pending: AtomicBool::new(false),
            notify_hwnd: AtomicIsize::new(0),
            reinit_requested: AtomicBool::new(false),
            params_version: AtomicU32::new(0),
            quit: AtomicBool::new(false),
        }
    }

    /// Store new values; returns true if anything changed.
    pub fn publish(&self, bins: &[u8; BINS], meters: &[u8; MAX_CHANNELS]) -> bool {
        let mut changed = false;
        for (a, &v) in self.bins.iter().zip(bins) {
            changed |= a.swap(v, Relaxed) != v;
        }
        for (a, &v) in self.meters.iter().zip(meters) {
            changed |= a.swap(v, Relaxed) != v;
        }
        changed
    }

    pub fn read_bins(&self, out: &mut [u8; BINS]) {
        for (o, a) in out.iter_mut().zip(&self.bins) {
            *o = a.load(Relaxed);
        }
    }

    pub fn read_meters(&self, out: &mut [u8; MAX_CHANNELS]) {
        for (o, a) in out.iter_mut().zip(&self.meters) {
            *o = a.load(Relaxed);
        }
    }

    pub fn clear(&self) {
        self.publish(&[0; BINS], &[0; MAX_CHANNELS]);
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_reports_changes() {
        let s = Shared::new();
        let mut b = [0u8; BINS];
        let m = [0u8; MAX_CHANNELS];
        assert!(!s.publish(&b, &m));
        b[3] = 7;
        assert!(s.publish(&b, &m));
        assert!(!s.publish(&b, &m));
        let mut out = [0u8; BINS];
        s.read_bins(&mut out);
        assert_eq!(out[3], 7);
    }
}
