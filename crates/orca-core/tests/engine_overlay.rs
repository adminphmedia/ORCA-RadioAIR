//! Sweeper přes skladbu (pozice "start") a "before_end".

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
fn sweeper_plays_over_song_at_end() {
    let dir = std::env::temp_dir().join(format!("orca-ov-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (song, sw) = (dir.join("s.wav"), dir.join("w.wav"));
    write_wav(&song, 440.0, 3.0, 48_000);
    write_wav(&sw, 1000.0, 1.0, 48_000);
    let mut cfg = Config::default();
    cfg.audio.output_device = "none".into(); cfg.audio.normalize = false; cfg.auto.start_in_auto = false;
    let engine = Engine::start(Arc::new(RwLock::new(cfg)), EventBus::new());
    let tap = engine.add_tap();
    engine.queue_push(QueueItem::new(tr(&song, 1, TrackKind::Song, 3000), "manual", "t"), None);
    let mut ov = QueueItem::new(tr(&sw, 2, TrackKind::Sweeper, 1000), "manual", "t");
    ov.position = Some("before_end".into()); // má skončit s koncem skladby → hraje 2.0–3.0 s
    engine.queue_push(ov, None);
    std::thread::sleep(Duration::from_millis(100));
    engine.send(Cmd::Play);
    let rate = engine.sample_rate() as f32;
    let mut out = vec![];
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(3600) { if let Ok(b) = tap.recv_timeout(Duration::from_millis(200)) { out.extend_from_slice(&b); } }
    let mono: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
    let start = mono.iter().position(|x| x.abs() > 0.01).unwrap();
    let win = |a: f32, l: f32| { let s = start + (a * rate) as usize; &mono[s..(s + (l * rate) as usize).min(mono.len())] };
    assert!(power(win(0.5, 0.5), 1000.0, rate) < 1e-5, "sweeper nesmí hrát na začátku");
    let (p_song, p_sw) = (power(win(2.3, 0.4), 440.0, rate), power(win(2.3, 0.4), 1000.0, rate));
    assert!(p_sw > 1e-4 && p_song > 1e-5, "na konci hrají obě (sweeper přes ztlumenou skladbu): song={p_song} sweeper={p_sw}");
    assert!(engine.queue_snapshot().is_empty(), "sweeper se z fronty připojil ke skladbě");
    engine.send(Cmd::Shutdown);
    let _ = std::fs::remove_dir_all(&dir);
}
