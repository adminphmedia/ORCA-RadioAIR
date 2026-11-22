//! Audioanalýza pro Selector: tempo (BPM) a energie skladby. Běží jako zrušitelná dávka mimo playout
//! (vlákno s nízkou prioritou), výsledky jsou označené původem a jistotou a nikdy nepřepíšou ruční ani importované hodnoty.
//!
//! * Tempo: obálka nástupů (rozdíl logaritmické energie krátkých oken, kladná část) → autokorelace pro 60–200 BPM
//!   s váhou preferující 80–160 BPM (zamezení dvojnásobku / polovině) → parabolické zpřesnění vrcholu.
//! * Energie: hrubá hodnota = hlasitost (RMS dBFS) + hustota nástupů + „jas“ (podíl vysokých frekvencí přes první diferenci);
//!   na stupnici 1–5 se převádí podle kvintilů celé knihovny (relativní energie v rámci stanice).

use super::attrs::{self, MusicAttrs};
use crate::db::Db;
use crate::decode::AudioDecoder;
use crate::model::TrackKind;
use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

const RATE: f64 = 11_025.0;
const HOP: usize = 128; // ≈ 86 snímků/s
const MAX_SECONDS: f64 = 150.0;
const SKIP_SECONDS: f64 = 15.0;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Features { pub bpm: Option<f64>, pub bpm_confidence: f64, pub energy_raw: f64, pub loudness_db: f64, pub onset_rate: f64, pub brightness: f64 }

/// Mono signál ~11 kHz z části skladby (přeskočí úvod, max. 150 s).
fn load_mono(path: &Path) -> Result<Vec<f32>> {
    let mut dec = AudioDecoder::open(path)?;
    let sr = dec.sample_rate as f64;
    let step = (sr / RATE).max(1.0);
    let skip = (SKIP_SECONDS * sr) as u64;
    let max = ((MAX_SECONDS + SKIP_SECONDS) * sr) as u64;
    let mut buf = vec![];
    let mut out = Vec::with_capacity((MAX_SECONDS * RATE) as usize);
    let (mut n, mut acc, mut cnt, mut next) = (0u64, 0f32, 0f32, 0f64);
    let mut all = Vec::new();
    while dec.next_stereo(&mut buf)? {
        for fr in buf.chunks_exact(2) {
            let m = (fr[0] + fr[1]) * 0.5;
            acc += m; cnt += 1.0;
            if (n as f64) >= next { all.push(acc / cnt); acc = 0.0; cnt = 0.0; next += step; }
            n += 1;
        }
        if n > max { break; }
    }
    // krátké skladby (jingly) bez přeskakování úvodu
    let skip_s = if (n as f64) > (SKIP_SECONDS * 3.0) * sr { (skip as f64 / step) as usize } else { 0 };
    out.extend_from_slice(&all[skip_s.min(all.len())..]);
    Ok(out)
}

