//! Živý vstup: playlist se zastaví a pokračuje na pokyn (Pokračovat / GPI).

use orca_core::config::Config;
use orca_core::engine::{Cmd, Engine};
use orca_core::events::EventBus;
use orca_core::model::{QueueItem, Track, TrackKind};
use parking_lot::RwLock;
use std::f32::consts::PI;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn write_wav(path: &Path, freq: f32, secs: f32, rate: u32) {
    let n = (secs * rate as f32) as usize;
    let mut data = Vec::with_capacity(n * 4);
    for i in 0..n { let v = ((2.0 * PI * freq * i as f32 / rate as f32).sin() * 0.4 * 32767.0) as i16; data.extend_from_slice(&v.to_le_bytes()); data.extend_from_slice(&v.to_le_bytes()); }
    let mut f = std::fs::File::create(path).unwrap();
    for (a, b) in [(b"RIFF" as &[u8], (36 + data.len() as u32).to_le_bytes().to_vec())] { f.write_all(a).unwrap(); f.write_all(&b).unwrap(); }
    f.write_all(b"WAVEfmt ").unwrap();
    for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), rate.to_le_bytes().to_vec(), (rate * 4).to_le_bytes().to_vec(), 4u16.to_le_bytes().to_vec(), 16u16.to_le_bytes().to_vec()] { f.write_all(&v).unwrap(); }
    f.write_all(b"data").unwrap(); f.write_all(&(data.len() as u32).to_le_bytes()).unwrap(); f.write_all(&data).unwrap();
}

fn power(s: &[f32], freq: f32, rate: f32) -> f32 {
    let k = 2.0 * (2.0 * PI * freq / rate).cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &x in s { let s0 = x + k * s1 - s2; s2 = s1; s1 = s0; }
    (s1 * s1 + s2 * s2 - k * s1 * s2) / s.len() as f32
}

fn tr(path: &Path, id: i64, kind: TrackKind, dur: i64) -> Track {
    Track { id, kind: Some(kind), path: path.to_string_lossy().into(), filename: "x.wav".into(), title: format!("T{id}"), duration_ms: dur, mix_ms: dur, cue_out_ms: dur, analyzed: true, enabled: true, ..Default::default() }
}

#[test]
fn live_break_waits_for_continue() {
    let dir = std::env::temp_dir().join(format!("orca-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.wav"), dir.join("b.wav"));
    write_wav(&a, 440.0, 0.6, 48_000);
    write_wav(&b, 880.0, 1.0, 48_000);
    let mut cfg = Config::default();
    cfg.audio.output_device = "none".into(); cfg.audio.normalize = false; cfg.auto.start_in_auto = false; cfg.audio.fade_on_segue = false; cfg.audio.leveler.enabled = false;
    let engine = Engine::start(Arc::new(RwLock::new(cfg)), EventBus::new());
    let tap = engine.add_tap();
    engine.queue_push(QueueItem::new(tr(&a, 1, TrackKind::Asset, 600), "manual", "t"), None);
    engine.queue_push(QueueItem::live_break("Host ve studiu", 0, false, "manual", "t"), None);
    engine.queue_push(QueueItem::new(tr(&b, 2, TrackKind::Asset, 1000), "manual", "t"), None);
    std::thread::sleep(Duration::from_millis(100));
    engine.send(Cmd::Play);
    let rate = engine.sample_rate() as f32;
    let mut out = vec![];
    let t0 = Instant::now();
    let mut continued = false;
    while t0.elapsed() < Duration::from_millis(3200) {
        if !continued && t0.elapsed() > Duration::from_millis(1800) { assert!(engine.live_active(), "živý vstup běží"); assert!(engine.live_continue()); continued = true; }
        if let Ok(bk) = tap.recv_timeout(Duration::from_millis(100)) { out.extend_from_slice(&bk); }
    }
    let mono: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
    let start = mono.iter().position(|x| x.abs() > 0.005).unwrap();
    let p = |a: f32, f: f32| { let s = start + (a * rate) as usize; power(&mono[s..(s + (0.2 * rate) as usize).min(mono.len())], f, rate) };
    assert!(p(0.2, 440.0) > 1e-5, "A hraje");
    assert!(p(1.0, 880.0) < 1e-6 && p(1.0, 440.0) < 1e-6, "během živého vstupu ticho (B nezačalo)");
    assert!(p(2.2, 880.0) > 1e-5, "po Pokračovat hraje B");
    assert!(!engine.live_active());
    engine.send(Cmd::Shutdown);
    let _ = std::fs::remove_dir_all(&dir);
}
