//! Crossfaded looping and overdub: a looped sine plays through its wraps
//! without clicks when faded, and clicks without a fade.
//!
//! The loop is 0.3333332 s, 73.33 cycles of 220 Hz, so each wrap joins two
//! phases 120 degrees apart. The measure is the largest jump between adjacent
//! output samples near each wrap, against a pure sine's largest step.

use softcut::{Quirks, Voice};

const SR: f32 = 48000.0;
/// Loop points between samples: 592.59 and 16592.59 frames.
const START: f32 = 0.0123457;
const END: f32 = 0.3456789;
const HZ: f32 = 220.0;
const AMP: f32 = 0.4;

fn sine(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| AMP * (std::f32::consts::TAU * HZ * i as f32 / SR).sin())
        .collect()
}

/// The largest step between adjacent samples of the sine itself.
fn sine_step() -> f32 {
    AMP * std::f32::consts::TAU * HZ / SR
}

fn looping_voice(fade: f32) -> Voice {
    let mut v = Voice::with_quirks(SR, Quirks::Fixed);
    v.set_loop_start(START);
    v.set_loop_end(END);
    v.set_loop(true);
    v.set_fade_time(fade);
    v
}

/// Play `frames` one sample at a time, returning the largest adjacent-sample
/// jump within each wrap's crossfade (plus 20 samples either side), and the
/// number of wraps.
fn jump_at_wraps(v: &mut Voice, buf: &mut [f32], frames: usize, fade: f32) -> (f32, usize) {
    let (mut out, mut pos) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
    for _ in 0..frames {
        let mut y = [0.0];
        v.process_block(buf, &[0.0], &mut y);
        out.push(y[0]);
        pos.push(v.position());
    }
    let wraps: Vec<usize> = (1..frames).filter(|&i| pos[i] < pos[i - 1] - 0.1).collect();
    let span = (fade * SR) as usize + 20;
    let jump = wraps
        .iter()
        .flat_map(|&w| w.saturating_sub(20)..(w + span).min(frames - 1))
        .map(|i| (out[i + 1] - out[i]).abs())
        .fold(0.0, f32::max);
    (jump, wraps.len())
}

#[test]
fn filled_loop_wraps_without_clicks() {
    let play = |fade: f32| {
        let mut buf = vec![0.0; 1 << 15];
        buf[..20000].copy_from_slice(&sine(20000));
        let mut v = looping_voice(fade);
        v.set_play(true);
        v.cut_to(START);
        jump_at_wraps(&mut v, &mut buf, 96000, fade)
    };
    let (faded, wraps) = play(0.01);
    let (cut, _) = play(0.0);
    assert!(wraps >= 5, "{wraps} wraps");
    // Measured: 0.0115 faded (the sine's own step), 0.515 without a fade.
    assert!(faded < 1.25 * sine_step(), "faded jump {faded}");
    assert!(cut > 10.0 * sine_step(), "unfaded jump {cut}");
}

/// Recording crossfades write the same input through both heads, so a later
/// playback crossfade reads matching material from either side of the wrap.
/// Stopping the recording mid-loop leaves a separate seam, away from the
/// wraps, which no fade setting removes.
#[test]
fn overdubbed_loop_wraps_without_clicks() {
    let record_and_play = |fade: f32| {
        let mut buf = vec![0.0; 1 << 15];
        let mut v = looping_voice(fade);
        v.set_rec_level(1.0);
        v.set_pre_level(0.5);
        v.set_rec(true);
        v.set_play(true);
        v.cut_to(START);
        // About 2.4 passes, overdubbing each.
        let mut out = vec![0.0; 40000];
        v.process_block(&mut buf, &sine(40000), &mut out);
        v.set_rec(false);
        jump_at_wraps(&mut v, &mut buf, 96000, fade)
    };
    let (faded, wraps) = record_and_play(0.01);
    let (cut, _) = record_and_play(0.0);
    assert!(wraps >= 5, "{wraps} wraps");
    // Measured: 0.0119 faded, 0.260 without a fade.
    assert!(faded < 1.25 * sine_step(), "faded jump {faded}");
    assert!(cut > 10.0 * sine_step(), "unfaded jump {cut}");
}
