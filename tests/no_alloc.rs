//! Proves the audio hot path (Analyzer::process / tick / quantise / publish) never
//! touches the heap, using a counting global allocator.

use soundsight::config::Config;
use soundsight::dsp::analyzer::{Analyzer, BINS};
use soundsight::dsp::layout::{Layout, MAX_CHANNELS};
use soundsight::state::Shared;
use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: AllocLayout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocLayout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocLayout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn hot_path_does_not_allocate() {
    let cfg = Config::default();
    let mut a = Analyzer::new(Layout::from_mask(8, 0x63F, &cfg.speakers), 48_000, &cfg);
    let shared = Shared::new();
    // 100 ms of 7.1 noise-ish signal, prepared before measuring.
    let mut seed = 1u32;
    let buf: Vec<f32> = (0..4800 * 8)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect();
    let i16buf: Vec<i16> = buf.iter().map(|&x| (x * 30000.0) as i16).collect();
    let mut qb = [0u8; BINS];
    let mut qm = [0u8; MAX_CHANNELS];

    let before = ALLOCS.load(Ordering::SeqCst);
    for _ in 0..20 {
        a.process(&buf);
        a.process(&i16buf);
        a.process_silence(480);
        a.tick(0.01);
        a.quantized_bins(&mut qb);
        a.quantized_meters(&mut qm);
        std::hint::black_box(shared.publish(&qb, &qm));
    }
    let after = ALLOCS.load(Ordering::SeqCst);
    assert_eq!(after - before, 0, "audio hot path allocated");
}
