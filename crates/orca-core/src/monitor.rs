//! Předposlech (monitor / PFL) na samostatné zvukové zařízení – nezasahuje do vysílání.

use crate::audio_out::open_output;
use cpal::traits::{DeviceTrait, HostTrait};
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

/// Kam jde předposlech vzhledem k vysílání. Předposlech NIKDY nesmí hrát na zařízení, kam jde vysílání.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MonitorRoute {
    /// Zařízení vysílání (None = vysílá se jen do streamů, bez zvukové karty)
    pub broadcast: Option<String>,
    /// Zařízení instant jinglů (pokud je samostatné)
    pub instants: Option<String>,
    /// Zařízení předposlechu (vyřešené – prázdné nastavení = výchozí výstup systému)
    pub monitor: Option<String>,
    /// Výchozí výstup systému (sem hraje zvuk z okna aplikace / prohlížeče)
    pub system_default: Option<String>,
    /// Předposlech přes engine je bezpečný (jiné zařízení než vysílání)
    pub monitor_safe: bool,
    /// Zvuk z okna aplikace (prohlížeč) je bezpečný – výchozí výstup systému není vysílací
    pub browser_safe: bool,
    pub reason: String,
}

fn resolve(name: &str) -> Option<String> {
    if name == "none" { return None; }
    let host = cpal::default_host();
    if name.is_empty() || name == "default" { return host.default_output_device().and_then(|d| d.name().ok()); }
    host.output_devices().ok()?.filter_map(|d| d.name().ok()).find(|n| n == name)
}

pub fn route(cfg: &Config) -> MonitorRoute {
    let broadcast = resolve(&cfg.audio.output_device);
    let instants = if cfg.audio.instants_device.is_empty() || cfg.audio.instants_device == "none" { None } else { resolve(&cfg.audio.instants_device) };
    let system_default = cpal::default_host().default_output_device().and_then(|d| d.name().ok());
    let monitor = resolve(&cfg.audio.monitor_device);
    let air = |d: &Option<String>| d.is_some() && (*d == broadcast || *d == instants);
    let monitor_safe = monitor.is_some() && !air(&monitor);
    let browser_safe = system_default.is_some() && !air(&system_default);
    let reason = if monitor.is_none() {
        if cfg.audio.monitor_device.is_empty() { "V počítači není žádný zvukový výstup pro předposlech.".into() } else { format!("Zařízení pro předposlech „{}“ není připojené.", cfg.audio.monitor_device) }
    } else if !monitor_safe {
        format!("Předposlech by hrál do vysílání – „{}“ je zároveň výstup vysílání. V Ovládací panel → Audio → Předposlech zvolte jiný výstup (sluchátka, druhý výstup zvukové karty).", monitor.clone().unwrap_or_default())
    } else { String::new() };
    MonitorRoute { broadcast, instants, monitor, system_default, monitor_safe, browser_safe, reason }
}

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
        let r = route(cfg);
        if !r.monitor_safe { anyhow::bail!("{}", r.reason); }
        let out = open_output(&cfg.audio.monitor_device, 48_000, 200)?;
        // pojistka: otevřené zařízení nesmí být vysílací (např. změna výchozího výstupu systému mezitím)
        if Some(&out.device_name) == r.broadcast.as_ref() || Some(&out.device_name) == r.instants.as_ref() { anyhow::bail!("Předposlech by hrál do vysílání – zvolte pro něj jiný výstup."); }
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
