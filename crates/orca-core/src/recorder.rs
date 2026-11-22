//! Studio: nahrávání moderátorských vstupů / voice tracků z mikrofonu přímo do knihovny (WAV 16 bit).

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
pub struct InputDevice { pub name: String, pub is_default: bool, pub channels: u16, pub default_rate: u32 }

pub fn list_input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let def = host.default_input_device().and_then(|d| d.name().ok());
    let mut v = vec![];
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            let Ok(name) = d.name() else { continue };
            let (ch, rate) = d.default_input_config().map(|c| (c.channels(), c.sample_rate().0)).unwrap_or((1, 48000));
            v.push(InputDevice { is_default: def.as_deref() == Some(name.as_str()), name, channels: ch, default_rate: rate });
        }
    }
    v
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RecorderStatus {
    pub recording: bool,
    pub device: String,
    pub elapsed_ms: u64,
    pub level_l: f32,
    pub level_r: f32,
    pub path: Option<String>,
    pub kind: String,
    pub category: String,
    pub title: String,
}

struct Session {
    stop: Arc<AtomicBool>,
    path: PathBuf,
    started: Instant,
    device: String,
    kind: String,
    category: String,
    title: String,
    writer_done: crossbeam_channel::Receiver<Result<u64>>,
}

pub struct Recorder {
    session: Mutex<Option<Session>>,
    level_l: Arc<AtomicU32>,
    level_r: Arc<AtomicU32>,
    frames: Arc<AtomicU64>,
}

impl Default for Recorder { fn default() -> Self { Self::new() } }

impl Recorder {
    pub fn new() -> Self {
        Self { session: Mutex::new(None), level_l: Arc::new(AtomicU32::new(0)), level_r: Arc::new(AtomicU32::new(0)), frames: Arc::new(AtomicU64::new(0)) }
    }

    pub fn status(&self) -> RecorderStatus {
        let s = self.session.lock();
        match s.as_ref() {
            Some(x) => RecorderStatus {
                recording: true, device: x.device.clone(), elapsed_ms: x.started.elapsed().as_millis() as u64,
                level_l: f32::from_bits(self.level_l.load(Ordering::Relaxed)), level_r: f32::from_bits(self.level_r.load(Ordering::Relaxed)),
                path: Some(x.path.to_string_lossy().to_string()), kind: x.kind.clone(), category: x.category.clone(), title: x.title.clone(),
            },
            None => RecorderStatus::default(),
        }
    }

    /// Spustí nahrávání do `path` (WAV). Stream žije ve vlastním vlákně.
    pub fn start(&self, device_name: &str, path: &Path, mono: bool, kind: &str, category: &str, title: &str) -> Result<()> {
        if self.session.lock().is_some() { return Err(anyhow!("nahrávání už běží")); }
        let host = cpal::default_host();
        let device = if device_name.is_empty() {
            host.default_input_device().ok_or_else(|| anyhow!("žádný vstup (mikrofon) není k dispozici"))?
        } else {
            host.input_devices()?.find(|d| d.name().map(|n| n == device_name).unwrap_or(false)).ok_or_else(|| anyhow!("vstup '{device_name}' nenalezen"))?
        };
        let dev_name = device.name().unwrap_or_default();
        let cfg = device.default_input_config().map_err(|e| anyhow!("mikrofon / vstup '{dev_name}' nelze otevřít ({e})"))?;
        let fmt = cfg.sample_format();
        let sc: cpal::StreamConfig = cfg.config();
        let in_ch = sc.channels as usize;
        let out_ch: u16 = if mono || in_ch == 1 { 1 } else { 2 };
        if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
        let spec = hound::WavSpec { channels: out_ch, sample_rate: sc.sample_rate.0, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut writer = hound::WavWriter::create(path, spec)?;
        let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(512);
        let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<u64>>(1);
        // zapisovací vlákno
        std::thread::Builder::new().name("orca-rec-writer".into()).spawn(move || {
            let mut n: u64 = 0;
            let res = (|| -> Result<u64> {
                while let Ok(buf) = rx.recv() {
                    for s in buf { writer.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?; n += 1; }
                }
                writer.finalize()?;
                Ok(n / out_ch as u64)
            })();
            let _ = done_tx.send(res);
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let (ll, lr, frames) = (self.level_l.clone(), self.level_r.clone(), self.frames.clone());
        frames.store(0, Ordering::Relaxed);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        {
            let stop = stop.clone();
            std::thread::Builder::new().name("orca-rec-input".into()).spawn(move || {
                let built = match fmt {
                    SampleFormat::F32 => build::<f32>(&device, &sc, in_ch, out_ch as usize, tx, ll, lr, frames),
                    SampleFormat::I16 => build::<i16>(&device, &sc, in_ch, out_ch as usize, tx, ll, lr, frames),
                    SampleFormat::I32 => build::<i32>(&device, &sc, in_ch, out_ch as usize, tx, ll, lr, frames),
                    SampleFormat::U16 => build::<u16>(&device, &sc, in_ch, out_ch as usize, tx, ll, lr, frames),
                    f => Err(anyhow!("nepodporovaný formát vstupu {f:?}")),
                };
                match built {
                    Ok(stream) => {
                        if let Err(e) = stream.play() { let _ = ready_tx.send(Err(e.into())); return; }
                        let _ = ready_tx.send(Ok(()));
                        while !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(50)); }
                        drop(stream); // uzavře kanál → zapisovač dokončí soubor
                    }
                    Err(e) => { let _ = ready_tx.send(Err(anyhow!("mikrofon / vstup nelze spustit ({e})"))); }
                }
            })?;
        }
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => { let _ = std::fs::remove_file(path); return Err(e); }
            Err(_) => { stop.store(true, Ordering::SeqCst); return Err(anyhow!("vstupní zařízení neodpovídá")); }
        }
        *self.session.lock() = Some(Session {
            stop, path: path.to_path_buf(), started: Instant::now(), device: dev_name,
            kind: kind.into(), category: category.into(), title: title.into(), writer_done: done_rx,
        });
        Ok(())
    }

