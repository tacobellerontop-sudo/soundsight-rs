//! Whole-pipeline cost simulation: `cargo run --release --example bench_pipeline`
//!
//! Feeds 60 s of game-like 7.1 audio (constant ambience, footsteps from changing
//! directions every ~1.2 s, a gunshot burst every ~7 s) through the analyser and the
//! same publish / frame-pacing / painting logic the app uses, and counts what costs
//! CPU and compositor time outside the DSP itself:
//! redraws, pixel bytes pushed to DWM, and how long the overlay windows must be visible.

use soundsight::config::Config;
use soundsight::dsp::analyzer::{Analyzer, BINS};
use soundsight::dsp::layout::Layout;
use soundsight::render::{paint_frame, Bounds, EdgeLayout, Style};

const SR: usize = 48_000;
const CH: usize = 8;

fn main() {
    let cfg = Config::default();
    let mut a = Analyzer::new(Layout::from_mask(CH, 0x63F, &cfg.speakers), SR as u32, &cfg);
    let layout = EdgeLayout::new(2560, 1440, cfg.overlay.thickness as i32);
    let style = Style::from_config(&cfg);
    let mut bufs: [Vec<u32>; 4] = std::array::from_fn(|e| vec![0u32; layout.strip_len(e)]);
    let mut painted: [Option<Bounds>; 4] = [None; 4];

    let mut seed = 1u32;
    let mut rnd = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };

    let packets = 60 * 100; // 10 ms packets
                            // Same pacing as the app: new/louder sounds at refresh_hz, fade-outs at 30 Hz.
    let fast = 1.0 / cfg.general.refresh_hz as f64;
    let slow = fast.max(1.0 / 30.0);
    let (mut last_draw, mut pending, mut rising) = (-1.0f64, false, false);
    // Windows go off screen after 1.5 s without content.
    let (mut last_content, mut shown_packets) = (-10.0f64, 0u64);
    let mut published = [0u8; BINS];
    let (mut requests, mut draws, mut bytes, mut visible_packets) = (0u64, 0u64, 0u64, 0u64);
    let mut packet = vec![0f32; SR / 100 * CH];

    for p in 0..packets {
        let t = p as f64 / 100.0;
        // Footstep: 80 ms burst every 1.2 s from a direction that rotates.
        let step = (t % 1.2) < 0.08;
        let step_ch = [6usize, 4, 5, 7, 1, 0][(t / 1.2) as usize % 6];
        // Gunshot burst: 10 shots over 1 s every 7 s, from the right-rear.
        let gun = (t % 7.0) < 1.0 && (t % 0.1) < 0.03;
        for f in 0..SR / 100 {
            for c in 0..CH {
                let mut x = rnd() * 0.004; // ambience
                if step && c == step_ch {
                    x += rnd() * 0.12;
                }
                if gun && (c == 5 || c == 7) {
                    x += rnd() * 0.6;
                }
                packet[f * CH + c] = x;
            }
        }
        a.process(&packet);

        let mut q = [0u8; BINS];
        a.quantized_bins(&mut q);
        if q != published {
            rising |= q.iter().zip(published.iter()).any(|(n, o)| n > o);
            published = q;
            if !pending {
                pending = true;
                requests += 1;
            }
        }
        // Frame pacing: draw when a change is pending and the cap allows it.
        let interval = if rising { fast } else { slow };
        if pending && t - last_draw >= interval - 1e-9 {
            pending = false;
            rising = false;
            last_draw = t;
            draws += 1;
            let dirty = {
                let [b0, b1, b2, b3] = &mut bufs;
                let mut bb: [&mut [u32]; 4] = [b0, b1, b2, b3];
                paint_frame(&layout, &style, &published, &mut bb, &mut painted)
            };
            for d in dirty.iter().flatten() {
                bytes += d.area() as u64 * 4;
            }
        }
        if painted.iter().any(|p| p.is_some()) {
            visible_packets += 1;
            last_content = t;
        }
        if t - last_content < 1.5 {
            shown_packets += 1;
        }
    }

    println!("60 s of game-like 7.1 audio at 2560x1440:");
    println!("  redraw requests     : {requests}");
    println!(
        "  frames drawn        : {draws} ({:.1} per second)",
        draws as f64 / 60.0
    );
    println!(
        "  pixels pushed to DWM: {:.1} MB ({:.0} KB/s)",
        bytes as f64 / 1e6,
        bytes as f64 / 60.0 / 1e3
    );
    println!(
        "  overlay has content : {:.0}% of the time",
        visible_packets as f64 / packets as f64 * 100.0
    );
    println!(
        "  windows on screen   : {:.0}% of the time",
        shown_packets as f64 / packets as f64 * 100.0
    );
}