pub fn analyze_samples(x: &[f32]) -> Features {
    let frames = x.len() / HOP;
    if frames < 64 { return Features::default(); }
    let mut energy = Vec::with_capacity(frames);
    let mut hf = 0f64; let mut tot = 0f64;
    for f in 0..frames {
        let w = &x[f * HOP..(f + 1) * HOP];
        let e: f64 = w.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / HOP as f64;
        energy.push(e);
        tot += e;
        hf += w.windows(2).map(|p| { let d = (p[1] - p[0]) as f64; d * d }).sum::<f64>() / HOP as f64;
    }
    let log_e: Vec<f64> = energy.iter().map(|e| (e + 1e-10).ln()).collect();
    let mut onset: Vec<f64> = log_e.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    // odečíst klouzavý průměr (odstraní pomalé změny hlasitosti)
    let k = 16;
    let pref: Vec<f64> = std::iter::once(0.0).chain(onset.iter().scan(0.0, |s, v| { *s += v; Some(*s) })).collect();
    for i in 0..onset.len() { let a = i.saturating_sub(k); let b = (i + k + 1).min(onset.len()); let mean = (pref[b] - pref[a]) / (b - a) as f64; onset[i] = (onset[i] - mean).max(0.0); }
    // vyhlazení (nástupy mezi snímky → širší vrcholy autokorelace i u neceločíselné periody)
    let ker = [1.0, 2.0, 3.0, 2.0, 1.0];
    let onset: Vec<f64> = (0..onset.len()).map(|i| (0..5).map(|j| { let k = i as isize + j as isize - 2; if k < 0 || k as usize >= onset.len() { 0.0 } else { onset[k as usize] * ker[j] } }).sum::<f64>() / 9.0).collect();
    let fps = RATE / HOP as f64;
    let mean_on: f64 = onset.iter().sum::<f64>() / onset.len() as f64;
    let c: Vec<f64> = onset.iter().map(|v| v - mean_on).collect();
    let acf0: f64 = c.iter().map(|v| v * v).sum::<f64>().max(1e-12);
    let lag_min = (fps * 60.0 / 200.0).floor() as usize;
    let lag_max = (fps * 60.0 / 60.0).ceil() as usize;
    let mut acf = vec![0f64; lag_max + 2];
    for lag in lag_min.saturating_sub(1)..=lag_max + 1 {
        if lag >= c.len() { break; }
        acf[lag] = c.iter().zip(&c[lag..]).map(|(a, b)| a * b).sum::<f64>() / acf0;
    }
    let mut best: Option<(usize, f64)> = None;
    for lag in lag_min..=lag_max {
        if lag >= c.len() { break; }
        let bpm = fps * 60.0 / lag as f64;
        let prior = (-0.5 * ((bpm / 120.0).log2() / 0.9).powi(2)).exp();
        // podpora z dvojnásobné periody (takt) zvyšuje jistotu
        let support = acf.get(lag * 2).copied().unwrap_or(0.0).max(0.0) * 0.25;
        let score = (acf[lag] + support) * prior;
        if best.map(|b| score > b.1).unwrap_or(true) { best = Some((lag, score)); }
    }
    let (bpm, conf) = match best {
        Some((lag, _)) if acf[lag] > 0.05 => {
            let b = acf[lag]; let (a, cc) = (acf[lag - 1], acf.get(lag + 1).copied().unwrap_or(b));
            let denom = a - 2.0 * b + cc;
            let shift = if denom.abs() > 1e-9 { (0.5 * (a - cc) / denom).clamp(-0.5, 0.5) } else { 0.0 };
            let bpm = fps * 60.0 / (lag as f64 + shift);
            (Some((bpm * 10.0).round() / 10.0), acf[lag].clamp(0.0, 1.0))
        }
        _ => (None, 0.0),
    };
    let loud = 10.0 * (tot / frames as f64 + 1e-12).log10();
    let bright = (hf / tot.max(1e-12)).clamp(0.0, 4.0);
    let rate = onset.iter().filter(|v| **v > mean_on * 2.0).count() as f64 / (frames as f64 / fps);
    // hrubá energie: hlasitost (dB, typicky −30…−8), hustota nástupů (/s), jas
    let raw = (loud + 30.0) / 22.0 * 0.45 + (rate / 4.0).min(1.5) * 0.35 + (bright / 1.5).min(1.5) * 0.20;
    Features { bpm, bpm_confidence: (conf * 100.0).round() / 100.0, energy_raw: (raw * 1000.0).round() / 1000.0, loudness_db: (loud * 10.0).round() / 10.0, onset_rate: (rate * 100.0).round() / 100.0, brightness: (bright * 100.0).round() / 100.0 }
}

pub fn analyze_file(path: &Path) -> Result<Features> { Ok(analyze_samples(&load_mono(path)?)) }

// ---------------------------------------------------------------------------------------------
// Dávka

#[derive(Debug, Clone, Serialize, Default)]
pub struct BatchStatus { pub running: bool, pub total: usize, pub done: usize, pub bpm_set: usize, pub energy_set: usize, pub kept_manual: usize, pub errors: Vec<String>, pub message: String, pub started_at: String, pub finished_at: Option<String> }

fn cell() -> &'static (Mutex<BatchStatus>, AtomicBool) {
    static S: OnceLock<(Mutex<BatchStatus>, AtomicBool)> = OnceLock::new();
    S.get_or_init(|| (Mutex::new(BatchStatus::default()), AtomicBool::new(false)))
}
pub fn status() -> BatchStatus { cell().0.lock().clone() }
pub fn cancel() { cell().1.store(true, Ordering::Relaxed); }

/// Smí analýza zapsat pole? Ne, pokud ho uživatel zadal ručně nebo importem.
fn writable(a: &MusicAttrs, k: &str, only_missing: bool) -> bool {
    match a.origin.get(k).map(|s| s.as_str()) {
        Some("manual") | Some("import") => false,
        _ => !only_missing || attrs::field_value(a, k).is_empty(),
    }
}

