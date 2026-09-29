//! Replays scenarios recorded from the C++ softcut-lib (via softcut-py) and
//! compares output, buffer contents, head positions, both crossfading heads,
//! and the flags softcut-lib changes itself (`rec`, `rec_once`, `fade_time`). Each voice and engine
//! scenario runs under both [`Quirks`] modes.
//! Fixtures come from `scripts/gen_fixtures.py`.

use std::fs;
use std::path::PathBuf;

use softcut::{Engine, EngineCmd, EngineConfig, FadeShape, Quirks, Voice, VoiceCmd, buffer};

const SR: f32 = 48000.0;
const FRAMES: usize = 1 << 15;
/// Fixtures are recorded on x86-64, where Rust matches them exactly. The
/// margin covers other libms, and arm64, where clang fuses `a * b + c`.
const TOL: f32 = 1e-6;
/// Against arm64 recordings, a 1-ulp difference in the slewed rate
/// accumulates in the head phase during varispeed: measured 7.6e-5.
const TOL_VARISPEED: f32 = 2e-4;

fn fixture(name: &str, ext: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.{ext}"))
}

/// A fixture recorded under `quirks`: `Fixed` results live in `fixed/`.
fn result(quirks: Quirks, name: &str, ext: &str) -> PathBuf {
    match quirks {
        Quirks::Upstream => fixture(name, ext),
        Quirks::Fixed => fixture(&format!("fixed/{name}"), ext),
    }
}

fn read_f32(path: PathBuf) -> Vec<f32> {
    let bytes = fs::read(path).unwrap();
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn read_state(quirks: Quirks, name: &str) -> Vec<Vec<f64>> {
    fs::read_to_string(result(quirks, name, "state"))
        .unwrap()
        .lines()
        .map(|l| l.split_whitespace().map(|w| w.parse().unwrap()).collect())
        .collect()
}

fn voice_cmd(param: &str, v: &str) -> VoiceCmd {
    let b = || v == "1";
    let x = || v.parse::<f32>().unwrap();
    let shape = || match v {
        "linear" => FadeShape::Linear,
        "sine" => FadeShape::Sine,
        "raised" => FadeShape::Raised,
        _ => panic!("unknown fade shape {v}"),
    };
    use VoiceCmd::*;
    match param {
        "rate" => Rate(x()),
        "loop_start" => LoopStart(x()),
        "loop_end" => LoopEnd(x()),
        "loop" => Loop(b()),
        "fade_time" => FadeTime(x()),
        "rec_level" => RecLevel(x()),
        "pre_level" => PreLevel(x()),
        "rec" => Rec(b()),
        "rec_once" => RecOnce(b()),
        "play" => Play(b()),
        "rec_offset" => RecOffset(x()),
        "rec_pre_slew_time" => RecPreSlewTime(x()),
        "rate_slew_time" => RateSlewTime(x()),
        "phase_quant" => PhaseQuant(x()),
        "phase_offset" => PhaseOffset(x()),
        "pre_filter_fc" => PreFilterFc(x()),
        "pre_filter_rq" => PreFilterRq(x()),
        "pre_filter_lp" => PreFilterLp(x()),
        "pre_filter_hp" => PreFilterHp(x()),
        "pre_filter_bp" => PreFilterBp(x()),
        "pre_filter_br" => PreFilterBr(x()),
        "pre_filter_dry" => PreFilterDry(x()),
        "pre_filter_fc_mod" => PreFilterFcMod(x()),
        "post_filter_fc" => PostFilterFc(x()),
        "post_filter_rq" => PostFilterRq(x()),
        "post_filter_lp" => PostFilterLp(x()),
        "post_filter_hp" => PostFilterHp(x()),
        "post_filter_bp" => PostFilterBp(x()),
        "post_filter_br" => PostFilterBr(x()),
        "post_filter_dry" => PostFilterDry(x()),
        "rec_fade_shape" => RecFadeShape(shape()),
        "pre_fade_shape" => PreFadeShape(shape()),
        "rec_delay_ratio" => RecDelayRatio(x()),
        "pre_window_ratio" => PreWindowRatio(x()),
        _ => panic!("unknown param {param}"),
    }
}

fn assert_close(what: &str, got: &[f32], want: &[f32], tol: f32) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    let (i, err) = got
        .iter()
        .zip(want)
        .map(|(g, w)| (g - w).abs())
        .enumerate()
        .fold(
            (0, 0.0f32),
            |acc, (i, e)| if e > acc.1 { (i, e) } else { acc },
        );
    assert!(
        err <= tol,
        "{what}: max error {err:e} at {i} (got {}, want {})",
        got[i],
        want[i]
    );
}

