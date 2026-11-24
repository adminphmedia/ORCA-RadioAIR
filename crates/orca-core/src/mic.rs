//! Mikrofon studia: výběr vstupu (USB mikrofon, vstup zvukové karty, audio rozhraní …) a jeho přimíchání
//! do programu. Režim „duck“ jen ztlumí hudbu (mikrofon jde přes mixážní pult), režim „mix“ mikrofon
//! zachytí přes počítač a při zapnutém mikrofonu ho přimíchá do vysílání (zvukovka i streamy).

use crate::config::Config;
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MicConfig {
    /// duck = mikrofon jde přes pult, ORCA jen ztlumí hudbu; mix = mikrofon přes počítač do vysílání
    pub mode: String,
    /// Vstupní zařízení (prázdné = výchozí vstup systému)
    pub device: String,
    /// auto | left | right | stereo – který kanál vstupu je mikrofon
    pub channel: String,
    /// Zesílení mikrofonu (dB)
    pub gain_db: f32,
    /// Šumová brána: pod touto úrovní (dBFS) se mikrofon ztiší; -90 = vypnuto
    pub gate_db: f32,
    /// Náběh / doběh při zapnutí a vypnutí (ms)
    pub ramp_ms: u32,
}

impl Default for MicConfig {
    fn default() -> Self { Self { mode: "duck".into(), device: String::new(), channel: "auto".into(), gain_db: 0.0, gate_db: -90.0, ramp_ms: 40 } }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MicStatus {
    /// off (režim duck) | starting | ready | error
    pub state: String,
    pub device: String,
    pub mode: String,
    pub channels: u16,
    pub rate: u32,
    pub message: String,
    /// Úroveň vstupu (špička, dBFS) – i když je mikrofon vypnutý (pro nastavení zisku)
    pub level_db: f32,
    /// Mikrofon je právě ve vysílání
    pub on_air: bool,
}

struct Bus {
    cons: Mutex<Option<rtrb::Consumer<f32>>>,
    status: RwLock<MicStatus>,
    level: AtomicU32,
    sig: Mutex<String>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
    gain: Mutex<(f32, f32)>,
    params: RwLock<(f32, f32, f32)>,
}

fn bus() -> &'static Bus {
    static B: OnceLock<Bus> = OnceLock::new();
    B.get_or_init(|| Bus {
        cons: Mutex::new(None), status: RwLock::new(MicStatus { state: "off".into(), mode: "duck".into(), level_db: -90.0, ..Default::default() }),
        level: AtomicU32::new((-90.0f32).to_bits()), sig: Mutex::new(String::new()), stop: Mutex::new(None), gain: Mutex::new((0.0, 1.0)), params: RwLock::new((1.0, 0.0, 0.002)),
    })
}

pub fn status() -> MicStatus {
    let mut s = bus().status.read().clone();
    s.level_db = f32::from_bits(bus().level.load(Ordering::Relaxed));
    s.on_air = bus().gain.lock().0 > 0.01;
    s
}

/// Hlídá nastavení a podle něj spouští / zastavuje záznam ze vstupu (volá se z vlákna každých 500 ms).
pub fn start(cfg: Arc<RwLock<Config>>, engine_rate: Arc<dyn Fn() -> u32 + Send + Sync>) {
    std::thread::Builder::new().name("orca-mic".into()).spawn(move || loop {
        let c = cfg.read().mic.clone();
        ensure(&c, engine_rate());
        std::thread::sleep(Duration::from_millis(500));
    }).ok();
}

fn ensure(c: &MicConfig, rate: u32) {
    if rate == 0 { return; }
    let b = bus();
    let lin = |db: f32| 10f32.powf(db / 20.0);
    *b.params.write() = (lin(c.gain_db), if c.gate_db <= -89.0 { 0.0 } else { lin(c.gate_db) }, 1.0 / (rate.max(8000) as f32 * c.ramp_ms.max(5) as f32 / 1000.0));
    let sig = format!("{}|{}|{}|{rate}", c.mode, c.device, c.channel);
    if *b.sig.lock() == sig { return; }
    *b.sig.lock() = sig;
    if let Some(s) = b.stop.lock().take() { s.store(true, Ordering::SeqCst); }
    *b.cons.lock() = None;
    if c.mode != "mix" {
        *b.status.write() = MicStatus { state: "off".into(), mode: c.mode.clone(), device: c.device.clone(), level_db: -90.0, ..Default::default() };
        b.level.store((-90.0f32).to_bits(), Ordering::Relaxed);
        return;
    }
    *b.status.write() = MicStatus { state: "starting".into(), mode: c.mode.clone(), device: c.device.clone(), level_db: -90.0, ..Default::default() };
    let stop = Arc::new(AtomicBool::new(false));
    *b.stop.lock() = Some(stop.clone());
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(rate as usize * 2);
    *b.cons.lock() = Some(cons);
    let (device, channel) = (c.device.clone(), c.channel.clone());
    std::thread::Builder::new().name("orca-mic-input".into()).spawn(move || {
        if let Err(e) = capture(&device, &channel, rate, prod, stop) {
            log::warn!("mikrofon: {e}");
            let mut s = bus().status.write();
            s.state = "error".into();
            s.message = e.to_string();
        }
    }).ok();
}

