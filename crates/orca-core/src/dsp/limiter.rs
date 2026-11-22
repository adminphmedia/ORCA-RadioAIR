//! True-peak limiter: stereo-linked, lookahead, detekce mezivzorkových špiček 4× převzorkováním
//! (polyfázový FIR, princip ITU-R BS.1770-4 příloha 2). Bez alokací při zpracování.

/// 4× polyfázový interpolátor pro odhad true peaku.
#[derive(Clone)]
pub struct Oversampler4 {
    taps: Vec<[f32; 4]>, // taps[k][phase]
    hist: [Vec<f32>; 2],
    pos: usize,
}

pub const OS_TAPS_PER_PHASE: usize = 32;

impl Oversampler4 {
    pub fn new() -> Self {
        // okénkovaná sinc (Kaiser β = 8), mezní kmitočet 0,47 · fs (původní), zisk 4
        let n = OS_TAPS_PER_PHASE * 4;
        let beta = 10.0f64;
        let i0 = |x: f64| { let mut s = 1.0; let mut t = 1.0; for k in 1..30 { t *= (x / 2.0) / k as f64; s += t * t; } s };
        let fc = 0.495 / 4.0;
        let mid = (n as f64 - 1.0) / 2.0;
        let mut h = vec![0f64; n];
        for (i, v) in h.iter_mut().enumerate() {
            let x = i as f64 - mid;
            let sinc = if x.abs() < 1e-9 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
            let w = i0(beta * (1.0 - (2.0 * i as f64 / (n as f64 - 1.0) - 1.0).powi(2)).max(0.0).sqrt()) / i0(beta);
            *v = sinc * w * 4.0;
        }
        let taps = (0..OS_TAPS_PER_PHASE).map(|k| [h[k * 4] as f32, h[k * 4 + 1] as f32, h[k * 4 + 2] as f32, h[k * 4 + 3] as f32]).collect();
        Self { taps, hist: [vec![0.0; OS_TAPS_PER_PHASE], vec![0.0; OS_TAPS_PER_PHASE]], pos: 0 }
    }
    /// Zpoždění detekce ve vzorcích (původní frekvence).
    pub fn delay() -> usize { (OS_TAPS_PER_PHASE * 4 - 1) / 8 }
    /// Vloží vzorek (L, R) a vrátí max |hodnotu| ze 4 interpolovaných fází pro oba kanály.
    #[inline]
    pub fn push_peak(&mut self, l: f32, r: f32) -> f32 {
        let n = OS_TAPS_PER_PHASE;
        self.pos = if self.pos == 0 { n - 1 } else { self.pos - 1 };
        self.hist[0][self.pos] = l;
        self.hist[1][self.pos] = r;
        let mut peak = 0f32;
        for ch in 0..2 {
            let mut acc = [0f32; 4];
            let h = &self.hist[ch];
            for (k, t) in self.taps.iter().enumerate() {
                let x = h[(self.pos + k) % n];
                acc[0] += t[0] * x; acc[1] += t[1] * x; acc[2] += t[2] * x; acc[3] += t[3] * x;
            }
            for a in acc { peak = peak.max(a.abs()); }
        }
        peak
    }
    pub fn reset(&mut self) { for h in self.hist.iter_mut() { h.iter_mut().for_each(|x| *x = 0.0); } }
}

impl Default for Oversampler4 { fn default() -> Self { Self::new() } }

const MARGIN: usize = 32;
const MARGIN_DB: f32 = 0.2;

pub struct TruePeakLimiter {
    pub ceiling_lin: f32,
    release_coef: f32,
    look: usize,
    os: Oversampler4,
    // zpožďovací linka audia
    dl: Vec<[f32; 2]>,
    dl_pos: usize,
    // klouzavé minimum požadovaného zisku (monotónní fronta v kruhovém poli)
    mq_val: Vec<f32>,
    mq_idx: Vec<u64>,
    mq_head: usize,
    mq_len: usize,
    // klouzavý průměr (box filtr)
    box_buf: Vec<f32>,
    box_pos: usize,
    box_sum: f64,
    n: u64,
    g: f32,
    pub gr_db: f32,
    pub max_gr_db: f32,
}