fn assert_state(quirks: Quirks, name: &str, got: &[Vec<f64>]) {
    let want = read_state(quirks, name);
    assert_eq!(got.len(), want.len(), "{name}: state count");
    for (k, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.len(), w.len(), "{name}: state {k} field count");
        for (j, (a, b)) in g.iter().zip(w).enumerate() {
            assert!(
                (a - b).abs() < 1e-5,
                "{name} {quirks:?}: state {k} field {j}: got {a}, want {b}"
            );
        }
    }
}

/// Fields as in `state_line` in `scripts/gen_fixtures.py`.
fn state(v: &Voice) -> Vec<f64> {
    let mut s = vec![
        v.position() as f64,
        v.saved_position() as f64,
        v.quant_phase(),
        v.rec() as u8 as f64,
        v.rec_once() as u8 as f64,
        v.play() as u8 as f64,
        v.fade_time() as f64,
    ];
    for h in v.heads() {
        s.extend([h.position as f64, h.fade as f64, h.active as u8 as f64]);
    }
    s
}

const QUIRKS: [Quirks; 2] = [Quirks::Upstream, Quirks::Fixed];

fn run_voice(name: &str, compare_output: bool, tol: f32) {
    for q in QUIRKS {
        run_voice_with(q, name, compare_output, tol);
    }
}

fn run_voice_with(quirks: Quirks, name: &str, compare_output: bool, tol: f32) {
    let input = read_f32(fixture(name, "in.f32"));
    let ops = fs::read_to_string(fixture(name, "ops")).unwrap();
    let mut v = Voice::with_quirks(SR, quirks);
    let mut buf = vec![0.0f32; FRAMES];
    let (mut pos, mut out, mut states) = (0, Vec::new(), Vec::new());
    for line in ops.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w[0] {
            "process" => {
                let n: usize = w[1].parse().unwrap();
                let mut o = vec![0.0; n];
                v.process_block(&mut buf, &input[pos..pos + n], &mut o);
                out.extend(o);
                pos += n;
                states.push(state(&v));
            }
            "set" => v.apply(voice_cmd(w[1], w[2])),
            "cut" => v.cut_to(w[1].parse().unwrap()),
            "stop" => v.stop(),
            "reset" => v.reset(),
            "fill" => buf.fill(w[1].parse().unwrap()),
            op => panic!("unknown op {op}"),
        }
    }
    if compare_output {
        assert_close(
            &format!("{name} {quirks:?} output"),
            &out,
            &read_f32(result(quirks, name, "out.f32")),
            tol,
        );
    }
    assert_close(
        &format!("{name} {quirks:?} buffer"),
        &buf,
        &read_f32(result(quirks, name, "buf.f32")),
        tol,
    );
    assert_state(quirks, name, &states);
}

#[test]
fn record_loop() {
    run_voice("record_loop", true, TOL);
}

#[test]
fn overdub_varispeed() {
    run_voice("overdub_varispeed", true, TOL_VARISPEED);
}

#[test]
fn filters() {
    run_voice("filters", true, TOL);
}

#[test]
fn rec_once() {
    run_voice("rec_once", true, TOL);
}

#[test]
fn one_shot() {
    run_voice("one_shot", true, TOL);
}

