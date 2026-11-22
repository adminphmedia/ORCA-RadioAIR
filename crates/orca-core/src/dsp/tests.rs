//! Reprodukovatelné testy processingu na syntetickém materiálu.
//! Spuštění s výpisem měření: `cargo test -p orca-core dsp:: -- --nocapture`

use super::params::{effective, factory_presets, template, BasicControls, Preset};
use super::*;

const FS: u32 = 48000;

/// Deterministický šum (xorshift) – „růžový“ přibližně filtrem 1. řádu.
struct Noise { s: u64, b: [f32; 3] }
impl Noise {
    fn new(seed: u64) -> Self { Self { s: seed | 1, b: [0.0; 3] } }
    fn white(&mut self) -> f32 { self.s ^= self.s << 13; self.s ^= self.s >> 7; self.s ^= self.s << 17; ((self.s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 }
    fn pink(&mut self) -> f32 { let w = self.white(); self.b[0] = 0.99765 * self.b[0] + w * 0.0990460; self.b[1] = 0.96300 * self.b[1] + w * 0.2965164; self.b[2] = 0.57000 * self.b[2] + w * 1.0526913; (self.b[0] + self.b[1] + self.b[2] + w * 0.1848) * 0.2 }
}

fn rms_db(x: &[f32]) -> f32 { let p: f64 = x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64; (10.0 * p.max(1e-12).log10()) as f32 }
fn scale_to(x: &mut [f32], db: f32) { let g = db_to_lin(db - rms_db(x)); x.iter_mut().for_each(|v| *v *= g); }

/// Segmenty programu (prokládané stereo): (název, data)
fn program() -> Vec<(&'static str, Vec<f32>)> {
    let sec = |s: f32| (s * FS as f32) as usize;
    let mut n = Noise::new(7);
    let mut out = vec![];
    // moderní CHR – hustá hudba, hlasitě
    let mut v = vec![0f32; sec(12.0) * 2];
    for (i, fr) in v.chunks_exact_mut(2).enumerate() { let t = i as f32 / FS as f32; let env = 0.8 + 0.2 * (t * 2.0 * std::f32::consts::PI * 2.0).sin(); let a = n.pink() * env; fr[0] = a + 0.1 * n.white(); fr[1] = a + 0.1 * n.white(); }
    scale_to(&mut v, -14.0); out.push(("CHR skladba", v));
    // tichá starší skladba
    let mut v = vec![0f32; sec(12.0) * 2];
    for (i, fr) in v.chunks_exact_mut(2).enumerate() { let t = i as f32 / FS as f32; let env = 0.5 + 0.5 * (t * 0.7).sin().abs(); let a = n.pink() * env; fr[0] = a; fr[1] = a * 0.9 + 0.05 * n.white(); }
    scale_to(&mut v, -30.0); out.push(("tichá starší skladba", v));
    // hlasitý jingle
    let mut v = vec![0f32; sec(5.0) * 2];
    for fr in v.chunks_exact_mut(2) { let a = n.pink() + 0.35 * n.white(); fr[0] = a; fr[1] = a; }
    scale_to(&mut v, -9.0); out.push(("hlasitý jingle", v));
    // mluvené slovo s pauzami (slabiky 4 Hz, pauzy)
    let mut v = vec![0f32; sec(12.0) * 2];
    for (i, fr) in v.chunks_exact_mut(2).enumerate() { let t = i as f32 / FS as f32; let syl = ((t * 4.0 * std::f32::consts::PI).sin()).max(0.0); let pause = if (t % 3.0) > 2.4 { 0.02 } else { 1.0 }; let a = (n.pink() * 0.7 + 0.3 * (t * 2.0 * std::f32::consts::PI * 180.0).sin()) * syl * pause; fr[0] = a; fr[1] = a; }
    scale_to(&mut v, -24.0); out.push(("mluvené slovo", v));
    // ticho a náběh
    out.push(("ticho", vec![0f32; sec(4.0) * 2]));
    // taneční skladba s výrazným basem (kopák 60 Hz, 128 BPM)
    let mut v = vec![0f32; sec(12.0) * 2];
    for (i, fr) in v.chunks_exact_mut(2).enumerate() { let t = i as f32 / FS as f32; let beat = t % (60.0 / 128.0); let kick = (2.0 * std::f32::consts::PI * 60.0 * t).sin() * (-beat * 18.0).exp() * 3.0; let a = kick + n.pink() * 0.6; fr[0] = a; fr[1] = a; }
    scale_to(&mut v, -12.0); out.push(("taneční s basem", v));
    // velmi nízká úroveň a přebuzení
    let mut v = vec![0f32; sec(6.0) * 2]; for fr in v.chunks_exact_mut(2) { let a = n.pink(); fr[0] = a; fr[1] = a; } scale_to(&mut v, -48.0); out.push(("velmi nízká úroveň", v));
    let mut v = vec![0f32; sec(6.0) * 2]; for fr in v.chunks_exact_mut(2) { let a = n.pink(); fr[0] = a; fr[1] = a; } scale_to(&mut v, -4.0); out.push(("přebuzený vstup", v));
    out
}

fn lufs(x: &[f32], fs: u32) -> f64 {
    let mut e = ebur128::EbuR128::new(2, fs, ebur128::Mode::I).unwrap();
    e.add_frames_f32(x).unwrap();
    e.loudness_global().unwrap_or(f64::NEG_INFINITY)
}
fn true_peak(x: &[f32], fs: u32) -> f64 {
    let mut e = ebur128::EbuR128::new(2, fs, ebur128::Mode::TRUE_PEAK).unwrap();
    e.add_frames_f32(x).unwrap();
    let p = e.true_peak(0).unwrap().max(e.true_peak(1).unwrap());
    20.0 * p.log10()
}

fn preset(id: &str) -> Preset { factory_presets().into_iter().find(|p| p.id == id).unwrap() }

/// Celý program jedním řetězcem (stav se drží přes zdroje), měření po segmentech.
fn run(p: &Preset) -> Vec<(&'static str, f64, f64)> {
    let mut chain = MasterChain::new(effective(p, &BasicControls::default()), FS, 1);
    let segs = program();
    let mut res = vec![];
    for (name, data) in segs {
        let mut out = data.clone();
        for blk in out.chunks_mut(960) { chain.process(blk); }
        // vynechat první 2 s náběhu AGC v segmentu
        let skip = (2 * FS as usize * 2).min(out.len());
        res.push((name, lufs(&data[skip..], FS), lufs(&out[skip..], FS)));
    }
    res
}

#[test]
fn energic_chr_reduces_source_differences() {
    let r = run(&preset("energic-chr"));
    println!("\nENERGIC CHR – hlasitost po segmentech (LUFS, po 2 s náběhu):");
    for (n, a, b) in &r { println!("  {n:24} vstup {a:7.1}  výstup {b:7.1}"); }
    let music: Vec<&(&str, f64, f64)> = r.iter().filter(|x| ["CHR skladba", "tichá starší skladba", "hlasitý jingle", "mluvené slovo", "taneční s basem"].contains(&x.0)).collect();
    let spread = |f: fn(&(&str, f64, f64)) -> f64| { let v: Vec<f64> = music.iter().map(|x| f(x)).collect(); v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min) };
    let (si, so) = (spread(|x| x.1), spread(|x| x.2));
    println!("  rozptyl mezi zdroji: vstup {si:.1} LU → výstup {so:.1} LU");
    assert!(so < si - 5.0, "processing má zmírnit rozdíly mezi zdroji ({si:.1} → {so:.1})");
    // ticho nesmí být vytaženo (gate)
    let silence = r.iter().find(|x| x.0 == "ticho").unwrap();
    assert!(silence.2 < -70.0 || silence.2.is_infinite());
}

#[test]
fn presets_change_pcm() {
    let base = run(&preset("transparent"));
    for id in ["energic-chr", "energic-dance"] {
        let r = run(&preset(id));
        let diff: f64 = r.iter().zip(base.iter()).filter(|(a, _)| a.2.is_finite()).map(|(a, b)| (a.2 - b.2).abs()).sum();
        println!("{id}: součet rozdílů hlasitosti vůči Transparent {diff:.1} LU");
        assert!(diff > 5.0);
    }
    // Transparent: hlasitost zachována
    for (n, a, b) in &base { if a.is_finite() && *a > -60.0 { assert!((a - b).abs() < 0.3, "{n}: {a:.2} → {b:.2}"); } }
}

/// Syntetický test mezivzorkových špiček: sinus fs/4, fáze 45°, vzorky ±1,0 → true peak ≈ +3 dBTP.
fn isp_signal(fs: u32, secs: f32) -> Vec<f32> {
    let n = (fs as f32 * secs) as usize;
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n { let s = ((std::f64::consts::FRAC_PI_2 * (i % 4) as f64 + std::f64::consts::FRAC_PI_4).sin() * std::f64::consts::SQRT_2) as f32; v.push(s); v.push(s); }
    v
}

#[test]
fn true_peak_limiter_holds_ceiling() {
    let inp = isp_signal(FS, 3.0);
    println!("\nTrue-peak test (sinus fs/4, 45°): vstup {:.2} dBTP", true_peak(&inp, FS));
    for (rate, ceil) in [(48000u32, -2.0f32), (44100, -2.0), (48000, -1.0)] {
        let mut b = Branch::new(BranchConf { ceiling_dbtp: ceil, sample_rate: if rate == 48000 { 0 } else { rate }, ..template("icecast", "test") }, FS, rate);
        let mut out = vec![];
        for blk in inp.chunks(960) { b.process(blk, &mut out); }
        let tp = true_peak(&out, rate);
        println!("  větev {rate} Hz, strop {ceil} dBTP → naměřeno {tp:.2} dBTP (latence {:.2} ms)", b.latency_ms());
        assert!(tp <= ceil as f64 + 0.05, "{rate}: {tp}");
    }
    // běžný program (přebuzený) přes ENERGIC CHR + větev
    let mut chain = MasterChain::new(effective(&preset("energic-chr"), &BasicControls::default()), FS, 1);
    let mut b = Branch::new(template("icecast", "t"), FS, 44100);
    let mut out = vec![];
    for (_, mut d) in program() { for blk in d.chunks_mut(960) { chain.process(blk); b.process(blk, &mut out); } }
    let tp = true_peak(&out, 44100);
    println!("  program přes ENERGIC CHR → Icecast 44,1 kHz: {tp:.2} dBTP");
    assert!(tp <= -1.95);
}

#[test]
fn realtime_load() {
    let p = preset("energic-chr");
    let mut chain = MasterChain::new(effective(&p, &BasicControls::default()), FS, 1);
    let mut b1 = Branch::new(template("icecast", "a"), FS, 44100);
    let mut b2 = Branch::new(template("dab", "b"), FS, 48000);
    let mut n = Noise::new(3);
    let secs = 30.0;
    let mut data: Vec<f32> = (0..(secs * FS as f32) as usize * 2).map(|_| n.pink() * 0.3).collect();
    let t = std::time::Instant::now();
    let (mut o1, mut o2) = (vec![], vec![]);
    for blk in data.chunks_mut(960) { chain.process(blk); o1.clear(); o2.clear(); b1.process(blk, &mut o1); b2.process(blk, &mut o2); }
    let el = t.elapsed().as_secs_f64();
    println!("\nZátěž: {secs} s audia (master + 2 větve) za {el:.3} s → {:.1}× rychleji než reálný čas, {:.2} % jednoho jádra", secs as f64 / el, el / secs as f64 * 100.0);
    if !cfg!(debug_assertions) { assert!(el < secs as f64 / 3.0, "processing je příliš pomalý ({el:.2} s)"); }
}


/// Referenční true peak 8× převzorkováním (nezávislé na ebur128).
fn tp_8x(x: &[f32], fs: u32) -> f64 {
    use rubato::Resampler;
    let mut r = rubato::FftFixedIn::<f32>::new(fs as usize, fs as usize * 8, 1024, 1, 2).unwrap();
    let (mut l, mut rr): (Vec<f32>, Vec<f32>) = (x.iter().step_by(2).cloned().collect(), x.iter().skip(1).step_by(2).cloned().collect());
    let n = l.len() / 1024 * 1024; l.truncate(n); rr.truncate(n);
    let mut mx = 0f32;
    for i in (0..n).step_by(1024) { let o = r.process(&[&l[i..i + 1024], &rr[i..i + 1024]], None).unwrap(); for c in &o { for v in c { mx = mx.max(v.abs()); } } }
    20.0 * (mx as f64).log10()
}

#[test]
fn true_peak_reference_8x() {
    let inp = isp_signal(FS, 3.0);
    let mut b = Branch::new(BranchConf { ceiling_dbtp: -2.0, ..template("icecast", "t") }, FS, 48000);
    let mut out = vec![];
    for blk in inp.chunks(960) { b.process(blk, &mut out); }
    let (e, r) = (true_peak(&out, FS), tp_8x(&out, FS));
    println!("ISP test −2 dBTP: ebur128 {e:.2} dBTP, reference 8× {r:.2} dBTP");
    assert!(e <= -2.0 && r <= -2.0);
}
