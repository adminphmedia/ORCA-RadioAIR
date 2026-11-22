//! Vysílací zvukový processing ORCA RadioAIR.
//!
//! Programový mix → společný master řetězec (vstupní trim → HPF/DC → AGC → EQ → 4pásmová dynamika →
//! de-esser → šířka sterea) → rozvětvení → výstupní větve (převzorkování → trim → true-peak limiter) → enkodéry.

pub mod dynamics;
pub mod filters;
pub mod limiter;
pub mod params;

use dynamics::{db_to_lin, Agc, BandComp, DeEsser};
use filters::{Biquad, Coefs, Crossover4};
use limiter::TruePeakLimiter;
use params::{BranchConf, ProcParams};
use rubato::Resampler;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Sdílené měření redukcí (zapisuje DSP po blocích, čte API) – bez zámků.
#[derive(Default)]
pub struct ProcStats {
    pub agc_gain_db: AtomicU32,
    pub agc_gated: AtomicU32,
    pub band_gr_db: [AtomicU32; 4],
    pub deess_gr_db: AtomicU32,
    pub cpu_us_last: AtomicU32,
    pub cpu_us_max: AtomicU32,
    pub blocks: AtomicU64,
    pub errors: AtomicU64,
    pub clip_samples: AtomicU64,
}
fn put(a: &AtomicU32, v: f32) { a.store(v.to_bits(), Ordering::Relaxed); }
pub fn get(a: &AtomicU32) -> f32 { f32::from_bits(a.load(Ordering::Relaxed)) }

pub fn stats() -> &'static Arc<ProcStats> {
    static S: OnceLock<Arc<ProcStats>> = OnceLock::new();
    S.get_or_init(|| Arc::new(ProcStats::default()))
}

/// Master řetězec (zvuk stanice). Zpracování v reálném čase bez alokací.
pub struct MasterChain {
    pub params: ProcParams,
    fs: f32,
    in_gain: f32,
    hpf: [Biquad; 2],
    agc: Agc,
    eq: Vec<Biquad>,
    xo: Crossover4,
    comps: [BandComp; 4],
    deess: DeEsser,
    width: f32,
    pub generation: u64,
}

impl MasterChain {
    pub fn new(mut params: ProcParams, fs: u32, generation: u64) -> Self {
        let fs_f = fs as f32;
        params.validate(fs_f);
        let f = fs as f64;
        let hpf_c = if params.hpf.enabled { Coefs::highpass(params.hpf.freq_hz as f64, std::f64::consts::FRAC_1_SQRT_2, f) } else { Coefs::IDENTITY };
        let mut eq = vec![];
        let e = &params.eq;
        if e.low_shelf.enabled && e.low_shelf.gain_db.abs() > 0.01 { eq.push(Biquad::new(Coefs::low_shelf(e.low_shelf.freq_hz as f64, e.low_shelf.q as f64, e.low_shelf.gain_db as f64, f))); }
        for b in e.bands.iter().filter(|b| b.enabled && b.gain_db.abs() > 0.01) { eq.push(Biquad::new(Coefs::peaking(b.freq_hz as f64, b.q as f64, b.gain_db as f64, f))); }
        if e.high_shelf.enabled && e.high_shelf.gain_db.abs() > 0.01 { eq.push(Biquad::new(Coefs::high_shelf(e.high_shelf.freq_hz as f64, e.high_shelf.q as f64, e.high_shelf.gain_db as f64, f))); }
        let xo = Crossover4::new([params.mb.xover_hz[0] as f64, params.mb.xover_hz[1] as f64, params.mb.xover_hz[2] as f64], f);
        let comps = [0, 1, 2, 3].map(|i| BandComp::new(&params.mb.bands[i], fs_f));
        Self {
            in_gain: db_to_lin(params.input_gain_db), hpf: [Biquad::new(hpf_c), Biquad::new(hpf_c)], agc: Agc::new(&params.agc, fs_f), eq, xo, comps,
            deess: DeEsser::new(&params.deesser, fs_f), width: params.stereo.width_pct / 100.0, fs: fs_f, params, generation,
        }
    }