#[test]
fn phase_quant() {
    run_voice("phase_quant", true, TOL);
}

/// Loop points between samples (592.59 and 16592.59 frames), with overdub
/// and a rate change.
#[test]
fn fractional_loop() {
    run_voice("fractional_loop", true, TOL);
}

/// Upstream leaves the output unwritten when recording without playing, so
/// only the buffer is compared.
#[test]
fn rec_only() {
    run_voice("rec_only", false, TOL);
}

/// A raised pre curve under a linear rec curve: upstream ignores it, `Fixed`
/// applies it.
#[test]
fn pre_curve_quirk() {
    let name = "pre_curve_quirk";
    let buf = |q| read_f32(result(q, name, "buf.f32"));
    assert_ne!(
        buf(Quirks::Upstream),
        buf(Quirks::Fixed),
        "fixtures must differ between modes to exercise the quirk"
    );
    run_voice(name, true, TOL);
}

#[test]
fn engine_feedback() {
    for q in QUIRKS {
        engine_feedback_with(q);
    }
}

fn engine_feedback_with(quirks: Quirks) {
    let name = "engine_feedback";
    let input = read_f32(fixture(name, "in.f32"));
    let ops = fs::read_to_string(fixture(name, "ops")).unwrap();
    let mut e = Engine::new(EngineConfig {
        sample_rate: SR,
        voices: 2,
        buffers: 1,
        buffer_frames: FRAMES,
        block_size: 64,
        out_channels: 2,
        quirks,
        ..Default::default()
    });
    let (mut pos, mut out, mut states) = (0, Vec::new(), Vec::new());
    for line in ops.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let idx = |k: usize| w[k].parse::<usize>().unwrap();
        let x = |k: usize| w[k].parse::<f32>().unwrap();
        match w[0] {
            "process" => {
                let n = idx(1);
                let mut o = vec![0.0; n * 2];
                e.process(&input[pos..pos + n], &mut o);
                out.extend(o);
                pos += n;
                for v in e.voices() {
                    states.push(state(v));
                }
            }
            "vset" => e.apply(EngineCmd::Voice(idx(1), voice_cmd(w[2], w[3]))),
            "vcut" => e.apply(EngineCmd::Voice(idx(1), VoiceCmd::CutTo(x(2)))),
            "level" => e.apply(EngineCmd::Level(idx(1), x(2))),
            "pan" => e.apply(EngineCmd::Pan(idx(1), x(2))),
            "input_gain" => e.apply(EngineCmd::InputGain(idx(1), x(2))),
            "fb" => e.apply(EngineCmd::Feedback {
                src: idx(1),
                dst: idx(2),
                amount: x(3),
            }),
            op => panic!("unknown op {op}"),
        }
    }
    assert_close(
        &format!("engine {quirks:?} output"),
        &out,
        &read_f32(result(quirks, name, "out.f32")),
        TOL,
    );
    assert_close(
        &format!("engine {quirks:?} buffer"),
        &e.buffers()[0],
        &read_f32(result(quirks, name, "buf.f32")),
        TOL,
    );
    assert_state(quirks, name, &states);
}

#[test]
fn buffer_ops() {
    let name = "buffer_ops";
    let mut buf = read_f32(fixture(name, "in.f32"));
    let src = read_f32(fixture(name, "src.f32"));
    let ops = fs::read_to_string(fixture(name, "ops")).unwrap();
    for line in ops.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let u = |k: usize| w[k].parse::<usize>().unwrap();
        let x = |k: usize| w[k].parse::<f32>().unwrap();
        match w[0] {
            "write" => buffer::write(&mut buf, u(1), &src[u(2)..u(2) + u(3)], x(4), x(5), u(6)),
            "clear" => buffer::clear(&mut buf, u(1), u(2), x(3), u(4)),
            op => panic!("unknown op {op}"),
        }
    }
    assert_close("buffer ops", &buf, &read_f32(fixture(name, "buf.f32")), TOL);
}
