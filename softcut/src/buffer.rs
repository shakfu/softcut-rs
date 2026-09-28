//! norns buffer operations on plain sample slices. Port of softcut-py's
//! `buffer_ops.hpp`.
//!
//! Every operation is one blended write over a region of `dst`:
//!
//! `dst = dst * (1 - env) + (dst * preserve + src * mix) * env`
//!
//! `env` is 1 inside the region and ramps linearly from 0 over `fade` frames
//! at each edge (capped at half the region), so the write crossfades into the
//! surrounding audio instead of clicking. Regions are clipped to the slices;
//! nothing allocates, so these are safe on the audio thread.

#[inline]
fn env(i: usize, n: usize, f: usize) -> f32 {
    if f == 0 {
        1.0
    } else if i < f {
        i as f32 / f as f32
    } else if i >= n - f {
        (n - 1 - i) as f32 / f as f32
    } else {
        1.0
    }
}

#[inline]
fn blend(d: f32, s: f32, env: f32, preserve: f32, mix: f32) -> f32 {
    d * (1.0 - env) + (d * preserve + s * mix) * env
}

/// Blend `src` into `dst` starting at `start`. With `preserve = 0, mix = 1`
/// and no fade, a plain overwrite (norns `buffer_read`).
pub fn write(dst: &mut [f32], start: usize, src: &[f32], preserve: f32, mix: f32, fade: usize) {
    let n = src.len().min(dst.len().saturating_sub(start));
    if n == 0 {
        return;
    }
    let f = fade.min(n / 2);
    for (i, (d, &s)) in dst[start..start + n].iter_mut().zip(src).enumerate() {
        *d = blend(*d, s, env(i, n, f), preserve, mix);
    }
}

/// Scale `len` frames from `start` by `preserve`; 0 silences them (norns
/// `buffer_clear_region`).
pub fn clear(dst: &mut [f32], start: usize, len: usize, preserve: f32, fade: usize) {
    let n = len.min(dst.len().saturating_sub(start));
    if n == 0 {
        return;
    }
    let f = fade.min(n / 2);
    for (i, d) in dst[start..start + n].iter_mut().enumerate() {
        *d = blend(*d, 0.0, env(i, n, f), preserve, 0.0);
    }
}

/// Blend `len` frames of `src` from `src_start` into `dst` at `dst_start`,
/// optionally reversed (norns `buffer_copy`, with `mix = 1`). For a copy
/// within one buffer, use [`copy_within`].
#[allow(clippy::too_many_arguments)]
pub fn copy(
    src: &[f32],
    dst: &mut [f32],
    src_start: usize,
    dst_start: usize,
    len: usize,
    preserve: f32,
    fade: usize,
    reverse: bool,
) {
    let src = &src[src_start.min(src.len())..];
    let n = len.min(src.len()).min(dst.len().saturating_sub(dst_start));
    if n == 0 {
        return;
    }
    let f = fade.min(n / 2);
    for (i, d) in dst[dst_start..dst_start + n].iter_mut().enumerate() {
        let s = if reverse { src[n - 1 - i] } else { src[i] };
        *d = blend(*d, s, env(i, n, f), preserve, 1.0);
    }
}

