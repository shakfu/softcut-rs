"""Generate golden fixtures for softcut/tests/golden.rs from the C++ softcut-lib.

Runs the scenarios below through softcut-py (Python bindings to softcut-lib)
and writes, per scenario, into softcut/tests/fixtures/:

  <name>.ops      the scenario, one op per line (replayed by the Rust test)
  <name>.in.f32   mono input, little-endian f32
  <name>.out.f32  output of every `process` op, concatenated
  <name>.buf.f32  final contents of buffer 0
  <name>.state    per `process` op: position saved_position quant_phase
                  (Python's rec/play readbacks mirror the last set value, not DSP
                  state, so they are omitted)

Pass scenario names to regenerate only those; with none, all are written.

Needs softcut-py (github.com/shakfu/softcut-py) built from current source.
An installed build older than its source may not match these calls:
  uv venv /tmp/sc && uv pip install --python /tmp/sc/bin/python ../softcut-py
  /tmp/sc/bin/python scripts/gen_fixtures.py
"""

import array
import math
import pathlib
import random

import softcut

SR = 48000.0
FRAMES = 1 << 15
OUT = pathlib.Path(__file__).resolve().parent.parent / "softcut" / "tests" / "fixtures"

BOOLS = {"rec", "play", "loop", "rec_once"}


def sine(n, hz, amp):
    return [amp * math.sin(2 * math.pi * hz * i / SR) for i in range(n)]


def noise(n, amp, seed):
    rng = random.Random(seed)
    return [amp * (2 * rng.random() - 1) for _ in range(n)]


# Voice scenarios. Ops: set <param> <value> | cut <sec> | stop | reset | process <n>
VOICE_SCENARIOS = {
    "record_loop": (
        sine(40000, 220, 0.4),
        """
        set loop_start 0
        set loop_end 0.5
        set loop 1
        set rec_level 1
        set pre_level 0
        set rec 1
        set play 1
        cut 0
        process 30000
        set rec 0
        process 10000
        """,
    ),
    "overdub_varispeed": (
        noise(60000, 0.3, 1),
        """
        set loop_start 0.1
        set loop_end 0.4
        set loop 1
        set fade_time 0.05
        set rec_level 0.8
        set pre_level 0.5
        set rate_slew_time 0.02
        set rec 1
        set play 1
        cut 0.1
        set rate 1.5
        process 20000
        set rate -0.75
        process 20000
        set rate 0.5
        set pre_level 0.9
        process 20000
        """,
    ),
    "filters": (
        noise(30000, 0.5, 2),
        """
        set loop_start 0
        set loop_end 0.25
        set loop 1
        set rec_level 1
        set pre_filter_lp 0
        set pre_filter_bp 1
        set pre_filter_fc 2000
        set pre_filter_rq 1
        set pre_filter_dry 0.2
        set post_filter_hp 1
        set post_filter_lp 0.5
        set post_filter_fc 500
        set post_filter_rq 2
        set post_filter_dry 0
        set rec 1
        set play 1
        cut 0
        set rate 0.5
        set pre_filter_fc_mod 1
        process 15000
        set pre_filter_fc 5000
        process 15000
        """,
    ),
    "rec_once": (
        sine(40000, 330, 0.5),
        """
        set loop_start 0
        set loop_end 0.2
        set loop 1
        set rec_level 1
        set play 1
        cut 0
        set rec_once 1
        """
        + "process 512\n" * 60,
    ),
    "one_shot": (
        sine(20000, 110, 0.3),
        """
        set loop_start 0
        set loop_end 0.1
        set loop 0
        set rec_level 1
        set rec 1
        set play 1
        cut 0
        process 10000
        set rec 0
        cut 0.02
        process 10000
        """,
    ),
    "phase_quant": (
        [0.0] * 30000,
        """
        set loop_start 0
        set loop_end 0.3
        set loop 1
        set phase_quant 0.0625
        set phase_offset 0.01
        set play 1
        cut 0.05
        """
        + "process 1000\n" * 30,
    ),
    # Loop points between samples: 592.59 and 16592.59 frames at 48 kHz.
    "fractional_loop": (
        sine(40000, 220, 0.4),
        """
        set loop_start 0.0123457
        set loop_end 0.3456789
        set loop 1
        set fade_time 0.02
        set rec_level 1
        set pre_level 0.5
        set rec 1
        set play 1
        cut 0.0123457
        process 20000
        set rate 0.75
        process 10000
        set rec 0
        process 10000
        """,
    ),
    "rec_only": (
        sine(20000, 440, 0.6),
        """
        set loop_start 0
        set loop_end 0.3
        set loop 1
        set rec_level 1
        set pre_level 0.25
        set rec 1
        process 20000
        """,
    ),
}

# Engine scenarios: 2 voices sharing buffer 0, block size 64, stereo out.
# Ops: vset <voice> <param> <value> | vcut <voice> <sec> | level|pan|input_gain <voice> <x>
#      | fb <src> <dst> <x> | process <n>
ENGINE_SCENARIOS = {
    "engine_feedback": (
        noise(20000, 0.3, 3),
        """
        vset 0 loop_start 0
        vset 0 loop_end 0.2
        vset 0 loop 1
        vset 0 rec_level 1
        vset 0 rec 1
        vset 0 play 1
        vcut 0 0
        vset 1 loop_start 0.2
        vset 1 loop_end 0.35
        vset 1 loop 1
        vset 1 rec_level 0.7
        vset 1 pre_level 0.5
        vset 1 rate -1.25
        vset 1 rec 1
        vset 1 play 1
        vcut 1 0.3
        pan 0 -0.6
        pan 1 0.8
        level 1 0.7
        input_gain 1 0.3
        fb 0 1 0.5
        fb 1 1 0.2
        process 10000
        fb 1 0 0.4
        process 10000
        """,
    ),
}


