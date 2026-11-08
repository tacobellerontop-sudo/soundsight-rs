//! DSP cost estimate: `cargo run --release --example bench_dsp`
//! Prints how much CPU one core spends analysing real-time audio at 48 kHz
//! (best of several runs, so background noise on the machine doesn't inflate it).

use soundsight::config::Config;
use soundsight::dsp::analyzer::Analyzer;
use soundsight::dsp::layout::Layout;
use std::time::Instant;

fn bench(name: &str, ch: usize, mask: u32, meters: bool) {
    let cfg = Config::default();
    let sr = 48_000usize;
    let mut a = Analyzer::new(Layout::from_mask(ch, mask, &cfg.speakers), sr as u32, &cfg);
    a.set_meters(meters);
    let mut seed = 7u32;
    // 10 ms packets, like WASAPI delivers in event mode.
    let packet: Vec<f32> = (0..sr / 100 * ch)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
        })
        .collect();
    let seconds = 60;
    let mut best = f64::MAX;
    for _ in 0..7 {
        let t = Instant::now();
        for _ in 0..seconds * 100 {
            std::hint::black_box(a.process(std::hint::black_box(&packet)));
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "{name:>6}: {:.3}% of one core ({:.1} us per second of audio)",
        best / seconds as f64 * 100.0,
        best / seconds as f64 * 1e6
    );
}

fn main() {
    println!("In game (Settings closed, speaker-test meters off):");
    bench("stereo", 2, 0x3, false);
    bench("5.1", 6, 0x60F, false);
    bench("7.1", 8, 0x63F, false);
    println!("Settings open (meters on):");
    bench("7.1", 8, 0x63F, true);
}