/// [`copy`] within one buffer. Returns false, changing nothing, for the one
/// case that needs a temporary copy: a reversed copy between regions that
/// overlap without coinciding.
///
/// A forward copy iterates away from the overlap, as `memmove` does. A
/// reversed copy onto the same region swaps pairs from the ends inward.
pub fn copy_within(
    buf: &mut [f32],
    src_start: usize,
    dst_start: usize,
    len: usize,
    preserve: f32,
    fade: usize,
    reverse: bool,
) -> bool {
    let limit = buf.len().saturating_sub(src_start.max(dst_start));
    let n = len.min(limit);
    let f = fade.min(n / 2);
    let (s0, d0) = (src_start, dst_start);
    let overlaps = s0 < d0 + n && d0 < s0 + n;

    if !reverse {
        let mut step =
            |i: usize| buf[d0 + i] = blend(buf[d0 + i], buf[s0 + i], env(i, n, f), preserve, 1.0);
        if d0 <= s0 {
            (0..n).for_each(&mut step);
        } else {
            (0..n).rev().for_each(&mut step);
        }
    } else if !overlaps {
        for i in 0..n {
            buf[d0 + i] = blend(
                buf[d0 + i],
                buf[s0 + n - 1 - i],
                env(i, n, f),
                preserve,
                1.0,
            );
        }
    } else if s0 == d0 {
        for i in 0..n.div_ceil(2) {
            let j = n - 1 - i;
            let (a, b) = (buf[d0 + i], buf[d0 + j]);
            buf[d0 + i] = blend(a, b, env(i, n, f), preserve, 1.0);
            if j != i {
                buf[d0 + j] = blend(b, a, env(j, n, f), preserve, 1.0);
            }
        }
    } else {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    /// Reference for copies: take the source out first, as softcut-py does.
    fn reference(
        buf: &[f32],
        s0: usize,
        d0: usize,
        n: usize,
        p: f32,
        f: usize,
        rev: bool,
    ) -> Vec<f32> {
        let mut src = buf[s0..s0 + n].to_vec();
        if rev {
            src.reverse();
        }
        let mut out = buf.to_vec();
        write(&mut out, d0, &src, p, 1.0, f);
        out
    }

    #[test]
    fn write_blends_with_edge_fade() {
        let mut d = vec![1.0; 10];
        write(&mut d, 2, &[3.0; 5], 0.5, 1.0, 2);
        // env over 5 frames, fade 2: 0, 0.5, 1, 0.5, 0.
        let t = |e: f32| 1.0 * (1.0 - e) + (0.5 + 3.0) * e;
        let want = [
            1.0,
            1.0,
            t(0.0),
            t(0.5),
            t(1.0),
            t(0.5),
            t(0.0),
            1.0,
            1.0,
            1.0,
        ];
        assert_eq!(d, want);
    }

    #[test]
    fn write_and_clear_clip_to_the_buffer() {
        let mut d = vec![1.0; 4];
        write(&mut d, 2, &[5.0; 10], 0.0, 1.0, 0);
        assert_eq!(d, [1.0, 1.0, 5.0, 5.0]);
        write(&mut d, 9, &[5.0; 10], 0.0, 1.0, 0);
        clear(&mut d, 1, 100, 0.0, 0);
        assert_eq!(d, [1.0, 0.0, 0.0, 0.0]);
        let mut d = vec![2.0; 4];
        clear(&mut d, 0, 4, 0.5, 0);
        assert_eq!(d, [1.0; 4]);
    }

    #[test]
    fn copy_within_matches_copy_out_first() {
        let buf = ramp(64);
        // (src, dst, len, reverse): overlapping both ways, disjoint, same region.
        let cases = [
            (4, 10, 20, false),
            (10, 4, 20, false),
            (0, 32, 16, true),
            (40, 2, 16, true),
            (8, 8, 21, true),
            (8, 8, 20, true),
        ];
        for (s0, d0, n, rev) in cases {
            for (p, f) in [(0.0, 0), (0.5, 3)] {
                let mut got = buf.clone();
                assert!(copy_within(&mut got, s0, d0, n, p, f, rev));
                assert_eq!(
                    got,
                    reference(&buf, s0, d0, n, p, f, rev),
                    "{s0}->{d0} n={n} rev={rev} p={p} f={f}"
                );
            }
        }
    }

    #[test]
    fn partially_overlapping_reverse_is_refused() {
        let mut b = ramp(32);
        assert!(!copy_within(&mut b, 0, 4, 10, 0.0, 0, true));
        assert_eq!(b, ramp(32));
    }

    #[test]
    fn copy_between_buffers_reverses() {
        let src = ramp(8);
        let mut dst = vec![0.0; 8];
        copy(&src, &mut dst, 2, 1, 4, 0.0, 0, true);
        assert_eq!(dst, [0.0, 5.0, 4.0, 3.0, 2.0, 0.0, 0.0, 0.0]);
    }
}
