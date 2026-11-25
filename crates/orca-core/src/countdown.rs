//! Časomíra (odpočet): ORCA vygeneruje zvuk odpočtu – pípnutí každou sekundu a dlouhý tón v cílovém čase,
//! volitelně s podkresem namíchaným pod odpočet. Hraje jako položka fronty; po posledním tónu jede další položka.
//! V přesném čase: start se spočítá tak, aby poslední (dlouhý) tón zazněl přesně v zadaný okamžik.

use crate::model::{QueueItem, Track, TrackKind};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::f32::consts::PI;
use std::path::{Path, PathBuf};

pub const STYLES: &[(&str, &str)] = &[
    ("greenwich", "Klasické pípání (Greenwich)"),
    ("high", "Vysoké pípání"),
    ("soft", "Jemný tón"),
    ("digital", "Digitální hodinky"),
    ("tick", "Tikání hodin"),
    ("sonar", "Sonar"),
    ("rising", "Stoupající tóny"),
    ("marimba", "Marimba"),
    ("pulse", "Hluboký puls"),
    ("news", "Zpravodajský blip"),
    ("custom", "Vlastní zvuk"),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CountdownSpec {
    pub style: String,
    /// počet krátkých tónů před cílovým časem (1–10)
    pub seconds: u32,
    /// v cílovém čase dlouhý tón (jinak jen poslední krátký)
    pub long_final: bool,
    pub volume_db: f32,
    /// podkres pod odpočtem (soubor z knihovny)
    pub bed_track_id: Option<i64>,
    pub bed_level_db: f32,
    /// Podkres začne už N s před prvním tónem (předehra)
    pub bed_pre_s: f32,
}

impl Default for CountdownSpec {
    fn default() -> Self { Self { style: "greenwich".into(), seconds: 5, long_final: true, volume_db: -3.0, bed_track_id: None, bed_level_db: -10.0, bed_pre_s: 0.0 } }
}

impl CountdownSpec {
    pub fn label(&self) -> String {
        let s = STYLES.iter().find(|x| x.0 == self.style).map(|x| x.1).unwrap_or("Časomíra");
        format!("Časomíra {} s – {}", self.seconds.clamp(1, 10), s)
    }
    /// Od začátku souboru (bez předehry) do začátku posledního tónu.
    pub fn to_final_ms(&self) -> i64 { self.seconds.clamp(1, 10) as i64 * 1000 }
}

fn env(i: usize, len: usize, attack: usize, release: usize) -> f32 {
    let a = if i < attack { i as f32 / attack.max(1) as f32 } else { 1.0 };
    let r = if i + release > len { (len - i) as f32 / release.max(1) as f32 } else { 1.0 };
    a.min(r)
}

/// Jeden tón odpočtu (mono). `k` = pořadí (0..n), `fin` = poslední tón v cílovém čase.
fn unit(style: &str, k: u32, n: u32, fin: bool, rate: u32) -> Vec<f32> {
    let sr = rate as f32;
    let ms = |m: f32| (m * sr / 1000.0) as usize;
    let tone = |freq: f32, len_ms: f32, amp: f32| -> Vec<f32> {
        let len = ms(len_ms);
        (0..len).map(|i| (2.0 * PI * freq * i as f32 / sr).sin() * amp * env(i, len, ms(4.0), ms(8.0))).collect()
    };
    let decay = |freqs: &[(f32, f32)], len_ms: f32, tau_ms: f32| -> Vec<f32> {
        let len = ms(len_ms); let tau = tau_ms * sr / 1000.0;
        (0..len).map(|i| { let t = i as f32 / sr; let e = (-(i as f32) / tau).exp() * env(i, len, ms(2.0), ms(20.0)); freqs.iter().map(|(f, a)| (2.0 * PI * f * t).sin() * a).sum::<f32>() * e }).collect()
    };
    match style {
        "high" => if fin { tone(1500.0, 600.0, 0.7) } else { tone(1500.0, 90.0, 0.7) },
        "soft" => { let len = if fin { 900.0 } else { 220.0 }; let l = ms(len); (0..l).map(|i| (2.0 * PI * 880.0 * i as f32 / sr).sin() * 0.55 * env(i, l, ms(40.0), ms(if fin { 500.0 } else { 120.0 }))).collect() }
        "digital" => {
            let sq = |f: f32, len_ms: f32| -> Vec<f32> { let l = ms(len_ms); (0..l).map(|i| { let p = (f * i as f32 / sr).fract(); (if p < 0.5 { 0.35 } else { -0.35 }) * env(i, l, ms(1.0), ms(3.0)) }).collect() };
            if fin { let mut v = sq(2000.0, 120.0); v.extend(vec![0.0; ms(60.0)]); v.extend(sq(2000.0, 120.0)); v.extend(vec![0.0; ms(60.0)]); v.extend(sq(2000.0, 300.0)); v } else { let mut v = sq(2000.0, 60.0); v.extend(vec![0.0; ms(50.0)]); v.extend(sq(2000.0, 60.0)); v }
        }
        "tick" => {
            // tik-tak: krátké cvaknutí (šum přes rezonanci), střídavě výš a níž; v cíli zvonek
            if fin { decay(&[(1318.0, 0.5), (2637.0, 0.25), (3951.0, 0.12)], 1400.0, 380.0) } else {
                let f = if k % 2 == 0 { 3200.0 } else { 2400.0 };
                let l = ms(35.0); let mut seed = 12345u32 + k;
                (0..l).map(|i| { seed = seed.wrapping_mul(1103515245).wrapping_add(12345); let nse = ((seed >> 16) as f32 / 32768.0 - 1.0) * 0.4; (nse + (2.0 * PI * f * i as f32 / sr).sin() * 0.5) * (-(i as f32) / (sr * 0.006)).exp() }).collect()
            }
        }
        "sonar" => if fin { decay(&[(1200.0, 0.7), (2400.0, 0.1)], 1500.0, 450.0) } else { decay(&[(1200.0, 0.6), (2400.0, 0.08)], 700.0, 160.0) },
        "rising" => {
            // každý tón o půltón výš, v cíli durový akord
            let base = 440.0 * 2f32.powf((k as f32) / 12.0);
            if fin { let f = 440.0 * 2f32.powf(n as f32 / 12.0); decay(&[(f, 0.35), (f * 1.26, 0.3), (f * 1.5, 0.3), (f * 2.0, 0.15)], 1200.0, 500.0) } else { tone(base, 150.0, 0.6) }
        }
        "marimba" => { let f = if fin { 523.25 } else { 783.99 }; decay(&[(f, 0.6), (f * 4.0, 0.15), (f * 9.2, 0.05)], if fin { 1300.0 } else { 600.0 }, if fin { 350.0 } else { 120.0 }) }
        "pulse" => {
            // hluboké „srdce“: dvojitý úder 60 Hz; v cíli delší basový tón
            let thump = |f: f32, l_ms: f32, tau: f32| decay(&[(f, 0.9), (f * 2.0, 0.25)], l_ms, tau);
            if fin { thump(55.0, 1200.0, 420.0) } else { let mut v = thump(65.0, 160.0, 45.0); v.extend(vec![0.0; ms(70.0)]); v.extend(thump(60.0, 160.0, 45.0).into_iter().map(|x| x * 0.7)); v }
        }
        "news" => {
            // dvoutónový blip (G6–C7); v cíli tři stoupající tóny
            if fin { let mut v = tone(784.0, 90.0, 0.55); v.extend(tone(1047.0, 90.0, 0.55)); v.extend(tone(1568.0, 450.0, 0.55)); v } else { let mut v = tone(1568.0, 45.0, 0.5); v.extend(tone(2093.0, 45.0, 0.5)); v }
        }
        // greenwich (výchozí): 1 kHz, 100 ms, dlouhý 500 ms
        _ => if fin { tone(1000.0, 500.0, 0.7) } else { tone(1000.0, 100.0, 0.7) },
    }
}

pub fn dir(data_dir: &Path) -> PathBuf { data_dir.join("Countdown") }

static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
/// Nastaví datovou složku (při startu) – pro rozvinutí playlistů, kde není po ruce.
pub fn set_data_dir(p: &Path) { let _ = DATA_DIR.set(p.to_path_buf()); }
pub fn data_dir_of(_db: &crate::db::Db) -> PathBuf { DATA_DIR.get().cloned().unwrap_or_else(|| std::env::temp_dir().join("orca")) }
fn custom_path(data_dir: &Path, which: &str) -> Option<PathBuf> {
    let d = dir(data_dir);
    std::fs::read_dir(&d).ok()?.flatten().map(|e| e.path()).find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(which))
}
pub fn custom_status(data_dir: &Path) -> (bool, bool) { (custom_path(data_dir, "custom_unit").is_some(), custom_path(data_dir, "custom_final").is_some()) }

