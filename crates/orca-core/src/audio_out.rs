//! Výstup na zvukovou kartu (CoreAudio na macOS, WASAPI na Windows, ALSA na Linuxu) přes cpal.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo { pub name: String, pub is_default: bool, pub channels: u16, pub default_rate: u32 }

pub fn list_output_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let def = host.default_output_device().and_then(|d| d.name().ok());
    let mut v = vec![];
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            let Ok(name) = d.name() else { continue };
            let (ch, rate) = d.default_output_config().map(|c| (c.channels(), c.sample_rate().0)).unwrap_or((2, 48000));
            v.push(DeviceInfo { is_default: def.as_deref() == Some(name.as_str()), name, channels: ch, default_rate: rate });
        }
    }
    v
}

/// Běžící výstup. Stream žije ve vlastním vlákně (cpal::Stream není Send).
pub struct OutputHandle {
    pub producer: rtrb::Producer<f32>,
    pub sample_rate: u32,
    pub device_name: String,
    pub failed: Arc<AtomicBool>,
    pub underruns: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl Drop for OutputHandle {
    fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); }
}

fn find_device(name: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if name.is_empty() || name == "default" {
        return host.default_output_device().ok_or_else(|| anyhow!("žádné výchozí výstupní zařízení"));
    }
    for d in host.output_devices()? {
        if d.name().map(|n| n == name).unwrap_or(false) { return Ok(d); }
    }
    Err(anyhow!("zařízení '{name}' nenalezeno"))
}

/// Otevře výstup. `want_rate` = požadovaná frekvence (pokud ji zařízení umí).
pub fn open_output(name: &str, want_rate: u32, buffer_ms: u32) -> Result<OutputHandle> {
    let device = find_device(name)?;
    let device_name = device.name().unwrap_or_default();
    let def = device.default_output_config()?;
    let mut chosen = None;
    if let Ok(cfgs) = device.supported_output_configs() {
        for c in cfgs {
            if c.min_sample_rate().0 <= want_rate && c.max_sample_rate().0 >= want_rate && c.channels() >= 2 {
                let better = match &chosen { None => true, Some(prev) => fmt_rank(c.sample_format()) > fmt_rank(prev_fmt(prev)) };
                if better { chosen = Some(c.with_sample_rate(cpal::SampleRate(want_rate))); }
            }
        }
    }
    let supported = chosen.unwrap_or(def);
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.config();
    let rate = config.sample_rate.0;
    let channels = config.channels as usize;
    let cap = ((rate as u64 * buffer_ms.max(40) as u64 / 1000) as usize * 2).max(4096);
    let (producer, consumer) = rtrb::RingBuffer::<f32>::new(cap);
    let failed = Arc::new(AtomicBool::new(false));
    let underruns = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
    {
        let (failed, underruns, stop) = (failed.clone(), underruns.clone(), stop.clone());
        std::thread::Builder::new().name("orca-audio-out".into()).spawn(move || {
            let res = match sample_format {
                SampleFormat::F32 => build::<f32>(&device, &config, channels, consumer, failed.clone(), underruns),
                SampleFormat::I16 => build::<i16>(&device, &config, channels, consumer, failed.clone(), underruns),
                SampleFormat::I32 => build::<i32>(&device, &config, channels, consumer, failed.clone(), underruns),
                SampleFormat::U16 => build::<u16>(&device, &config, channels, consumer, failed.clone(), underruns),
                f => Err(anyhow!("nepodporovaný formát vzorků {f:?}")),
            };
            match res {
                Ok(stream) => {
                    if let Err(e) = stream.play() { let _ = ready_tx.send(Err(e.into())); return; }
                    let _ = ready_tx.send(Ok(()));
                    while !stop.load(Ordering::Relaxed) && !failed.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    drop(stream);
                }
                Err(e) => { let _ = ready_tx.send(Err(e)); }
            }
        })?;
    }
    ready_rx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow!("zvukové zařízení neodpovídá"))??;
    Ok(OutputHandle { producer, sample_rate: rate, device_name, failed, underruns, stop })
}

fn fmt_rank(f: SampleFormat) -> u8 { match f { SampleFormat::F32 => 4, SampleFormat::I32 => 3, SampleFormat::I16 => 2, _ => 1 } }
fn prev_fmt(c: &cpal::SupportedStreamConfig) -> SampleFormat { c.sample_format() }

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, channels: usize, mut cons: rtrb::Consumer<f32>, failed: Arc<AtomicBool>, underruns: Arc<AtomicU64>) -> Result<cpal::Stream>
where T: SizedSample + FromSample<f32> {
    let err_failed = failed.clone();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            let avail = cons.slots() / 2;
            let n = frames.min(avail);
            if n < frames { underruns.fetch_add(1, Ordering::Relaxed); }
            if let Ok(chunk) = cons.read_chunk(n * 2) {
                let (a, b) = chunk.as_slices();
                let mut it = a.iter().chain(b.iter());
                for f in 0..n {
                    let l = *it.next().unwrap_or(&0.0);
                    let r = *it.next().unwrap_or(&0.0);
                    let base = f * channels;
                    if channels == 1 {
                        data[base] = T::from_sample((l + r) * 0.5);
                    } else {
                        data[base] = T::from_sample(l);
                        data[base + 1] = T::from_sample(r);
                        for c in 2..channels { data[base + c] = T::from_sample(0.0f32); }
                    }
                }
                chunk.commit_all();
            }
            for s in data[n * channels..].iter_mut() { *s = T::from_sample(0.0f32); }
        },
        move |e| { log::error!("chyba zvukového výstupu: {e}"); err_failed.store(true, Ordering::SeqCst); },
        None,
    )?;
    Ok(stream)
}
