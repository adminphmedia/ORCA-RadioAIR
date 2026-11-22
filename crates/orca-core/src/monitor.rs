//! Předposlech (monitor / PFL) na samostatné zvukové zařízení – nezasahuje do vysílání.

use crate::audio_out::open_output;
use crate::config::Config;
use crate::decode::StereoStream;
use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Default)]
pub struct MonitorStatus { pub playing: bool, pub track_id: Option<i64>, pub position_ms: i64, pub duration_ms: i64, pub device: String, pub error: Option<String> }

#[derive(Default)]
pub struct Monitor {
    stop: Mutex<Option<Arc<AtomicBool>>>,
    status: Arc<RwLock<MonitorStatus>>,
    pos: Arc<AtomicU64>,
}

impl Monitor {
    pub fn status(&self) -> MonitorStatus {
        let mut s = self.status.read().clone();
        s.position_ms = self.pos.load(Ordering::Relaxed) as i64;
        s
    }

    pub fn stop(&self) {
        if let Some(f) = self.stop.lock().take() { f.store(true, Ordering::SeqCst); }
        self.status.write().playing = false;
    }

    pub fn play(&self, cfg: &Config, track_id: Option<i64>, path: PathBuf, start_ms: i64, duration_ms: i64) -> Result<()> {
        self.stop();
        let out = open_output(&cfg.audio.monitor_device, 48_000, 200)?;
        let rate = out.sample_rate;
        let mut stream = StereoStream::open(&path, rate, start_ms)?;
        let flag = Arc::new(AtomicBool::new(false));
        *self.stop.lock() = Some(flag.clone());
        *self.status.write() = MonitorStatus { playing: true, track_id, position_ms: start_ms, duration_ms, device: out.device_name.clone(), error: None };
        self.pos.store(start_ms.max(0) as u64, Ordering::Relaxed);
        let (status, pos) = (self.status.clone(), self.pos.clone());
        std::thread::Builder::new().name("orca-monitor".into()).spawn(move || {
            let mut out = out;
            let mut buf = Vec::new();
            let mut frames: u64 = 0;
            'outer: while !flag.load(Ordering::Relaxed) {
                match stream.read(&mut buf) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => { status.write().error = Some(e.to_string()); break; }
                }
                let mut i = 0;
                while i < buf.len() {
                    if flag.load(Ordering::Relaxed) || out.failed.load(Ordering::Relaxed) { break 'outer; }
                    let n = out.producer.slots().min(buf.len() - i);
                    if n == 0 { std::thread::sleep(Duration::from_millis(5)); continue; }
                    if let Ok(chunk) = out.producer.write_chunk_uninit(n) { chunk.fill_from_iter(buf[i..i + n].iter().copied()); }
                    i += n;
                }
                frames += (buf.len() / 2) as u64;
                pos.store(start_ms.max(0) as u64 + frames * 1000 / rate as u64, Ordering::Relaxed);
            }
            if !flag.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(300)); }
            status.write().playing = false;
        })?;
        Ok(())
    }
}
