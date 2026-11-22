//! Vyrovnání hlasitosti programu (LUFS leveler): měří krátkodobou hlasitost podle ITU-R BS.1770
//! (K-filtr, okno 3 s) a pomalu dorovnává zisk k cílové hlasitosti stanice. Srovná soubory bez analýzy,
//! převzaté vysílání, živé vstupy i rozdíly mezi skladbami – ještě před zvukovým processingem.
//! Ticho a velmi tiché pasáže (gate) zesilování zastaví, při mikrofonu (talk-over) se zisk drží.

#[derive(Clone, Copy)]
struct Biquad { b0: f64, b1: f64, b2: f64, a1: f64, a2: f64, z1: f64, z2: f64 }
impl Biquad {
    fn run(&mut self, x: f64) -> f64 { let y = self.b0 * x + self.z1; self.z1 = self.b1 * x - self.a1 * y + self.z2; self.z2 = self.b2 * x - self.a2 * y; y }
}

/// K-filtr BS.1770 pro libovolnou vzorkovací frekvenci.
fn k_filter(fs: f64) -> [Biquad; 2] {
    let pi = std::f64::consts::PI;
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (pi * f0 / fs).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad { b0: (vh + vb * k / q + k * k) / a0, b1: 2.0 * (k * k - vh) / a0, b2: (vh - vb * k / q + k * k) / a0, a1: 2.0 * (k * k - 1.0) / a0, a2: (1.0 - k / q + k * k) / a0, z1: 0.0, z2: 0.0 };
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (pi * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let hp = Biquad { b0: 1.0, b1: -2.0, b2: 1.0, a1: 2.0 * (k * k - 1.0) / a0, a2: (1.0 - k / q + k * k) / a0, z1: 0.0, z2: 0.0 };
    [shelf, hp]
}

#[derive(Debug, Clone, Copy)]
pub struct LevelerParams {
    pub target_lufs: f64,
    pub max_boost_db: f64,
    pub max_cut_db: f64,
    /// Rychlost zesílení / zeslabení (dB/s)
    pub up_db_s: f64,
    pub down_db_s: f64,
    /// Pod cílem o víc než gate_lu se nezesiluje (ticho, dojezdy, tiché úvody)
    pub gate_lu: f64,
}
impl LevelerParams {
    pub fn for_speed(target_lufs: f64, max_boost_db: f64, max_cut_db: f64, speed: &str) -> Self {
        let (up, down) = match speed { "slow" => (0.5, 2.0), "fast" => (4.0, 10.0), _ => (1.5, 5.0) };
        Self { target_lufs, max_boost_db: max_boost_db.clamp(0.0, 24.0), max_cut_db: max_cut_db.clamp(0.0, 24.0), up_db_s: up, down_db_s: down, gate_lu: 20.0 }
    }
}

pub struct Leveler {
    fs: f64,
    kf: [[Biquad; 2]; 2],
    /// energie po 100ms blocích (K-vážená, průměr L+R)
    blocks: std::collections::VecDeque<f64>,
    acc: f64,
    acc_n: usize,
    block_len: usize,
    gain_db: f64,
    loudness: f64,
}

impl Leveler {
    pub fn new(fs: u32) -> Self {
        let k = k_filter(fs as f64);
        Self { fs: fs as f64, kf: [k, k], blocks: Default::default(), acc: 0.0, acc_n: 0, block_len: (fs / 10).max(1) as usize, gain_db: 0.0, loudness: -70.0 }
    }
    pub fn gain_db(&self) -> f64 { self.gain_db }
    /// Krátkodobá hlasitost vstupu (LUFS, okno 3 s)
    pub fn loudness(&self) -> f64 { self.loudness }
    pub fn reset_gain(&mut self) { self.gain_db = 0.0; }

    /// Zpracuje prokládané stereo na místě. `freeze` = držet zisk (mikrofon / talk-over).
    pub fn process(&mut self, buf: &mut [f32], p: &LevelerParams, freeze: bool) {
        let frames = buf.len() / 2;
        if frames == 0 { return; }
        // měření (před ziskem)
        for f in 0..frames {
            let l0 = self.kf[0][0].run(buf[f * 2] as f64);
            let l = self.kf[0][1].run(l0);
            let r0 = self.kf[1][0].run(buf[f * 2 + 1] as f64);
            let r = self.kf[1][1].run(r0);
            self.acc += l * l + r * r;
            self.acc_n += 1;
            if self.acc_n >= self.block_len {
                self.blocks.push_back(self.acc / self.acc_n as f64);
                if self.blocks.len() > 30 { self.blocks.pop_front(); }
                self.acc = 0.0; self.acc_n = 0;
            }
        }
        if !self.blocks.is_empty() {
            let ms = self.blocks.iter().sum::<f64>() / self.blocks.len() as f64;
            self.loudness = if ms > 1e-12 { -0.691 + 10.0 * ms.log10() } else { -70.0 };
        }
        // řízení zisku
        let dt = frames as f64 / self.fs;
        let start = self.gain_db;
        if !freeze && self.blocks.len() >= 10 {
            let want = (p.target_lufs - self.loudness).clamp(-p.max_cut_db, p.max_boost_db);
            let gated = self.loudness < p.target_lufs - p.gate_lu || self.loudness < -55.0;
            if want < self.gain_db { self.gain_db = (self.gain_db - p.down_db_s * dt).max(want); }
            else if !gated { self.gain_db = (self.gain_db + p.up_db_s * dt).min(want); }
        }
        // plynulá rampa zisku přes blok
        let (g0, g1) = (10f64.powf(start / 20.0), 10f64.powf(self.gain_db / 20.0));
        for f in 0..frames {
            let g = (g0 + (g1 - g0) * (f as f64 + 1.0) / frames as f64) as f32;
            buf[f * 2] *= g;
            buf[f * 2 + 1] *= g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: f32, secs: f32, fs: u32) -> Vec<f32> {
        let n = (secs * fs as f32) as usize;
        (0..n).flat_map(|i| { let v = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / fs as f32).sin() * amp; [v, v] }).collect()
    }
    fn lufs(buf: &[f32], fs: u32) -> f64 {
        let mut m = ebur128::EbuR128::new(2, fs, ebur128::Mode::I).unwrap();
        m.add_frames_f32(buf).unwrap();
        m.loudness_global().unwrap()
    }

    #[test]
    fn levels_quiet_and_loud_program_to_target() {
        let fs = 48_000;
        let p = LevelerParams::for_speed(-18.0, 12.0, 12.0, "fast");
        let mut lv = Leveler::new(fs);
        let mut res = vec![];
        // tichý program (−32 LUFS), hlasitý (−9 LUFS), znovu tichý – po 20 s
        for amp in [0.035f32, 0.5, 0.035] {
            let mut x = tone(amp, 20.0, fs);
            let before = lufs(&x[x.len() / 2..], fs);
            for ch in x.chunks_mut(960) { lv.process(ch, &p, false); }
            let after = lufs(&x[x.len() / 2..], fs);
            res.push((before, after));
        }
        for (b, a) in &res { assert!((a + 18.0).abs() < 1.0, "vstup {b:.1} LUFS → výstup {a:.1} LUFS (cíl −18)"); }
        let spread_in = res.iter().map(|r| r.0).fold(f64::MIN, f64::max) - res.iter().map(|r| r.0).fold(f64::MAX, f64::min);
        let spread_out = res.iter().map(|r| r.1).fold(f64::MIN, f64::max) - res.iter().map(|r| r.1).fold(f64::MAX, f64::min);
        assert!(spread_in > 20.0 && spread_out < 1.0, "rozptyl {spread_in:.1} → {spread_out:.1} LU");
    }

    #[test]
    fn silence_is_not_boosted_and_freeze_holds() {
        let fs = 48_000;
        let p = LevelerParams::for_speed(-18.0, 12.0, 12.0, "fast");
        let mut lv = Leveler::new(fs);
        let mut x = tone(0.0005, 10.0, fs); // ~ −63 LUFS = šum / ticho
        for ch in x.chunks_mut(960) { lv.process(ch, &p, false); }
        assert!(lv.gain_db().abs() < 0.01, "ticho nezesiluje: {}", lv.gain_db());
        let mut y = tone(0.5, 10.0, fs);
        for ch in y.chunks_mut(960) { lv.process(ch, &p, true); }
        assert!(lv.gain_db().abs() < 0.01, "freeze drží zisk");
    }
}