    /// Převezme stav AGC a kompresorů z předchozí instance (plynulé přepnutí bez skoku hlasitosti).
    pub fn warm_from(&mut self, old: &MasterChain) {
        self.agc.copy_state(&old.agc);
        for (n, o) in self.comps.iter_mut().zip(old.comps.iter()) { n.copy_state(o); }
    }

    pub fn sample_rate(&self) -> f32 { self.fs }

    /// Zpracuje blok prokládaného sterea na místě.
    pub fn process(&mut self, buf: &mut [f32]) {
        let g = self.in_gain;
        for fr in buf.chunks_exact_mut(2) {
            let a = self.hpf[0].tick(0, (fr[0] * g) as f64); fr[0] = self.hpf[1].tick(0, a) as f32;
            let b = self.hpf[0].tick(1, (fr[1] * g) as f64); fr[1] = self.hpf[1].tick(1, b) as f32;
        }
        self.agc.process(buf);
        let mb = self.params.mb.enabled;
        let ds = self.params.deesser.enabled;
        for fr in buf.chunks_exact_mut(2) {
            let (mut l, mut r) = (fr[0] as f64, fr[1] as f64);
            for q in self.eq.iter_mut() { l = q.tick(0, l); r = q.tick(1, r); }
            if mb {
                let bl = self.xo.split(0, l);
                let br = self.xo.split(1, r);
                l = 0.0; r = 0.0;
                for i in 0..4 {
                    let gg = self.comps[i].gain(bl[i] as f32, br[i] as f32) as f64;
                    l += bl[i] * gg; r += br[i] * gg;
                }
            }
            let (mut lf, mut rf) = (l as f32, r as f32);
            if ds { let o = self.deess.process(lf, rf); lf = o.0; rf = o.1; }
            if (self.width - 1.0).abs() > 1e-3 {
                let m = 0.5 * (lf + rf); let s = 0.5 * (lf - rf) * self.width;
                lf = m + s; rf = m - s;
            }
            fr[0] = lf; fr[1] = rf;
        }
        let st = stats();
        put(&st.agc_gain_db, self.agc.gain_db);
        st.agc_gated.store(self.agc.gated as u32, Ordering::Relaxed);
        for i in 0..4 { put(&st.band_gr_db[i], if mb { self.comps[i].gr_db } else { 0.0 }); }
        put(&st.deess_gr_db, if ds { self.deess.gr_db } else { 0.0 });
    }
}

// ---------------------------------------------------------------------------------------------
// Výstupní větev

/// Výstupní profil: (převzorkování) → trim → true-peak limiter. Latence je pevná.
pub struct Branch {
    pub conf: BranchConf,
    pub fs_in: u32,
    pub fs_out: u32,
    rs: Option<(rubato::FftFixedIn<f32>, [Vec<f32>; 2], Vec<Vec<f32>>)>,
    trim: f32,
    trim_target: f32,
    lim: TruePeakLimiter,
    pub key: String,
    pub bypass_limiter: bool,
}

impl Branch {
    pub fn new(conf: BranchConf, fs_in: u32, fs_out_hint: u32) -> Self {
        let mut conf = conf;
        conf.validate();
        let fs_out = if conf.sample_rate > 0 { conf.sample_rate } else if fs_out_hint > 0 { fs_out_hint } else { fs_in };
        let rs: Option<(rubato::FftFixedIn<f32>, [Vec<f32>; 2], Vec<Vec<f32>>)> = if fs_out != fs_in {
            match rubato::FftFixedIn::<f32>::new(fs_in as usize, fs_out as usize, 480, 2, 2) {
                Ok(r) => { let ob = r.output_buffer_allocate(true); Some((r, [Vec::with_capacity(2048), Vec::with_capacity(2048)], ob)) }
                Err(e) => { log::error!("převzorkování větve: {e}"); None }
            }
        } else { None };
        let fs_out = if rs.is_some() { fs_out } else { fs_in };
        let lim = TruePeakLimiter::new(fs_out as f32, conf.ceiling_dbtp, conf.release_ms, 5.0);
        let t = db_to_lin(conf.trim_db);
        Self { key: format!("branch:{}", conf.target), conf, fs_in, fs_out, rs, trim: t, trim_target: t, lim, bypass_limiter: false }
    }