    /// Ukončí nahrávání. Vrací (cesta, kategorie, délka ms). `discard` = soubor smazat.
    pub fn stop(&self, discard: bool) -> Result<Option<(PathBuf, String, u64)>> {
        let Some(s) = self.session.lock().take() else { return Ok(None) };
        s.stop.store(true, Ordering::SeqCst);
        let res = s.writer_done.recv_timeout(Duration::from_secs(10)).map_err(|_| anyhow!("zápis nahrávky neskončil"))?;
        self.level_l.store(0, Ordering::Relaxed);
        self.level_r.store(0, Ordering::Relaxed);
        let ms = s.started.elapsed().as_millis() as u64;
        res?;
        if discard { let _ = std::fs::remove_file(&s.path); return Ok(None); }
        Ok(Some((s.path, s.category, ms)))
    }
}

#[allow(clippy::too_many_arguments)]
fn build<T>(device: &cpal::Device, sc: &cpal::StreamConfig, in_ch: usize, out_ch: usize, tx: crossbeam_channel::Sender<Vec<f32>>,
    ll: Arc<AtomicU32>, lr: Arc<AtomicU32>, frames: Arc<AtomicU64>) -> Result<cpal::Stream>
where T: SizedSample, f32: cpal::FromSample<T> {
    use cpal::Sample;
    let stream = device.build_input_stream(
        sc,
        move |data: &[T], _| {
            let n = data.len() / in_ch.max(1);
            let mut out = Vec::with_capacity(n * out_ch);
            let (mut pl, mut pr) = (0f32, 0f32);
            for f in 0..n {
                let l: f32 = f32::from_sample(data[f * in_ch]);
                let r: f32 = if in_ch > 1 { f32::from_sample(data[f * in_ch + 1]) } else { l };
                pl = pl.max(l.abs()); pr = pr.max(r.abs());
                if out_ch == 1 { out.push(if in_ch > 1 { (l + r) * 0.5 } else { l }); } else { out.push(l); out.push(r); }
            }
            let decay = |a: &AtomicU32, p: f32| { let cur = f32::from_bits(a.load(Ordering::Relaxed)); a.store(p.max(cur * 0.9).to_bits(), Ordering::Relaxed); };
            decay(&ll, pl); decay(&lr, pr);
            frames.fetch_add(n as u64, Ordering::Relaxed);
            let _ = tx.try_send(out);
        },
        |e| log::error!("chyba nahrávání: {e}"),
        None,
    )?;
    Ok(stream)
}
