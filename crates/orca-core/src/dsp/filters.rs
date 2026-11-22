//! IIR filtry (RBJ Audio EQ Cookbook) a Linkwitz-Riley 4. řádu pro vícepásmové dělení.

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug, Default)]
pub struct Coefs { pub b0: f64, pub b1: f64, pub b2: f64, pub a1: f64, pub a2: f64 }

impl Coefs {
    pub const IDENTITY: Coefs = Coefs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };
    fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Coefs { Coefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 } }
    fn w(f: f64, fs: f64) -> (f64, f64) { let w0 = 2.0 * PI * (f / fs).clamp(1e-5, 0.49); (w0.cos(), w0.sin()) }
    pub fn lowpass(f: f64, q: f64, fs: f64) -> Coefs { let (c, s) = Self::w(f, fs); let a = s / (2.0 * q); Self::norm((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + a, -2.0 * c, 1.0 - a) }
    pub fn highpass(f: f64, q: f64, fs: f64) -> Coefs { let (c, s) = Self::w(f, fs); let a = s / (2.0 * q); Self::norm((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + a, -2.0 * c, 1.0 - a) }
    pub fn peaking(f: f64, q: f64, gain_db: f64, fs: f64) -> Coefs {
        let (c, s) = Self::w(f, fs); let a = s / (2.0 * q); let g = 10f64.powf(gain_db / 40.0);
        Self::norm(1.0 + a * g, -2.0 * c, 1.0 - a * g, 1.0 + a / g, -2.0 * c, 1.0 - a / g)
    }
    pub fn low_shelf(f: f64, q: f64, gain_db: f64, fs: f64) -> Coefs {
        let (c, s) = Self::w(f, fs); let a = s / (2.0 * q); let g = 10f64.powf(gain_db / 40.0); let sq = 2.0 * g.sqrt() * a;
        Self::norm(g * ((g + 1.0) - (g - 1.0) * c + sq), 2.0 * g * ((g - 1.0) - (g + 1.0) * c), g * ((g + 1.0) - (g - 1.0) * c - sq),
            (g + 1.0) + (g - 1.0) * c + sq, -2.0 * ((g - 1.0) + (g + 1.0) * c), (g + 1.0) + (g - 1.0) * c - sq)
    }
    pub fn high_shelf(f: f64, q: f64, gain_db: f64, fs: f64) -> Coefs {
        let (c, s) = Self::w(f, fs); let a = s / (2.0 * q); let g = 10f64.powf(gain_db / 40.0); let sq = 2.0 * g.sqrt() * a;
        Self::norm(g * ((g + 1.0) + (g - 1.0) * c + sq), -2.0 * g * ((g - 1.0) + (g + 1.0) * c), g * ((g + 1.0) + (g - 1.0) * c - sq),
            (g + 1.0) - (g - 1.0) * c + sq, 2.0 * ((g - 1.0) - (g + 1.0) * c), (g + 1.0) - (g - 1.0) * c - sq)
    }
}

/// Stereo bikvad (transponovaná přímá forma II, f64 stav).
#[derive(Clone, Debug, Default)]
pub struct Biquad { pub c: Coefs, z1: [f64; 2], z2: [f64; 2] }

impl Biquad {
    pub fn new(c: Coefs) -> Self { Self { c, z1: [0.0; 2], z2: [0.0; 2] } }
    #[inline]
    pub fn tick(&mut self, ch: usize, x: f64) -> f64 {
        let c = &self.c;
        let y = c.b0 * x + self.z1[ch];
        self.z1[ch] = c.b1 * x - c.a1 * y + self.z2[ch];
        self.z2[ch] = c.b2 * x - c.a2 * y;
        y
    }
    pub fn reset(&mut self) { self.z1 = [0.0; 2]; self.z2 = [0.0; 2]; }
}

/// Linkwitz-Riley 4. řádu (2× Butterworth) – dolní a horní propust.
#[derive(Clone, Debug)]
pub struct Lr4 { lp: [Biquad; 2], hp: [Biquad; 2] }

const BW_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

impl Lr4 {
    pub fn new(f: f64, fs: f64) -> Self {
        Self { lp: [Biquad::new(Coefs::lowpass(f, BW_Q, fs)), Biquad::new(Coefs::lowpass(f, BW_Q, fs))], hp: [Biquad::new(Coefs::highpass(f, BW_Q, fs)), Biquad::new(Coefs::highpass(f, BW_Q, fs))] }
    }
    #[inline]
    pub fn split(&mut self, ch: usize, x: f64) -> (f64, f64) {
        let l0 = self.lp[0].tick(ch, x); let l = self.lp[1].tick(ch, l0);
        let h0 = self.hp[0].tick(ch, x); let h = self.hp[1].tick(ch, h0);
        (l, h)
    }
}

/// Fázová kompenzace: LP4 + HP4 téže dělicí frekvence = allpass (plochá amplituda).
#[derive(Clone, Debug)]
pub struct Lr4Allpass(Lr4);
impl Lr4Allpass {
    pub fn new(f: f64, fs: f64) -> Self { Self(Lr4::new(f, fs)) }
    #[inline]
    pub fn tick(&mut self, ch: usize, x: f64) -> f64 { let (l, h) = self.0.split(ch, x); l + h }
}

/// Čtyřpásmové dělení s rekombinací bez změny amplitudové odezvy (strom LR4 + allpass kompenzace).
#[derive(Clone, Debug)]
pub struct Crossover4 { mid: Lr4, low: Lr4, high: Lr4, ap_low: Lr4Allpass, ap_high: Lr4Allpass }

impl Crossover4 {
    pub fn new(f: [f64; 3], fs: f64) -> Self {
        let mut f = f;
        let nyq = fs * 0.45;
        f[0] = f[0].clamp(40.0, nyq); f[1] = f[1].clamp(f[0] * 1.5, nyq); f[2] = f[2].clamp(f[1] * 1.5, nyq);
        Self { mid: Lr4::new(f[1], fs), low: Lr4::new(f[0], fs), high: Lr4::new(f[2], fs), ap_low: Lr4Allpass::new(f[2], fs), ap_high: Lr4Allpass::new(f[0], fs) }
    }
    #[inline]
    pub fn split(&mut self, ch: usize, x: f64) -> [f64; 4] {
        let (l, h) = self.mid.split(ch, x);
        let l = self.ap_low.tick(ch, l);
        let h = self.ap_high.tick(ch, h);
        let (b1, b2) = self.low.split(ch, l);
        let (b3, b4) = self.high.split(ch, h);
        [b1, b2, b3, b4]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Amplitudová odezva neutrálního dělení (součet pásem) musí být plochá.
    #[test]
    fn crossover_flat_sum() {
        let fs = 48000.0;
        for f in [50.0, 120.0, 500.0, 1000.0, 3000.0, 5000.0, 10000.0] {
            let mut x = Crossover4::new([120.0, 1000.0, 5000.0], fs);
            let n = 48000;
            let mut peak: f64 = 0.0;
            for i in 0..n {
                let s = (2.0 * PI * f * i as f64 / fs).sin();
                let b = x.split(0, s);
                if i > n / 2 { peak = peak.max((b[0] + b[1] + b[2] + b[3]).abs()); }
            }
            let db = 20.0 * peak.log10();
            assert!(db.abs() < 0.05, "{f} Hz: {db:.3} dB");
        }
    }
    #[test]
    fn shelf_gain() {
        let fs = 48000.0;
        let mut b = Biquad::new(Coefs::low_shelf(100.0, 0.707, 3.0, fs));
        let mut peak: f64 = 0.0;
        for i in 0..48000 { let y = b.tick(0, (2.0 * PI * 30.0 * i as f64 / fs).sin()); if i > 24000 { peak = peak.max(y.abs()); } }
        assert!((20.0 * peak.log10() - 3.0).abs() < 0.2);
    }
}