    /// Změna bezpečných parametrů zaživa (trim plynule, strop a release). Vrací false, když je nutný restart (jiná frekvence).
    pub fn set_conf(&mut self, mut c: BranchConf) -> bool {
        c.validate();
        let want_fs = if c.sample_rate > 0 { c.sample_rate } else { self.fs_out };
        if want_fs != self.fs_out { return false; }
        self.trim_target = db_to_lin(c.trim_db);
        self.lim.set_params(self.fs_out as f32, c.ceiling_dbtp, c.release_ms);
        self.conf = c;
        true
    }

    /// Latence větve v ms (převzorkování + lookahead limiteru).
    pub fn latency_ms(&self) -> f32 {
        let rs = self.rs.as_ref().map(|(r, _, _)| r.output_delay() as f32 / self.fs_out as f32 * 1000.0).unwrap_or(0.0);
        rs + self.lim.latency() as f32 / self.fs_out as f32 * 1000.0
    }

    pub fn limiter_gr_db(&self) -> f32 { self.lim.gr_db }

    /// Zpracuje blok (prokládané stereo, fs_in) a připíše výstup (fs_out) do `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let start = out.len();
        if let Some((rs, acc, ob)) = self.rs.as_mut() {
            for fr in input.chunks_exact(2) { acc[0].push(fr[0]); acc[1].push(fr[1]); }
            loop {
                let need = rs.input_frames_next();
                if acc[0].len() < need { break; }
                let inp = [&acc[0][..need], &acc[1][..need]];
                match rs.process_into_buffer(&inp, ob, None) {
                    Ok((_, n_out)) => { for i in 0..n_out { out.push(ob[0][i]); out.push(ob[1][i]); } }
                    Err(e) => { log::error!("převzorkování: {e}"); }
                }
                acc[0].drain(..need); acc[1].drain(..need);
            }
        } else {
            out.extend_from_slice(input);
        }
        let frames = (out.len() - start) / 2;
        if frames == 0 { return; }
        let (t0, t1) = (self.trim, self.trim_target);
        for (i, fr) in out[start..].chunks_exact_mut(2).enumerate() {
            let t = t0 + (t1 - t0) * ((i + 1) as f32 / frames as f32);
            let (l, r) = (fr[0] * t, fr[1] * t);
            if self.bypass_limiter { fr[0] = l; fr[1] = r; } else { let (a, b) = self.lim.process(l, r); fr[0] = a; fr[1] = b; }
        }
        self.trim = t1;
        self.lim.end_block();
    }
}

// ---------------------------------------------------------------------------------------------
// Měření (EBU R128 / ITU-R BS.1770): sample peak, true peak, M, S, I, LRA

#[derive(Debug, Clone, Serialize, Default)]
pub struct MeterSnap {
    pub key: String,
    pub sample_rate: u32,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
    /// Max. true peak od resetu
    pub true_peak_max_dbtp: f64,
    pub momentary_lufs: f64,
    pub shortterm_lufs: f64,
    pub integrated_lufs: f64,
    pub lra_lu: Option<f64>,
    pub since: String,
    pub seconds: f64,
    pub limiter_gr_db: Option<f32>,
    pub latency_ms: Option<f32>,
    pub pending: Option<String>,
}

pub struct Meter {
    ebu: ebur128::EbuR128,
    pub snap: MeterSnap,
    frames: u64,
    since_update: u64,
    fs: u32,
    peak_win: (f64, f64, u64),
}

fn todb(x: f64) -> f64 { if x > 0.0 { 20.0 * x.log10() } else { -120.0 } }