def parse_value(name, v):
    return bool(int(v)) if name in BOOLS else float(v)


def apply_voice_op(voice, words):
    op = words[0]
    if op == "set":
        setattr(voice, words[1], parse_value(words[1], words[2]))
    elif op == "cut":
        voice.cut_to(float(words[1]))
    elif op == "stop":
        voice.stop()
    elif op == "reset":
        voice.reset()
    else:
        raise ValueError(op)


def f32(xs):
    return array.array("f", xs)


def save(name, ops, inp, out, buf, states):
    (OUT / f"{name}.ops").write_text(ops)
    for suffix, data in (("in", inp), ("out", out), ("buf", buf)):
        with open(OUT / f"{name}.{suffix}.f32", "wb") as fh:
            f32(data).tofile(fh)
    (OUT / f"{name}.state").write_text("".join(states))


def state_line(v):
    return f"{v.position!r} {v.saved_position!r} {v.quant_phase!r}\n"


def run_voice(name, inp, ops):
    ops = "\n".join(line.strip() for line in ops.strip().splitlines()) + "\n"
    v = softcut.Voice(SR)
    buf = f32([0.0] * FRAMES)
    v.buffer = buf
    pos, out, states = 0, [], []
    for line in ops.splitlines():
        words = line.split()
        if words[0] == "process":
            n = int(words[1])
            o = f32([0.0] * n)
            v.process(f32(inp[pos : pos + n]), o)
            out.extend(o)
            pos += n
            states.append(state_line(v))
        else:
            apply_voice_op(v, words)
    save(name, ops, inp, out, buf, states)


def run_engine(name, inp, ops):
    ops = "\n".join(line.strip() for line in ops.strip().splitlines()) + "\n"
    eng = softcut.Engine(voices=2, mode="playback", sample_rate=SR, block_size=64)
    buf = eng.allocate(frames=FRAMES)
    pos, out, states = 0, [], []
    for line in ops.splitlines():
        w = line.split()
        if w[0] == "process":
            n = int(w[1])
            out.extend(eng.render(input=f32(inp[pos : pos + n])))
            pos += n
            states.append("".join(state_line(v) for v in eng))
        elif w[0] == "vset":
            apply_voice_op(eng[int(w[1])], ["set", w[2], w[3]])
        elif w[0] == "vcut":
            eng[int(w[1])].cut_to(float(w[2]))
        elif w[0] in ("level", "pan", "input_gain"):
            setattr(eng[int(w[1])], w[0], float(w[2]))
        elif w[0] == "fb":
            eng.feedback(int(w[1]), int(w[2]), float(w[3]))
        else:
            raise ValueError(w[0])
    save(name, ops, inp, out, buf, states)


# Buffer ops: blended writes and clears through softcut-py's `_buffer_apply`
# (buffer_ops.hpp). Ops, applied in order to one buffer:
#   write <start> <src_offset> <len> <preserve> <mix> <fade>
#   clear <start> <count> <preserve> <fade>
BUFFER_OPS = """
write 100 0 1000 0 1 0
write 500 1000 800 0.5 0.8 64
clear 700 300 0 32
clear 1200 400 0.25 0
write 3900 0 500 0.3 1 50
clear 4000 500 0.5 20
"""


def run_buffer_ops():
    from softcut import _core

    ops = "\n".join(line.strip() for line in BUFFER_OPS.strip().splitlines()) + "\n"
    buf = f32(noise(4096, 0.5, 4))
    src = f32(noise(2048, 0.7, 5))
    initial = list(buf)
    for line in ops.splitlines():
        w = line.split()
        if w[0] == "write":
            start, off, n = int(w[1]), int(w[2]), int(w[3])
            _core._buffer_apply(buf, start, src[off : off + n], float(w[4]), float(w[5]), int(w[6]))
        else:
            start, count = int(w[1]), int(w[2])
            _core._buffer_apply(buf, start, None, float(w[3]), 0.0, int(w[4]), count)
    (OUT / "buffer_ops.ops").write_text(ops)
    for suffix, data in (("in", initial), ("src", src), ("buf", buf)):
        with open(OUT / f"buffer_ops.{suffix}.f32", "wb") as fh:
            f32(data).tofile(fh)


def main():
    import sys

    known = [*VOICE_SCENARIOS, *ENGINE_SCENARIOS, "buffer_ops"]
    names = sys.argv[1:] or known
    unknown = set(names) - set(known)
    if unknown:
        sys.exit(f"unknown scenarios: {sorted(unknown)}; known: {known}")
    OUT.mkdir(parents=True, exist_ok=True)
    for name in names:
        if name in VOICE_SCENARIOS:
            run_voice(name, *VOICE_SCENARIOS[name])
        elif name in ENGINE_SCENARIOS:
            run_engine(name, *ENGINE_SCENARIOS[name])
        else:
            run_buffer_ops()
    print(f"wrote {len(names)} scenarios to {OUT}")


if __name__ == "__main__":
    main()