fn capture(device: &str, channel: &str, rate: u32, mut prod: rtrb::Producer<f32>, stop: Arc<AtomicBool>) -> Result<()> {
    let host = cpal::default_host();
    let dev = if device.is_empty() { host.default_input_device().ok_or_else(|| anyhow!("v počítači není žádný zvukový vstup"))? }
        else { host.input_devices()?.find(|d| d.name().map(|n| n == device).unwrap_or(false)).ok_or_else(|| anyhow!("vstup „{device}“ není připojený"))? };
    let name = dev.name().unwrap_or_default();
    let cfgi = dev.default_input_config()?;
    let sc: cpal::StreamConfig = cfgi.config();
    let ch = sc.channels as usize;
    let pick = match channel { "left" => 0, "right" => 1, "stereo" => 2, _ => if ch == 1 { 0 } else { 3 } };
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(64);
    let st = match cfgi.sample_format() {
        SampleFormat::F32 => build::<f32>(&dev, &sc, ch, pick, tx)?,
        SampleFormat::I16 => build::<i16>(&dev, &sc, ch, pick, tx)?,
        SampleFormat::I32 => build::<i32>(&dev, &sc, ch, pick, tx)?,
        SampleFormat::U16 => build::<u16>(&dev, &sc, ch, pick, tx)?,
        f => return Err(anyhow!("nepodporovaný formát vstupu {f:?}")),
    };
    st.play()?;
    {
        let mut s = bus().status.write();
        s.state = "ready".into(); s.device = name; s.channels = sc.channels; s.rate = sc.sample_rate.0; s.message.clear();
    }
    let mut rs = crate::relay::Resampler::new(sc.sample_rate.0, rate);
    let mut peak_hold = -90.0f32;
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(v) => {
                let out = rs.process(&v);
                let gain = bus().params.read().0;
                let pk = out.iter().fold(0.0f32, |m, x| m.max(x.abs())) * gain;
                let db = if pk > 0.0 { 20.0 * pk.log10() } else { -90.0 };
                peak_hold = if db > peak_hold { db } else { (peak_hold - 1.5).max(db) };
                bus().level.store(peak_hold.max(-90.0).to_bits(), Ordering::Relaxed);
                // nezapisovat, když je buffer plný (mixer nečte) – zahodit nejstarší data by zvyšovalo zpoždění
                if prod.slots() >= out.len() {
                    if let Ok(mut chunk) = prod.write_chunk_uninit(out.len()) {
                        let (a, b) = chunk.as_mut_slices();
                        let (sa, sb) = out.split_at(a.len());
                        for (d, s) in a.iter_mut().zip(sa) { d.write(*s); }
                        for (d, s) in b.iter_mut().zip(sb) { d.write(*s); }
                        unsafe { chunk.commit_all(); }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err(anyhow!("mikrofon se odpojil")),
        }
    }
    drop(st);
    Ok(())
}

fn build<T: SizedSample + Send + 'static>(dev: &cpal::Device, sc: &cpal::StreamConfig, ch: usize, pick: usize, tx: crossbeam_channel::Sender<Vec<f32>>) -> Result<cpal::Stream>
where f32: cpal::FromSample<T> {
    let st = dev.build_input_stream(sc, move |data: &[T], _| {
        let mut v = Vec::with_capacity(data.len() / ch.max(1) * 2);
        for fr in data.chunks(ch.max(1)) {
            let s = |i: usize| -> f32 { <f32 as cpal::FromSample<T>>::from_sample_(fr[i.min(fr.len() - 1)]) };
            let (l, r) = match pick { 0 => (s(0), s(0)), 1 => (s(1), s(1)), 2 => (s(0), s(1)), _ => { let m = (s(0) + s(1)) * 0.5; (m, m) } };
            v.push(l); v.push(r);
        }
        let _ = tx.try_send(v);
    }, |e| log::error!("mikrofon: {e}"), None)?;
    Ok(st)
}

/// Přimíchá mikrofon do bloku programu (volá mixer enginu). `on` = mikrofon zapnutý (talkover).
pub fn mix_into(mix: &mut [f32], on: bool) {
    let b = bus();
    let Some(mut guard) = b.cons.try_lock() else { return };
    let Some(cons) = guard.as_mut() else { return };
    let want = mix.len();
    // držet zpoždění nízké: při nahromadění dat nadbytek zahodit
    let avail = cons.slots();
    if avail > want * 6 { if let Ok(c) = cons.read_chunk(avail - want * 2) { c.commit_all(); } }
    let n = cons.slots().min(want) & !1;
    let (gain, gate, step) = *b.params.read();
    let mut g = b.gain.lock();
    let Ok(chunk) = cons.read_chunk(n) else { return };
    let (a, bb) = chunk.as_slices();
    let pk = a.iter().chain(bb).fold(0.0f32, |m, x| m.max(x.abs())) * gain;
    // brána s výdrží: g.1 = otevření brány 0..1
    let open = gate == 0.0 || pk >= gate;
    g.1 = if open { 1.0 } else { (g.1 - 0.02).max(0.0) };
    let target = if on { gain * g.1 } else { 0.0 };
    for (i, s) in a.iter().chain(bb).enumerate() {
        if i % 2 == 0 { let d = target - g.0; g.0 += d.clamp(-step * gain.max(1.0), step * gain.max(1.0)); }
        if g.0 > 0.0 { mix[i] += s * g.0; }
    }
    chunk.commit_all();
}

pub fn devices() -> Vec<crate::recorder::InputDevice> { crate::recorder::list_input_devices() }