/// Vlastní zvuk (jedna jednotka odpočtu, max. 1 s; finální max. 3 s) – stereo smíchané do mono.
fn load_custom(data_dir: &Path, which: &str, rate: u32, max_ms: i64) -> Option<Vec<f32>> {
    let p = custom_path(data_dir, which)?;
    let mut st = crate::decode::StereoStream::open(&p, rate, 0).ok()?;
    let mut out = vec![]; let mut buf = vec![];
    let want = (max_ms as usize * rate as usize / 1000) * 2;
    while out.len() < want { match st.read(&mut buf) { Ok(true) => out.extend_from_slice(&buf), _ => break } }
    out.truncate(want);
    let mut m: Vec<f32> = out.chunks(2).map(|c| (c[0] + c.get(1).copied().unwrap_or(c[0])) * 0.5).collect();
    let l = m.len(); let fade = (rate as usize / 200).min(l / 2);
    for i in 0..fade { m[l - 1 - i] *= i as f32 / fade as f32; }
    Some(m)
}

/// Vyrenderuje odpočet do WAV. `lead_ms` = ticho/podkres před prvním tónem (pro přesný čas).
pub fn render(db: &crate::db::Db, data_dir: &Path, spec: &CountdownSpec, rate: u32, lead_ms: i64) -> Result<(PathBuf, i64)> {
    let n = spec.seconds.clamp(1, 10);
    let sr = rate as usize;
    let pre = (spec.bed_pre_s.clamp(0.0, 30.0) * 1000.0) as i64 * spec.bed_track_id.is_some() as i64;
    let lead = lead_ms.max(0) + pre;
    let (cu, cf) = if spec.style == "custom" { (load_custom(data_dir, "custom_unit", rate, 1000), load_custom(data_dir, "custom_final", rate, 3000)) } else { (None, None) };
    if spec.style == "custom" && cu.is_none() { return Err(anyhow!("není nahraný vlastní zvuk odpočtu")); }
    let mut units: Vec<(i64, Vec<f32>)> = vec![];
    for k in 0..n {
        let u = match &cu { Some(c) => c.clone(), None => unit(&spec.style, k, n, false, rate) };
        units.push((lead + k as i64 * 1000, u));
    }
    let fin = if spec.long_final { match (&cf, &cu) { (Some(f), _) => f.clone(), (None, Some(u)) => u.clone(), _ => unit(&spec.style, n, n, true, rate) } } else { match &cu { Some(u) => u.clone(), None => unit(&spec.style, n, n, false, rate) } };
    let fin_at = lead + n as i64 * 1000;
    let total_ms = fin_at + (fin.len() as i64 * 1000 / rate as i64) + 250;
    let frames = total_ms as usize * sr / 1000;
    let mut mono = vec![0f32; frames];
    let g = 10f32.powf(spec.volume_db.clamp(-30.0, 6.0) / 20.0);
    for (at, u) in units.iter().chain(std::iter::once(&(fin_at, fin))) {
        let s = *at as usize * sr / 1000;
        for (i, x) in u.iter().enumerate() { if let Some(m) = mono.get_mut(s + i) { *m += x * g; } }
    }
    let mut st = vec![0f32; frames * 2];
    for (i, m) in mono.iter().enumerate() { st[i * 2] = *m; st[i * 2 + 1] = *m; }
    // podkres pod odpočtem: od začátku souboru, na konci dozní (300 ms)
    if let Some(bid) = spec.bed_track_id {
        if let Some(t) = db.get_track(bid)? {
            let (ci, _, _) = t.effective_cues();
            if let Ok(mut bs) = crate::decode::StereoStream::open(Path::new(&t.path), rate, ci) {
                let mut bed = vec![]; let mut buf = vec![];
                while bed.len() < st.len() { match bs.read(&mut buf) { Ok(true) => bed.extend_from_slice(&buf), _ => break } }
                let bg = 10f32.powf(spec.bed_level_db.clamp(-40.0, 0.0) / 20.0);
                let fade_in = sr / 20; let fade_out = sr * 3 / 10;
                let fr = st.len() / 2;
                for i in 0..fr.min(bed.len() / 2) {
                    let e = (i as f32 / fade_in as f32).min(1.0).min(((fr - i) as f32 / fade_out as f32).min(1.0));
                    st[i * 2] += bed[i * 2] * bg * e; st[i * 2 + 1] += bed[i * 2 + 1] * bg * e;
                }
            }
        }
    }
    for x in st.iter_mut() { *x = x.clamp(-0.98, 0.98); }
    let d = dir(data_dir);
    std::fs::create_dir_all(&d)?;
    // úklid starých vygenerovaných souborů
    if let Ok(rd) = std::fs::read_dir(&d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("cd_")).unwrap_or(false) {
                if e.metadata().and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).map(|a| a.as_secs() > 86_400).unwrap_or(false) { let _ = std::fs::remove_file(p); }
            }
        }
    }
    let path = d.join(format!("cd_{}.wav", uuid::Uuid::new_v4().simple()));
    let specw = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&path, specw)?;
    for x in st { w.write_sample((x * 32767.0) as i16)?; }
    w.finalize()?;
    Ok((path, total_ms))
}

/// Položka fronty pro vyrenderovaný odpočet.
pub fn queue_item(spec: &CountdownSpec, path: &Path, total_ms: i64, source: &str, reason: &str) -> QueueItem {
    let t = Track { kind: Some(TrackKind::Asset), title: spec.label(), artist: "ORCA".into(), filename: path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), path: path.to_string_lossy().to_string(),
        duration_ms: total_ms, cue_out_ms: total_ms, mix_ms: total_ms, enabled: true, analyzed: true, category: "Časomíra".into(), ..Default::default() };
    QueueItem::new(t, source, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn units_have_sound() {
        for (s, _) in STYLES.iter().filter(|s| s.0 != "custom") {
            for fin in [false, true] {
                let u = unit(s, 1, 5, fin, 48000);
                assert!(!u.is_empty() && u.len() < 48000 * 2, "{s}");
                assert!(u.iter().any(|x| x.abs() > 0.05), "{s} ticho");
            }
        }
    }
}