impl Meter {
    pub fn new(key: &str, fs: u32) -> Self {
        use ebur128::Mode;
        let ebu = ebur128::EbuR128::new(2, fs, Mode::M | Mode::S | Mode::I | Mode::LRA | Mode::TRUE_PEAK | Mode::SAMPLE_PEAK).expect("ebur128");
        Self { ebu, snap: MeterSnap { key: key.into(), sample_rate: fs, since: chrono::Local::now().to_rfc3339(), true_peak_max_dbtp: -120.0, ..Default::default() }, frames: 0, since_update: 0, fs, peak_win: (0.0, 0.0, 0) }
    }
    pub fn reset(&mut self) { *self = Meter::new(&self.snap.key.clone(), self.fs); }
    pub fn frames_fed(&self) -> u64 { self.frames }
    pub fn feed(&mut self, buf: &[f32]) {
        if buf.is_empty() { return; }
        let _ = self.ebu.add_frames_f32(buf);
        let n = (buf.len() / 2) as u64;
        self.frames += n;
        self.since_update += n;
        let sp = self.ebu.prev_sample_peak(0).unwrap_or(0.0).max(self.ebu.prev_sample_peak(1).unwrap_or(0.0));
        let tp = self.ebu.prev_true_peak(0).unwrap_or(0.0).max(self.ebu.prev_true_peak(1).unwrap_or(0.0));
        self.peak_win.0 = self.peak_win.0.max(sp);
        self.peak_win.1 = self.peak_win.1.max(tp);
        self.peak_win.2 += n;
        self.snap.true_peak_max_dbtp = self.snap.true_peak_max_dbtp.max(todb(tp));
        if self.since_update >= self.fs as u64 / 10 {
            self.since_update = 0;
            let s = &mut self.snap;
            s.momentary_lufs = self.ebu.loudness_momentary().unwrap_or(f64::NEG_INFINITY).max(-120.0);
            s.shortterm_lufs = self.ebu.loudness_shortterm().unwrap_or(f64::NEG_INFINITY).max(-120.0);
            s.integrated_lufs = self.ebu.loudness_global().unwrap_or(f64::NEG_INFINITY).max(-120.0);
            s.seconds = self.frames as f64 / self.fs as f64;
            // LRA až po dostatečném intervalu (min. 60 s)
            s.lra_lu = if s.seconds >= 60.0 { self.ebu.loudness_range().ok() } else { None };
            if self.peak_win.2 >= self.fs as u64 / 2 {
                s.sample_peak_dbfs = todb(self.peak_win.0);
                s.true_peak_dbtp = todb(self.peak_win.1);
                self.peak_win = (0.0, 0.0, 0);
            }
        }
    }
}

/// Registr měření (input, master, branch:*), čte API.
pub fn meters() -> &'static Mutex<HashMap<String, MeterSnap>> {
    static M: OnceLock<Mutex<HashMap<String, MeterSnap>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
pub fn publish(s: &MeterSnap) { meters().lock().insert(s.key.clone(), s.clone()); }

/// Požadavky na reset měřičů (generace).
pub fn meter_reset_gen() -> &'static AtomicU64 { static G: AtomicU64 = AtomicU64::new(0); &G }

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------------------------
// Běhové nastavení výstupních větví pro enkodéry (čtou ho vlákna streamů)

#[derive(Debug, Clone, Default)]
pub struct Runtime {
    /// Master processing je aktivní (signál do streamů není soft-clipnutý)
    pub enabled: bool,
    /// Servisní bypass celé cesty včetně výstupní ochrany
    pub bypass: bool,
    pub branches: Vec<BranchConf>,
}

pub fn runtime() -> &'static parking_lot::RwLock<Runtime> {
    static R: OnceLock<parking_lot::RwLock<Runtime>> = OnceLock::new();
    R.get_or_init(|| parking_lot::RwLock::new(Runtime::default()))
}
pub fn runtime_gen() -> &'static AtomicU64 { static G: AtomicU64 = AtomicU64::new(0); &G }

/// Větev pro daný výstup (podle běhového nastavení), nebo None.
pub fn branch_for(target: &str, fs_in: u32, fs_out_hint: u32) -> Option<Branch> {
    let rt = runtime().read();
    if rt.bypass { return None; }
    rt.branches.iter().find(|b| b.target == target && b.enabled).map(|b| Branch::new(b.clone(), fs_in, fs_out_hint))
}
