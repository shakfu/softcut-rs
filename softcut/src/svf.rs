//! State-variable filter (after Hal Chamberlin, Andy Simper). Port of `Svf.cpp`.

const MAX_NORM_FC: f32 = 0.4;
const MIN_FC: f32 = 10.0;

#[derive(Clone)]
pub(crate) struct Svf {
    lp_mix: f32,
    hp_mix: f32,
    bp_mix: f32,
    br_mix: f32,
    max_fc: f32,
    pi_sr: f32,
    fc: f32,
    rq: f32,
    g1: f32,
    g2: f32,
    g3: f32,
    g4: f32,
    g: f32,
    v0z: f32,
    v1: f32,
    v2: f32,
}

impl Svf {
    pub(crate) fn new() -> Self {
        let mut s = Self {
            lp_mix: 0.0,
            hp_mix: 0.0,
            bp_mix: 0.0,
            br_mix: 0.0,
            max_fc: 0.0,
            pi_sr: 0.0,
            fc: 12000.0,
            rq: 4.0,
            g1: 0.0,
            g2: 0.0,
            g3: 0.0,
            g4: 0.0,
            g: 0.0,
            v0z: 0.0,
            v1: 0.0,
            v2: 0.0,
        };
        s.set_sample_rate(48000.0);
        s
    }

    #[inline]
    pub(crate) fn next(&mut self, v0: f32) -> f32 {
        let v1z = self.v1;
        let v2z = self.v2;
        let v3 = v0 + self.v0z - 2.0 * v2z;
        self.v1 += self.g1 * v3 - self.g2 * v1z;
        self.v2 += self.g3 * v3 + self.g4 * v1z;
        self.v0z = v0;
        let lp = self.v2;
        let bp = self.v1;
        let br = v0 - self.rq * self.v1;
        let hp = br - self.v2;
        lp * self.lp_mix + hp * self.hp_mix + bp * self.bp_mix + br * self.br_mix
    }

    pub(crate) fn set_sample_rate(&mut self, sr: f32) {
        self.pi_sr = (std::f64::consts::PI / sr as f64) as f32;
        self.max_fc = sr * MAX_NORM_FC;
        self.calc_warp();
        self.calc_coeffs();
    }

    /// Clamped to [10 Hz, 0.4 * sr]. A later sample-rate change does not re-clamp.
    pub(crate) fn set_fc(&mut self, fc: f32) {
        self.fc = fc.min(self.max_fc).max(MIN_FC);
        self.calc_warp();
        self.calc_coeffs();
    }

    pub(crate) fn set_rq(&mut self, rq: f32) {
        self.rq = rq;
        self.calc_coeffs();
    }

    pub(crate) fn set_lp_mix(&mut self, x: f32) {
        self.lp_mix = x;
    }
    pub(crate) fn set_hp_mix(&mut self, x: f32) {
        self.hp_mix = x;
    }
    pub(crate) fn set_bp_mix(&mut self, x: f32) {
        self.bp_mix = x;
    }
    pub(crate) fn set_br_mix(&mut self, x: f32) {
        self.br_mix = x;
    }

    /// Effective cutoff, after clamping.
    pub(crate) fn fc(&self) -> f32 {
        self.fc
    }
    pub(crate) fn rq(&self) -> f32 {
        self.rq
    }
    pub(crate) fn lp_mix(&self) -> f32 {
        self.lp_mix
    }
    pub(crate) fn hp_mix(&self) -> f32 {
        self.hp_mix
    }
    pub(crate) fn bp_mix(&self) -> f32 {
        self.bp_mix
    }
    pub(crate) fn br_mix(&self) -> f32 {
        self.br_mix
    }

    pub(crate) fn clear_state(&mut self) {
        self.v0z = 0.0;
        self.v1 = 0.0;
        self.v2 = 0.0;
    }

    fn calc_warp(&mut self) {
        self.g = (self.fc * self.pi_sr).tan();
    }

    fn calc_coeffs(&mut self) {
        let g = self.g;
        self.g1 = g / (1.0 + g * (g + self.rq));
        self.g2 = 2.0 * (g + self.rq) * self.g1;
        self.g3 = g * self.g1;
        self.g4 = 2.0 * self.g1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowpass_passes_dc_highpass_blocks_it() {
        let mut lp = Svf::new();
        lp.set_lp_mix(1.0);
        lp.set_fc(1000.0);
        let mut hp = Svf::new();
        hp.set_hp_mix(1.0);
        hp.set_fc(1000.0);
        let (mut yl, mut yh) = (0.0, 0.0);
        for _ in 0..48000 {
            yl = lp.next(1.0);
            yh = hp.next(1.0);
        }
        assert!((yl - 1.0).abs() < 1e-3, "lp dc gain {yl}");
        assert!(yh.abs() < 1e-3, "hp dc gain {yh}");
    }

    #[test]
    fn fc_is_clamped() {
        let mut s = Svf::new();
        s.set_fc(1.0e6);
        assert_eq!(s.fc, 48000.0 * MAX_NORM_FC);
        s.set_fc(0.0);
        assert_eq!(s.fc, MIN_FC);
    }
}
