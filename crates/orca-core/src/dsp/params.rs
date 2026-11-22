//! Parametry vysílacího processingu, validace rozsahů, mapování „Intenzity“ a tovární presety ORCA.

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const FACTORY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HpfParams { pub enabled: bool, pub freq_hz: f32 }
impl Default for HpfParams { fn default() -> Self { Self { enabled: true, freq_hz: 30.0 } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgcParams {
    pub enabled: bool,
    /// Cílová úroveň RMS detektoru (dBFS, ne LUFS)
    pub target_dbfs: f32,
    pub max_boost_db: f32,
    pub max_cut_db: f32,
    /// Rychlost snižování zisku (dB/s)
    pub attack_db_s: f32,
    /// Rychlost návratu / zvyšování zisku (dB/s)
    pub release_db_s: f32,
    /// Pod touto úrovní (dBFS) se zisk nezvyšuje
    pub gate_dbfs: f32,
    /// Prodleva před zvyšováním zisku po tichu (ms)
    pub hold_ms: f32,
    /// Okno RMS detektoru (ms)
    pub window_ms: f32,
}
impl Default for AgcParams {
    fn default() -> Self { Self { enabled: true, target_dbfs: -20.0, max_boost_db: 6.0, max_cut_db: 8.0, attack_db_s: 3.0, release_db_s: 1.0, gate_dbfs: -45.0, hold_ms: 1500.0, window_ms: 1500.0 } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EqBand { pub enabled: bool, pub freq_hz: f32, pub gain_db: f32, pub q: f32 }
impl Default for EqBand { fn default() -> Self { Self { enabled: false, freq_hz: 1000.0, gain_db: 0.0, q: 1.0 } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EqParams { pub low_shelf: EqBand, pub high_shelf: EqBand, pub bands: Vec<EqBand> }
impl Default for EqParams {
    fn default() -> Self {
        Self { low_shelf: EqBand { enabled: true, freq_hz: 100.0, gain_db: 0.0, q: 0.707 }, high_shelf: EqBand { enabled: true, freq_hz: 8000.0, gain_db: 0.0, q: 0.707 },
            bands: vec![EqBand { freq_hz: 250.0, ..Default::default() }, EqBand { freq_hz: 2500.0, ..Default::default() }, EqBand { freq_hz: 6000.0, ..Default::default() }] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BandParams { pub threshold_dbfs: f32, pub ratio: f32, pub attack_ms: f32, pub release_ms: f32, pub knee_db: f32, pub max_gr_db: f32, pub makeup_db: f32 }
impl Default for BandParams { fn default() -> Self { Self { threshold_dbfs: -18.0, ratio: 2.0, attack_ms: 20.0, release_ms: 250.0, knee_db: 6.0, max_gr_db: 8.0, makeup_db: 0.0 } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MbParams { pub enabled: bool, pub xover_hz: [f32; 3], pub bands: [BandParams; 4] }
impl Default for MbParams { fn default() -> Self { Self { enabled: true, xover_hz: [120.0, 1000.0, 5000.0], bands: Default::default() } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DeesserParams { pub enabled: bool, pub freq_hz: f32, pub threshold_dbfs: f32, pub max_gr_db: f32 }
impl Default for DeesserParams { fn default() -> Self { Self { enabled: false, freq_hz: 6000.0, threshold_dbfs: -28.0, max_gr_db: 3.0 } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct StereoParams { pub width_pct: f32 }
impl Default for StereoParams { fn default() -> Self { Self { width_pct: 100.0 } } }

/// Kompletní preset zvuku stanice (master sběrnice).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProcParams {
    pub schema: u32,
    pub input_gain_db: f32,
    pub hpf: HpfParams,
    pub agc: AgcParams,
    pub eq: EqParams,
    pub mb: MbParams,
    pub deesser: DeesserParams,
    pub stereo: StereoParams,
}
impl Default for ProcParams {
    fn default() -> Self { Self { schema: SCHEMA_VERSION, input_gain_db: 0.0, hpf: Default::default(), agc: Default::default(), eq: Default::default(), mb: Default::default(), deesser: Default::default(), stereo: Default::default() } }
}

/// Preset (tovární nebo vlastní).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub description: String,
    /// mírná | střední | silná | ochrana
    pub intensity_label: String,
    pub usage: String,
    pub reference: String,
    pub factory: bool,
    pub version: u32,
    /// Bez dynamiky – ovladač Intenzita se nezobrazuje
    pub transparent: bool,
    pub params: ProcParams,
}
impl Default for Preset {
    fn default() -> Self { Self { id: String::new(), name: String::new(), description: String::new(), intensity_label: String::new(), usage: String::new(), reference: String::new(), factory: false, version: FACTORY_VERSION, transparent: false, params: ProcParams::default() } }
}

/// Jednoduché ovladače (základní režim).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BasicControls {
    /// 0–100, výchozí 50
    pub intensity: u8,
    /// Basy ±6 dB (low shelf 100 Hz)
    pub bass_db: f32,
    /// Jas ±6 dB (high shelf 8 kHz)
    pub brightness_db: f32,
    /// Vstupní zisk ±12 dB
    pub input_gain_db: f32,
}
impl Default for BasicControls { fn default() -> Self { Self { intensity: 50, bass_db: 0.0, brightness_db: 0.0, input_gain_db: 0.0 } } }

fn clampf(v: &mut f32, lo: f32, hi: f32, name: &str, errs: &mut Vec<String>) {
    if !v.is_finite() { errs.push(format!("{name}: neplatné číslo")); *v = lo; return; }
    if *v < lo || *v > hi { errs.push(format!("{name}: {v} mimo rozsah {lo}…{hi}")); *v = v.clamp(lo, hi); }
}

impl ProcParams {
    /// Kontrola rozsahů. Vrací chyby (hodnoty se zároveň omezí do bezpečných mezí).
    pub fn validate(&mut self, fs: f32) -> Vec<String> {
        let mut e = vec![];
        if self.schema > SCHEMA_VERSION { e.push(format!("nepodporovaná verze schématu {} (podporováno {SCHEMA_VERSION})", self.schema)); }
        let nyq = fs * 0.45;
        clampf(&mut self.input_gain_db, -12.0, 12.0, "vstupní zisk", &mut e);
        clampf(&mut self.hpf.freq_hz, 15.0, 150.0, "HPF", &mut e);
        let a = &mut self.agc;
        clampf(&mut a.target_dbfs, -36.0, -8.0, "AGC cíl", &mut e);
        clampf(&mut a.max_boost_db, 0.0, 18.0, "AGC max. zesílení", &mut e);
        clampf(&mut a.max_cut_db, 0.0, 18.0, "AGC max. zeslabení", &mut e);
        clampf(&mut a.attack_db_s, 0.2, 30.0, "AGC reakce", &mut e);
        clampf(&mut a.release_db_s, 0.1, 10.0, "AGC návrat", &mut e);
        clampf(&mut a.gate_dbfs, -80.0, -25.0, "AGC gate", &mut e);
        clampf(&mut a.hold_ms, 0.0, 10000.0, "AGC hold", &mut e);
        clampf(&mut a.window_ms, 100.0, 5000.0, "AGC okno", &mut e);
        for (n, b) in [("low shelf", &mut self.eq.low_shelf), ("high shelf", &mut self.eq.high_shelf)] {
            clampf(&mut b.freq_hz, 20.0, nyq, n, &mut e); clampf(&mut b.gain_db, -6.0, 6.0, n, &mut e); clampf(&mut b.q, 0.3, 2.0, n, &mut e);
        }
        if self.eq.bands.len() > 5 { e.push("max. 5 parametrických pásem".into()); self.eq.bands.truncate(5); }
        for b in self.eq.bands.iter_mut() { clampf(&mut b.freq_hz, 20.0, nyq, "EQ pásmo", &mut e); clampf(&mut b.gain_db, -6.0, 6.0, "EQ zisk", &mut e); clampf(&mut b.q, 0.3, 6.0, "EQ Q", &mut e); }
        let x = &mut self.mb.xover_hz;
        clampf(&mut x[0], 60.0, 300.0, "dělení 1", &mut e); clampf(&mut x[1], 400.0, 3000.0, "dělení 2", &mut e); clampf(&mut x[2], 2500.0, nyq.min(12000.0), "dělení 3", &mut e);
        for b in self.mb.bands.iter_mut() {
            clampf(&mut b.threshold_dbfs, -40.0, 0.0, "práh", &mut e); clampf(&mut b.ratio, 1.0, 8.0, "poměr", &mut e);
            clampf(&mut b.attack_ms, 0.5, 300.0, "attack", &mut e); clampf(&mut b.release_ms, 20.0, 5000.0, "release", &mut e);
            clampf(&mut b.knee_db, 0.0, 12.0, "knee", &mut e); clampf(&mut b.max_gr_db, 0.0, 18.0, "max. redukce", &mut e); clampf(&mut b.makeup_db, -6.0, 12.0, "makeup", &mut e);
        }
        clampf(&mut self.deesser.freq_hz, 3000.0, nyq.min(12000.0), "de-esser frekvence", &mut e);
        clampf(&mut self.deesser.threshold_dbfs, -50.0, -6.0, "de-esser práh", &mut e);
        clampf(&mut self.deesser.max_gr_db, 0.0, 10.0, "de-esser max.", &mut e);
        clampf(&mut self.stereo.width_pct, 0.0, 150.0, "šířka sterea", &mut e);
        e
    }
}

/// Deterministické mapování Intenzity (0–100, základ 50) a jednoduchých ovladačů na parametry presetu.
/// Intenzita mění jen AGC a vícepásmovou kompresi; true-peak strop ani výstupní cíle nemění.
pub fn effective(preset: &Preset, b: &BasicControls) -> ProcParams {
    let mut p = preset.params.clone();
    p.input_gain_db += b.input_gain_db.clamp(-12.0, 12.0);
    if !preset.transparent {
        let k = (b.intensity.min(100) as f32 - 50.0) / 50.0; // −1 … +1
        // AGC: rezerva zesílení ×(0,5 … 1,5)
        p.agc.max_boost_db = (p.agc.max_boost_db * (1.0 + 0.5 * k)).clamp(0.0, 18.0);
        for band in p.mb.bands.iter_mut() {
            // poměr: (r−1)·(0,5 … 1,5) + 1, práh −/+ 4 dB, max. redukce ×(0,7 … 1,3)
            band.ratio = (1.0 + (band.ratio - 1.0) * (1.0 + 0.5 * k)).clamp(1.0, 6.0);
            band.threshold_dbfs = (band.threshold_dbfs - 4.0 * k).clamp(-40.0, 0.0);
            band.max_gr_db = (band.max_gr_db * (1.0 + 0.3 * k)).clamp(0.0, 18.0);
        }
    }
    p.eq.low_shelf.enabled = true;
    p.eq.low_shelf.gain_db = (p.eq.low_shelf.gain_db + b.bass_db.clamp(-6.0, 6.0)).clamp(-6.0, 6.0);
    p.eq.high_shelf.enabled = true;
    p.eq.high_shelf.gain_db = (p.eq.high_shelf.gain_db + b.brightness_db.clamp(-6.0, 6.0)).clamp(-6.0, 6.0);
    p
}

/// Výstupní větev (profil výstupu): trim, true-peak strop, převzorkování.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BranchConf {
    /// "output" = zvuková karta, jinak id encoderu
    pub target: String,
    /// dab | icecast | pcm | pcm23
    pub template: String,
    pub enabled: bool,
    pub trim_db: f32,
    pub ceiling_dbtp: f32,
    pub release_ms: f32,
    /// Referenční hlasitost pro kalibraci a měření (LUFS) – nereguluje se k ní
    pub ref_lufs: f32,
    /// Převzorkování v ORCA před limiterem (0 = podle enkodéru / beze změny)
    pub sample_rate: u32,
}
impl Default for BranchConf { fn default() -> Self { template("icecast", "") } }

pub fn template(t: &str, target: &str) -> BranchConf {
    let (ceil, refl) = match t { "dab" => (-2.0, -18.0), "pcm" => (-1.0, -18.0), "pcm23" => (-1.0, -23.0), _ => (-2.0, -18.0) };
    BranchConf { target: target.into(), template: t.into(), enabled: true, trim_db: 0.0, ceiling_dbtp: ceil, release_ms: 150.0, ref_lufs: refl, sample_rate: if t == "dab" { 48000 } else { 0 } }
}

impl BranchConf {
    pub fn validate(&mut self) -> Vec<String> {
        let mut e = vec![];
        clampf(&mut self.trim_db, -12.0, 6.0, "trim", &mut e);
        clampf(&mut self.ceiling_dbtp, -9.0, 0.0, "true-peak strop", &mut e);
        clampf(&mut self.release_ms, 10.0, 1000.0, "release limiteru", &mut e);
        clampf(&mut self.ref_lufs, -31.0, -10.0, "referenční hlasitost", &mut e);
        if ![0, 32000, 44100, 48000].contains(&self.sample_rate) { e.push(format!("nepodporovaná vzorkovací frekvence {}", self.sample_rate)); self.sample_rate = 0; }
        e
    }
}

// ---------------------------------------------------------------------------------------------
// Tovární presety (verzovaná data)

pub fn factory_presets() -> Vec<Preset> {
    serde_json::from_str::<Vec<Preset>>(include_str!("../../assets/processing/factory_presets_v1.json")).expect("tovární presety")
        .into_iter().map(|mut p| { p.factory = true; p.version = FACTORY_VERSION; p }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn factory_presets_valid() {
        let v = factory_presets();
        assert_eq!(v.len(), 12);
        for mut p in v {
            let errs = p.params.validate(48000.0);
            assert!(errs.is_empty(), "{}: {errs:?}", p.id);
            let e = effective(&p, &BasicControls::default());
            // základ intenzity 50 = beze změny dynamiky
            assert_eq!(e.mb, p.params.mb, "{}", p.id);
            assert_eq!(e.agc, p.params.agc, "{}", p.id);
        }
    }
    #[test]
    fn intensity_monotonic() {
        let p = factory_presets().into_iter().find(|p| p.id == "energic-chr").unwrap();
        let lo = effective(&p, &BasicControls { intensity: 0, ..Default::default() });
        let hi = effective(&p, &BasicControls { intensity: 100, ..Default::default() });
        assert!(hi.mb.bands[2].ratio > lo.mb.bands[2].ratio);
        assert!(hi.mb.bands[2].threshold_dbfs < lo.mb.bands[2].threshold_dbfs);
        assert!(hi.agc.max_boost_db > lo.agc.max_boost_db);
    }
}
