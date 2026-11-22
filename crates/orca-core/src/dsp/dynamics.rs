//! Dynamika: pomalé AGC, vícepásmový kompresor (linkované stereo), de-esser.

use super::filters::{Biquad, Coefs};
use super::params::{AgcParams, BandParams, DeesserParams};

#[inline]
pub fn lin_to_db(x: f32) -> f32 { 20.0 * x.max(1e-9).log10() }
#[inline]
pub fn db_to_lin(db: f32) -> f32 { 10f32.powf(db / 20.0) }
fn coef(ms: f32, fs: f32) -> f32 { if ms <= 0.0 { 1.0 } else { 1.0 - (-1.0 / (ms / 1000.0 * fs)).exp() } }

/// Pomalé AGC: RMS detektor (dBFS), cílová úroveň, max. zesílení / zeslabení, rychlost reakce (dB/s),
/// návrat (dB/s), gate a hold. Zisk se počítá po blocích a v rámci bloku se lineárně interpoluje.
pub struct Agc {
    p: AgcParams,
    fs: f32,
    pow_coef: f32,
    pow: f32,
    pub gain_db: f32,
    hold_left: f32,
    pub gated: bool,
}

impl Agc {
    pub fn new(p: &AgcParams, fs: f32) -> Self { Self { p: p.clone(), fs, pow_coef: coef(p.window_ms, fs), pow: 0.0, gain_db: 0.0, hold_left: 0.0, gated: true } }
    pub fn copy_state(&mut self, o: &Agc) { self.pow = o.pow; self.gain_db = o.gain_db.clamp(-self.p.max_cut_db, self.p.max_boost_db); self.hold_left = o.hold_left; self.gated = o.gated; }

    /// Zpracuje blok (prokládané stereo) na místě.
    pub fn process(&mut self, buf: &mut [f32]) {
        let frames = buf.len() / 2;
        if frames == 0 { return; }
        if !self.p.enabled { self.gain_db = 0.0; return; }
        for fr in buf.chunks_exact(2) {
            let e = 0.5 * (fr[0] * fr[0] + fr[1] * fr[1]);
            self.pow += (e - self.pow) * self.pow_coef;
        }
        let level = 10.0 * self.pow.max(1e-12).log10() + 3.01; // RMS sinus 0 dBFS = 0 dB
        let dt = frames as f32 / self.fs;
        let start = self.gain_db;
        if level < self.p.gate_dbfs {
            // ticho / slabý signál: nezvyšovat zisk (zmrazit), po návratu signálu počkat hold
            self.gated = true;
            self.hold_left = self.p.hold_ms / 1000.0;
        } else {
            self.gated = false;
            let desired = (self.p.target_dbfs - level).clamp(-self.p.max_cut_db, self.p.max_boost_db);
            if desired < self.gain_db {
                self.gain_db = (self.gain_db - self.p.attack_db_s * dt).max(desired);
            } else if self.hold_left > 0.0 {
                self.hold_left -= dt;
            } else {
                self.gain_db = (self.gain_db + self.p.release_db_s * dt).min(desired);
            }
        }
        let (g0, g1) = (db_to_lin(start), db_to_lin(self.gain_db));
        for (i, fr) in buf.chunks_exact_mut(2).enumerate() {
            let g = g0 + (g1 - g0) * (i as f32 / frames as f32);
            fr[0] *= g; fr[1] *= g;
        }
    }
}

/// Kompresor jednoho pásma: RMS detektor (linkované stereo), soft knee, attack/release v dB doméně.
pub struct BandComp {
    p: BandParams,
    det_coef: f32,
    att: f32,
    rel: f32,
    pow: f32,
    pub gr_db: f32,
    makeup: f32,
}

impl BandComp {
    pub fn new(p: &BandParams, fs: f32) -> Self {
        Self { p: p.clone(), det_coef: coef(5.0, fs), att: coef(p.attack_ms, fs), rel: coef(p.release_ms, fs), pow: 0.0, gr_db: 0.0, makeup: db_to_lin(p.makeup_db) }
    }
    pub fn copy_state(&mut self, o: &BandComp) { self.pow = o.pow; self.gr_db = o.gr_db.min(self.p.max_gr_db); }
    #[inline]
    fn curve(&self, x_db: f32) -> f32 {
        let (t, r, k) = (self.p.threshold_dbfs, self.p.ratio.max(1.0), self.p.knee_db.max(0.0));
        let over = x_db - t;
        let slope = 1.0 - 1.0 / r;
        let gr = if k > 0.0 && over.abs() <= k / 2.0 { slope * (over + k / 2.0).powi(2) / (2.0 * k) } else if over > 0.0 { slope * over } else { 0.0 };
        gr.min(self.p.max_gr_db)
    }
    /// Vrátí lineární zisk pro vzorek (l, r).
    #[inline]
    pub fn gain(&mut self, l: f32, r: f32) -> f32 {
        let e = l.abs().max(r.abs());
        self.pow += (e * e - self.pow) * self.det_coef;
        let x_db = 10.0 * self.pow.max(1e-12).log10() + 3.01;
        let target = self.curve(x_db);
        let c = if target > self.gr_db { self.att } else { self.rel };
        self.gr_db += (target - self.gr_db) * c;
        db_to_lin(-self.gr_db) * self.makeup
    }
}

/// De-esser: horní pásmo (HPF) se dynamicky zeslabí; doplněk (x − HPF) zůstává → neutrální při 0 dB.
pub struct DeEsser {
    p: DeesserParams,
    hp: Biquad,
    pow: f32,
    det: f32,
    att: f32,
    rel: f32,
    pub gr_db: f32,
}

impl DeEsser {
    pub fn new(p: &DeesserParams, fs: f32) -> Self {
        Self { p: p.clone(), hp: Biquad::new(Coefs::highpass(p.freq_hz as f64, 0.707, fs as f64)), pow: 0.0, det: coef(3.0, fs), att: coef(1.0, fs), rel: coef(60.0, fs), gr_db: 0.0 }
    }
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let hl = self.hp.tick(0, l as f64) as f32;
        let hr = self.hp.tick(1, r as f64) as f32;
        if !self.p.enabled { return (l, r); }
        let e = hl.abs().max(hr.abs());
        self.pow += (e * e - self.pow) * self.det;
        let lvl = 10.0 * self.pow.max(1e-12).log10() + 3.01;
        let target = ((lvl - self.p.threshold_dbfs) * 0.75).clamp(0.0, self.p.max_gr_db);
        let c = if target > self.gr_db { self.att } else { self.rel };
        self.gr_db += (target - self.gr_db) * c;
        let g = db_to_lin(-self.gr_db);
        (l - hl + hl * g, r - hr + hr * g)
    }
}