impl TruePeakLimiter {
    /// `lookahead_ms` je pevné (latence se nemění při změně parametrů).
    pub fn new(fs: f32, ceiling_dbtp: f32, release_ms: f32, lookahead_ms: f32) -> Self {
        let look = ((lookahead_ms / 1000.0 * fs) as usize).max(16);
        // min-hold je o MARGIN delší než box filtr → tolerance zarovnání ±MARGIN/2 vzorků kolem zpoždění detektoru
        let dl_len = look + MARGIN;
        let mut l = Self {
            ceiling_lin: 1.0, release_coef: 0.0, look, os: Oversampler4::new(),
            dl: vec![[0.0; 2]; dl_len], dl_pos: 0,
            mq_val: vec![1.0; look + MARGIN + 2], mq_idx: vec![0; look + MARGIN + 2], mq_head: 0, mq_len: 0,
            box_buf: vec![1.0; look], box_pos: 0, box_sum: look as f64, n: 0, g: 1.0, gr_db: 0.0, max_gr_db: 0.0,
        };
        l.set_params(fs, ceiling_dbtp, release_ms);
        l
    }
    pub fn set_params(&mut self, fs: f32, ceiling_dbtp: f32, release_ms: f32) {
        // vnitřní rezerva 0,2 dB pro odchylku interpolátoru od referenčního měřiče BS.1770
        self.ceiling_lin = 10f32.powf((ceiling_dbtp.clamp(-12.0, 0.0) - MARGIN_DB) / 20.0);
        self.release_coef = 1.0 - (-1.0 / (release_ms.clamp(5.0, 2000.0) / 1000.0 * fs)).exp();
    }
    /// Celková latence ve vzorcích.
    pub fn latency(&self) -> usize { self.dl.len() - 1 }

    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        // 1) požadovaný zisk podle true peaku vstupu (detekce je zpožděná o delay() vzorků)
        let pk = self.os.push_peak(l, r);
        let req = if pk > self.ceiling_lin { self.ceiling_lin / pk } else { 1.0 };
        // 2) klouzavé minimum přes okno `look`
        let cap = self.mq_val.len();
        let n = self.n;
        while self.mq_len > 0 {
            let last = (self.mq_head + self.mq_len - 1) % cap;
            if self.mq_val[last] >= req { self.mq_len -= 1; } else { break; }
        }
        let slot = (self.mq_head + self.mq_len) % cap;
        self.mq_val[slot] = req; self.mq_idx[slot] = n; self.mq_len += 1;
        while self.mq_len > 0 && self.mq_idx[self.mq_head] + ((self.look + MARGIN) as u64) <= n { self.mq_head = (self.mq_head + 1) % cap; self.mq_len -= 1; }
        let mn = if self.mq_len > 0 { self.mq_val[self.mq_head] } else { 1.0 };
        // 3) box filtr délky `look` → plynulý náběh, který dosáhne minima přesně v čase špičky
        self.box_sum += mn as f64 - self.box_buf[self.box_pos] as f64;
        self.box_buf[self.box_pos] = mn;
        self.box_pos = (self.box_pos + 1) % self.look;
        let avg = (self.box_sum / self.look as f64) as f32;
        let target = avg;
        // 4) návrat (release) jen směrem nahoru
        self.g = if target < self.g { target } else { self.g + (target - self.g) * self.release_coef };
        self.n += 1;
        // 5) zpožděný signál × zisk
        let len = self.dl.len();
        self.dl[self.dl_pos] = [l, r];
        self.dl_pos = (self.dl_pos + 1) % len;
        let out = self.dl[self.dl_pos];
        let c = self.ceiling_lin * 10f32.powf(MARGIN_DB / 20.0); // pojistka na vzorek (sample peak ≤ strop)
        ((out[0] * self.g).clamp(-c, c), (out[1] * self.g).clamp(-c, c))
    }

    /// Po bloku: aktualizace měření redukce.
    pub fn end_block(&mut self) {
        self.gr_db = -20.0 * self.g.max(1e-6).log10();
        self.max_gr_db = self.max_gr_db.max(self.gr_db);
    }
}
