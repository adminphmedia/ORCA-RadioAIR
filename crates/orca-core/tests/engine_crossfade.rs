//! Integrační test mixéru: dvě skladby, crossfade od bodu mixu, odbočka pro encoder.

use orca_core::config::Config;
use orca_core::engine::{Cmd, Engine};
use orca_core::events::{Event, EventBus};
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
    for i in 0..n {
        let v = ((2.0 * PI * freq * i as f32 / rate as f32).sin() * 0.4 * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut f = std::fs::File::create(path).unwrap();
    let byte_rate = rate * 4;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data.len() as u32).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&4u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&data).unwrap();
}

/// Síla frekvence v okně (Goertzel).
fn power(samples: &[f32], freq: f32, rate: f32) -> f32 {
    let k = 2.0 * (2.0 * PI * freq / rate).cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &x in samples { let s0 = x + k * s1 - s2; s2 = s1; s1 = s0; }
    (s1 * s1 + s2 * s2 - k * s1 * s2) / samples.len() as f32
}

fn track(path: &Path, id: i64, dur: i64, mix: i64) -> Track {
    Track { id, kind: Some(TrackKind::Song), path: path.to_string_lossy().into(), filename: "t.wav".into(), title: format!("T{id}"),
        duration_ms: dur, cue_in_ms: 0, mix_ms: mix, cue_out_ms: dur, analyzed: true, enabled: true, ..Default::default() }
}

#[test]
fn crossfade_overlaps_and_sequence_is_gapless() {
    let dir = std::env::temp_dir().join(format!("orca-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    write_wav(&a, 440.0, 3.0, 44_100); // jiná frekvence než engine → test i resampleru
    write_wav(&b, 1000.0, 2.0, 48_000);

    let mut cfg = Config::default();
    cfg.audio.output_device = "none".into();
    cfg.audio.normalize = false;
    cfg.auto.start_in_auto = false;
    let cfg = Arc::new(RwLock::new(cfg));
    let events = EventBus::new();
    let evrx = events.subscribe();
    let engine = Engine::start(cfg, events);
    let tap = engine.add_tap();
    engine.queue_push(QueueItem::new(track(&a, 1, 3000, 2000), "manual", "test"), None); // mix ve 2. sekundě
    engine.queue_push(QueueItem::new(track(&b, 2, 2000, 2000), "manual", "test"), None);
    std::thread::sleep(Duration::from_millis(100));
    engine.send(Cmd::Play);

    let rate = engine.sample_rate() as f32;
    let mut out: Vec<f32> = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(4800) {
        if let Ok(b) = tap.recv_timeout(Duration::from_millis(200)) { out.extend_from_slice(&b); }
    }
    let mono: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
    // najdi začátek zvuku
    let start = mono.iter().position(|x| x.abs() > 0.01).expect("žádný zvuk");
    let win = |from_s: f32, len_s: f32| {
        let s = start + (from_s * rate) as usize;
        &mono[s..(s + (len_s * rate) as usize).min(mono.len())]
    };
    let (a1, b1) = (power(win(0.5, 0.5), 440.0, rate), power(win(0.5, 0.5), 1000.0, rate));
    assert!(a1 > b1 * 100.0, "na začátku má hrát jen A: {a1} vs {b1}");
    // 2.3–2.6 s: crossfade – musí znít obě
    let (a2, b2) = (power(win(2.3, 0.3), 440.0, rate), power(win(2.3, 0.3), 1000.0, rate));
    assert!(a2 > 1e-4 && b2 > 1e-4, "v crossfadu musí znít obě skladby: A={a2} B={b2}");
    // 3.3–3.8 s: jen B
    let (a3, b3) = (power(win(3.3, 0.5), 440.0, rate), power(win(3.3, 0.5), 1000.0, rate));
    assert!(b3 > a3 * 100.0, "na konci má hrát jen B: {a3} vs {b3}");
    // žádná díra delší než 20 ms mezi začátkem A a koncem B (celkem 4 s)
    let end = start + (3.95 * rate) as usize;
    let mut silent_run = 0usize;
    let mut max_run = 0usize;
    for &x in &mono[start..end.min(mono.len())] {
        if x.abs() < 1e-4 { silent_run += 1; max_run = max_run.max(silent_run); } else { silent_run = 0; }
    }
    assert!(max_run < (0.02 * rate) as usize, "díra v signálu: {} ms", max_run as f32 * 1000.0 / rate);

    let evs: Vec<Event> = evrx.try_iter().collect();
    let started = evs.iter().filter(|e| matches!(e, Event::TrackStarted { .. })).count();
    assert_eq!(started, 2, "obě skladby musí nahlásit start");
    engine.send(Cmd::Shutdown);
    let _ = std::fs::remove_dir_all(&dir);
}