/// Spustí analýzu na pozadí. `only_missing`: jen skladby bez hodnoty; jinak i přepočet dřívějších odhadů.
pub fn start(db: Arc<Db>, kinds: Vec<TrackKind>, only_missing: bool) -> Result<BatchStatus> {
    if cell().0.lock().running { anyhow::bail!("analýza už běží"); }
    cell().1.store(false, Ordering::Relaxed);
    let mut todo = vec![];
    let all = attrs::all(&db)?;
    for k in &kinds {
        let (tracks, _) = db.list_tracks(&crate::db::TrackFilter { kind: Some(k.as_str().into()), include_disabled: Some(true), limit: Some(1_000_000), ..Default::default() })?;
        for t in tracks {
            let a = all.get(&t.id).cloned().unwrap_or_else(|| MusicAttrs::new(t.id));
            if writable(&a, "bpm", only_missing) || writable(&a, "energy", only_missing) || !a.custom.contains_key("energy_raw") { todo.push((t.id, t.path.clone())); }
        }
    }
    *cell().0.lock() = BatchStatus { running: true, total: todo.len(), started_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), message: "analyzuji…".into(), ..Default::default() };
    std::thread::Builder::new().name("orca-audiofeat".into()).spawn(move || {
        let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Min);
        for (id, path) in todo {
            if cell().1.load(Ordering::Relaxed) { break; }
            let res = analyze_file(Path::new(&path));
            let mut st = cell().0.lock();
            st.done += 1;
            match res {
                Ok(f) => {
                    drop(st);
                    let mut a = attrs::get(&db, id).ok().flatten().unwrap_or_else(|| MusicAttrs::new(id));
                    a.custom.insert("energy_raw".into(), format!("{}", f.energy_raw));
                    let mut st = cell().0.lock();
                    if let Some(b) = f.bpm { if writable(&a, "bpm", only_missing) && f.bpm_confidence >= 0.1 { a.bpm = Some(b); a.origin.insert("bpm".into(), format!("analysis:{:.2}", f.bpm_confidence)); st.bpm_set += 1; } else if !writable(&a, "bpm", only_missing) { st.kept_manual += 1; } }
                    drop(st);
                    let _ = attrs::save(&db, &a);
                }
                Err(e) => { if st.errors.len() < 100 { st.errors.push(format!("{path}: {e}")); } }
            }
        }
        // energie 1–5 podle kvintilů hrubé energie v rámci knihovny
        let n = rescale_energy(&db, &kinds, only_missing).unwrap_or(0);
        let mut st = cell().0.lock();
        st.energy_set = n;
        st.running = false;
        st.finished_at = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
        st.message = if cell().1.load(Ordering::Relaxed) { "zrušeno".into() } else { format!("hotovo: BPM u {} skladeb, energie u {}", st.bpm_set, st.energy_set) };
    })?;
    Ok(status())
}

/// Převede hrubou energii na 1–5 podle kvintilů (zvlášť pro každý typ). Vrací počet zapsaných hodnot.
pub fn rescale_energy(db: &Db, kinds: &[TrackKind], only_missing: bool) -> Result<usize> {
    let all = attrs::all(db)?;
    let mut n = 0;
    for k in kinds {
        let (tracks, _) = db.list_tracks(&crate::db::TrackFilter { kind: Some(k.as_str().into()), include_disabled: Some(true), limit: Some(1_000_000), ..Default::default() })?;
        let mut v: Vec<(i64, f64)> = tracks.iter().filter_map(|t| all.get(&t.id).and_then(|a| a.custom.get("energy_raw")).and_then(|x| x.parse::<f64>().ok()).map(|x| (t.id, x))).collect();
        if v.len() < 5 { continue; }
        v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let len = v.len();
        for (i, (id, _)) in v.into_iter().enumerate() {
            let mut a = all.get(&id).cloned().unwrap_or_else(|| MusicAttrs::new(id));
            if !writable(&a, "energy", only_missing) { continue; }
            a.energy = Some((i * 5 / len) as i64 + 1);
            a.origin.insert("energy".into(), "analysis:0.70".into());
            attrs::save(db, &a)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn clicks(bpm: f64, secs: f64) -> Vec<f32> {
        let n = (secs * RATE) as usize;
        let period = RATE * 60.0 / bpm;
        let mut x = vec![0f32; n];
        let mut seed = 1u32;
        for (i, v) in x.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5) * 0.01;
            let ph = (i as f64 % period) / RATE;
            *v = noise + if ph < 0.03 { (1.0 - ph / 0.03) as f32 * 0.8 * ((i as f32) * 0.9).sin() } else { 0.0 };
        }
        x
    }
    #[test]
    fn tempo_from_clicks() {
        for bpm in [90.0, 120.0, 128.0, 150.0] {
            let f = analyze_samples(&clicks(bpm, 40.0));
            let got = f.bpm.expect("tempo");
            assert!((got - bpm).abs() / bpm < 0.03, "očekáváno {bpm}, změřeno {got}");
        }
        let quiet = analyze_samples(&clicks(80.0, 30.0).iter().map(|v| v * 0.1).collect::<Vec<_>>());
        let loud = analyze_samples(&clicks(160.0, 30.0));
        assert!(loud.energy_raw > quiet.energy_raw);
    }
}
