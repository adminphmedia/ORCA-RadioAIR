//! Autocue (začátek, bod mixu, konec) a měření hlasitosti LUFS (EBU R128).

use crate::config::CueThresholds;
use crate::decode::AudioDecoder;
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub duration_ms: i64,
    pub cue_in_ms: i64,
    pub mix_ms: i64,
    pub cue_out_ms: i64,
    pub lufs: Option<f64>,
}

const WIN_MS: usize = 10;
/// Maximální délka "konce" skladby, přes kterou se smí míchat (ms).
const MAX_MIX_TAIL_MS: i64 = 12_000;

fn to_db(x: f32) -> f32 { if x <= 1e-9 { -180.0 } else { 20.0 * x.log10() } }

pub fn analyze_file(path: &Path, th: &CueThresholds) -> Result<Analysis> {
    let mut dec = AudioDecoder::open(path)?;
    let rate = dec.sample_rate as usize;
    let win = (rate * WIN_MS / 1000).max(1);
    let mut ebu = ebur128::EbuR128::new(2, dec.sample_rate, ebur128::Mode::I).ok();
    let mut peaks: Vec<f32> = Vec::new();
    let mut rms: Vec<f32> = Vec::new();
    let (mut cur_peak, mut cur_sq, mut cur_n) = (0f32, 0f64, 0usize);
    let mut buf = Vec::new();
    let mut total_frames: u64 = 0;
    while dec.next_stereo(&mut buf)? {
        if let Some(e) = ebu.as_mut() { let _ = e.add_frames_f32(&buf); }
        for fr in buf.chunks_exact(2) {
            let a = fr[0].abs().max(fr[1].abs());
            if a > cur_peak { cur_peak = a; }
            cur_sq += ((fr[0] * fr[0] + fr[1] * fr[1]) * 0.5) as f64;
            cur_n += 1;
            if cur_n == win {
                peaks.push(cur_peak);
                rms.push((cur_sq / cur_n as f64).sqrt() as f32);
                cur_peak = 0.0; cur_sq = 0.0; cur_n = 0;
            }
        }
        total_frames += (buf.len() / 2) as u64;
    }
    if cur_n > 0 { peaks.push(cur_peak); rms.push((cur_sq / cur_n as f64).sqrt() as f32); }
    let duration_ms = (total_frames as f64 * 1000.0 / rate as f64).round() as i64;
    let lufs = ebu.and_then(|e| e.loudness_global().ok()).filter(|l| l.is_finite());
    Ok(compute_cues(&peaks, &rms, duration_ms, th, lufs))
}

pub fn compute_cues(peaks: &[f32], rms: &[f32], duration_ms: i64, th: &CueThresholds, lufs: Option<f64>) -> Analysis {
    let n = peaks.len();
    if n == 0 { return Analysis { duration_ms, cue_in_ms: 0, mix_ms: duration_ms, cue_out_ms: duration_ms, lufs }; }
    let w = WIN_MS as i64;
    let first = peaks.iter().position(|&p| to_db(p) > th.start_db);
    let last = peaks.iter().rposition(|&p| to_db(p) > th.end_db);
    let (cue_in, cue_out) = match (first, last) {
        (Some(f), Some(l)) if l >= f => ((f as i64 * w).min(duration_ms), ((l as i64 + 1) * w).min(duration_ms)),
        _ => (0, duration_ms), // ticho nebo nic nenalezeno → celý soubor
    };
    let mix = match th.mix_db {
        None => cue_out,
        Some(mix_db) => {
            // klouzavý průměr RMS přes ~300 ms
            let span = 30usize;
            let mut acc = 0f32;
            let mut smooth = vec![0f32; n];
            for i in 0..n {
                acc += rms[i];
                if i >= span { acc -= rms[i - span]; }
                smooth[i] = acc / (i.min(span - 1) + 1) as f32;
            }
            // referenční hlasitost = medián RMS těla skladby (mezi cue in/out, bez ticha)
            let (a, b) = ((cue_in / w) as usize, ((cue_out / w) as usize).min(n));
            let mut body: Vec<f32> = smooth[a..b.max(a)].iter().map(|&r| to_db(r)).filter(|&d| d > -60.0).collect();
            body.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            let reference = if body.is_empty() { -20.0 } else { body[body.len() / 2] };
            let threshold = reference + mix_db.min(0.0);
            let last_loud = smooth.iter().rposition(|&r| to_db(r) >= threshold);
            let m = match last_loud { Some(i) => ((i as i64 + 1) * w).min(cue_out), None => cue_out };
            m.max(cue_out - MAX_MIX_TAIL_MS).max(cue_in)
        }
    };
    Analysis { duration_ms, cue_in_ms: cue_in, mix_ms: mix, cue_out_ms: cue_out, lufs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_on_synthetic_envelope() {
        // 1 s ticho, 10 s plná hlasitost, 3 s doznívání, 1 s ticho
        let mut peaks = vec![0.0f32; 100];
        peaks.extend(std::iter::repeat(0.8).take(1000));
        for i in 0..300 { peaks.push(0.8 * (1.0 - i as f32 / 300.0)); }
        peaks.extend(std::iter::repeat(0.0).take(100));
        let rms: Vec<f32> = peaks.iter().map(|p| p * 0.7).collect();
        let th = CueThresholds { start_db: -40.0, end_db: -40.0, mix_db: Some(-15.0) };
        let a = compute_cues(&peaks, &rms, 15_000, &th, None);
        assert_eq!(a.cue_in_ms, 1000);
        assert!(a.cue_out_ms > 13_800 && a.cue_out_ms <= 14_000, "{a:?}");
        assert!(a.mix_ms > 11_000 && a.mix_ms < 13_900, "{a:?}");
        let spot = CueThresholds { start_db: -90.0, end_db: -90.0, mix_db: None };
        let b = compute_cues(&peaks, &rms, 15_000, &spot, None);
        assert_eq!(b.mix_ms, b.cue_out_ms);
    }
}
