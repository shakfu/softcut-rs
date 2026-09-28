//! Offline use of the library: record a tone into a loop, then play it back
//! at three rates across six voices, and report the realtime factor.
//!
//! cargo run --release -p softcut --example render

use std::time::Instant;

use softcut::{Engine, EngineCmd, EngineConfig, VoiceCmd};

fn main() {
    let sr = 48000.0;
    let mut e = Engine::new(EngineConfig {
        sample_rate: sr,
        ..Default::default()
    });
    let n = e.voices().len();
    for v in 0..n {
        let rate = [1.0, 0.5, -1.5][v % 3];
        for c in [
            VoiceCmd::LoopStart(0.0),
            VoiceCmd::LoopEnd(2.0),
            VoiceCmd::Loop(true),
            VoiceCmd::Rate(rate),
            VoiceCmd::RecLevel(1.0),
            VoiceCmd::PreLevel(0.5),
            VoiceCmd::Play(true),
            VoiceCmd::Rec(v == 0),
            VoiceCmd::CutTo(0.0),
        ] {
            e.apply(EngineCmd::Voice(v, c));
        }
        e.apply(EngineCmd::Pan(v, -1.0 + 2.0 * v as f32 / (n - 1) as f32));
        e.apply(EngineCmd::InputGain(v, if v == 0 { 1.0 } else { 0.0 }));
        e.apply(EngineCmd::Level(v, 0.3));
    }

    let seconds = 20;
    let block = 256;
    let input: Vec<f32> = (0..block).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
    let mut out = vec![0.0; block * e.out_channels()];
    let (mut sum_sq, mut count) = (0.0f64, 0usize);

    let t0 = Instant::now();
    for _ in 0..(seconds * sr as usize / block) {
        e.process(&input, &mut out);
        sum_sq += out.iter().map(|&x| (x as f64).powi(2)).sum::<f64>();
        count += out.len();
    }
    let elapsed = t0.elapsed().as_secs_f64();

    println!(
        "{n} voices, {seconds} s of audio in {:.1} ms: {:.0}x realtime",
        elapsed * 1e3,
        seconds as f64 / elapsed
    );
    println!("output rms {:.4}", (sum_sq / count as f64).sqrt());
}
