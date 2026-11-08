//! DSP cost estimate: `cargo run --release --example bench_dsp`
//! Prints how much CPU one core spends analysing real-time 7.1 audio at 48 kHz.

use soundsight::config::Config;
use soundsight::dsp::analyzer::Analyzer;
use soundsight::dsp::layout::Layout;
use std::time::Instant;

fn main() {
    let cfg = Config::default();
    let (sr, ch) = (48_000usize, 8usize);
    let mut a = Analyzer::new(Layout::from_mask(ch, 0x63F, &cfg.speakers), sr as u32, &cfg);
    let mut seed = 7u32;
    // 10 ms packets, like WASAPI delivers in event mode.
    let packet: Vec<f32> = (0..sr / 100 * ch)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
        })
        .collect();
    let seconds_of_audio = 120;
    let t = Instant::now();
    for _ in 0..seconds_of_audio * 100 {
        std::hint::black_box(a.process(std::hint::black_box(&packet)));
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "analysed {seconds_of_audio} s of 7.1 @ 48 kHz in {:.1} ms -> {:.3}% of one core (DSP only)",
        el * 1000.0,
        el / seconds_of_audio as f64 * 100.0
    );
}
