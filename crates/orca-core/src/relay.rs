//! Rebroadcast: převzetí vysílání z jiného studia – stream (Icecast/SHOUTcast/HLS/SRT/RTP/UDP přes ffmpeg)
//! nebo vstup zvukové karty. Záložní zdroje v řetězu, hlídání ticha a výpadku, návrat k automatice.

use crate::config::{Config, RelaySource};
use crate::model::{QueueItem, Track, TrackKind};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Délka převzetí bez konce (ukončí se ručně „Další“ / událostí relay_stop).
pub const OPEN_END_MS: i64 = 6 * 3600 * 1000;

#[derive(Debug, Clone, Serialize, Default)]
pub struct RelayStatus {
    pub id: String,
    pub name: String,
    /// idle | connecting | on_air | failed | test
    pub state: String,
    /// Zdroj, ze kterého se právě hraje (může být záložní)
    pub active_source: String,
    pub level_db: f32,
    pub since: Option<String>,
    pub message: String,
}

fn registry() -> &'static Mutex<HashMap<String, RelayStatus>> {
    static R: OnceLock<Mutex<HashMap<String, RelayStatus>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn statuses(cfg: &Config) -> Vec<RelayStatus> {
    let r = registry().lock();
    cfg.relay.sources.iter().map(|s| r.get(&s.id).cloned().unwrap_or(RelayStatus { id: s.id.clone(), name: s.name.clone(), state: "idle".into(), ..Default::default() })).collect()
}

fn set_status(id: &str, f: impl FnOnce(&mut RelayStatus)) {
    let mut r = registry().lock();
    let e = r.entry(id.to_string()).or_insert_with(|| RelayStatus { id: id.to_string(), state: "idle".into(), ..Default::default() });
    f(e);
}

/// Položka fronty pro převzetí vysílání.
pub fn queue_item(cfg: &Config, source_id: &str, duration_ms: i64, reason: &str) -> Option<QueueItem> {
    let src = cfg.relay.sources.iter().find(|s| s.id == source_id || s.name == source_id)?;
    let dur = if duration_ms > 0 { duration_ms } else { OPEN_END_MS };
    let t = Track {
        id: 0, kind: Some(TrackKind::Asset), title: format!("Rebroadcast: {}", src.name), artist: String::new(), filename: format!("relay-{}", src.id),
        path: String::new(), duration_ms: dur, cue_out_ms: dur, mix_ms: dur, enabled: true, analyzed: true, ..Default::default()
    };
    let mut q = QueueItem::new(t, "relay", reason.to_string());
    q.relay = Some(src.id.clone());
    Some(q)
}

/// Řetěz zdrojů: hlavní + záložní (bez opakování).
fn chain(cfg: &Config, id: &str) -> Vec<RelaySource> {
    let mut out: Vec<RelaySource> = vec![];
    let mut todo = vec![id.to_string()];
    while let Some(i) = todo.first().cloned() {
        todo.remove(0);
        if out.iter().any(|s| s.id == i) { continue; }
        if let Some(s) = cfg.relay.sources.iter().find(|s| s.id == i) {
            out.push(s.clone());
            todo.extend(s.backup_ids.iter().cloned());
        }
    }
    out
}

/// Výstup dat do přehrávače (ring buffer). Když hráč nestíhá číst (přednačtení), data zahazujeme – je to živé vysílání.
pub struct Sink {
    pub prod: rtrb::Producer<f32>,
    pub stop: Arc<AtomicBool>,
    pub written: u64,
    pub max_frames: u64,
    /// Přehrávač začal číst (do té doby se data zahazují – připojení předem bez zpoždění vysílání)
    pub live: Arc<AtomicBool>,
}

impl Sink {
    fn is_live(&self) -> bool { self.live.load(Ordering::Relaxed) }
    fn push(&mut self, data: &[f32]) {
        if !self.is_live() { return; }
        let mut slice = &data[..data.len() & !1];
        let remaining = ((self.max_frames.saturating_sub(self.written)) * 2) as usize;
        if slice.len() > remaining { slice = &slice[..remaining]; }
        let n = self.prod.slots().min(slice.len()) & !1;
        if n > 0 {
            if let Ok(mut chunk) = self.prod.write_chunk_uninit(n) {
                let (a, b) = chunk.as_mut_slices();
                let (sa, sb) = slice[..n].split_at(a.len());
                for (d, s) in a.iter_mut().zip(sa) { d.write(*s); }
                for (d, s) in b.iter_mut().zip(sb) { d.write(*s); }
                unsafe { chunk.commit_all(); }
            }
        }
        // i zahozená data se počítají do času převzetí
        self.written += (slice.len() / 2) as u64;
    }
    fn finished(&self) -> bool { self.stop.load(Ordering::Relaxed) || self.written >= self.max_frames }
}

enum Outcome { Finished, Failed(String) }

/// Hlavní smyčka převzetí – běží ve vlastním vlákně, vrací true když skončila chybou bez zálohy.
pub fn run(cfg: Config, source_id: String, rate: u32, mut sink: Sink) -> bool {
    let srcs = chain(&cfg, &source_id);
    let Some(main) = srcs.first().cloned() else { log::error!("rebroadcast: zdroj {source_id} neexistuje"); return true; };
    let started = chrono::Local::now().to_rfc3339();
    let mut round = 0;
    let failed = loop {
        let mut all_failed = true;
        for (i, s) in srcs.iter().enumerate() {
            if sink.finished() { all_failed = false; break; }
            set_status(&main.id, |st| { st.name = main.name.clone(); st.state = "connecting".into(); st.active_source = s.name.clone(); st.message = String::new(); st.since = Some(started.clone()); });
            let out = match s.typ.as_str() {
                "device" => run_device(s, rate, &mut sink, &main.id),
                _ => run_ffmpeg(&cfg, s, rate, &mut sink, &main.id),
            };
            match out {
                Outcome::Finished => { all_failed = false; break; }
                Outcome::Failed(msg) => {
                    log::warn!("rebroadcast {}: {msg}", s.name);
                    let next = srcs.get(i + 1).map(|n| n.name.clone());
                    set_status(&main.id, |st| { st.message = match &next { Some(n) => format!("{}: {msg} → přepínám na {n}", s.name), None => format!("{}: {msg}", s.name) }; });
                }
            }
        }
        if !all_failed || sink.finished() { break false; }
        round += 1;
        if main.fallback_to_auto || round > 1000 { break true; }
        // zkoušet znovu, dokud převzetí neskončí; mezitím ticho
        let silence = vec![0f32; rate as usize / 5 * 2];
        for _ in 0..15 { if sink.finished() { break; } sink.push(&silence); std::thread::sleep(Duration::from_millis(200)); }
    };
    set_status(&main.id, |st| { st.state = if failed { "failed".into() } else { "idle".into() }; st.level_db = -90.0; });
    failed
}

struct Watch { silence_db: f32, timeout: Duration, last_loud: Instant, started: Instant, got_data: bool, connect: Duration }

impl Watch {
    fn new(s: &RelaySource) -> Self {
        Self { silence_db: s.silence_db, timeout: Duration::from_secs(s.silence_timeout_s as u64), last_loud: Instant::now(), started: Instant::now(), got_data: false, connect: Duration::from_secs(s.connect_timeout_s.max(2) as u64) }
    }
    fn feed(&mut self, data: &[f32], id: &str, name: &str) {
        if !self.got_data { self.got_data = true; self.last_loud = Instant::now(); set_status(id, |st| { st.state = "on_air".into(); st.active_source = name.to_string(); }); }
        let peak = data.iter().fold(0f32, |m, x| m.max(x.abs()));
        let db = if peak > 0.0 { 20.0 * peak.log10() } else { -90.0 };
        if db > self.silence_db { self.last_loud = Instant::now(); }
        set_status(id, |st| st.level_db = db);
    }
    fn check(&self, live: bool) -> Option<String> {
        if !self.got_data && self.started.elapsed() > self.connect { return Some("nepodařilo se připojit (vypršel čas)".into()); }
        if live && self.got_data && !self.timeout.is_zero() && self.last_loud.elapsed() > self.timeout { return Some(format!("ticho déle než {} s", self.timeout.as_secs())); }
        None
    }
}

pub fn ffmpeg_input_args(url: &str, rate: u32) -> Vec<String> {
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into()];
    if url.starts_with("http://") || url.starts_with("https://") {
        a.extend(["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "4"].map(String::from));
    }
    a.extend(["-fflags", "nobuffer", "-i"].map(String::from));
    a.push(url.to_string());
    a.extend(["-vn", "-f", "f32le", "-ac", "2", "-ar"].map(String::from));
    a.push(rate.to_string());
    a.push("pipe:1".into());
    a
}

fn run_ffmpeg(cfg: &Config, s: &RelaySource, rate: u32, sink: &mut Sink, status_id: &str) -> Outcome {
    if s.url.trim().is_empty() { return Outcome::Failed("chybí adresa streamu".into()); }
    let ff = crate::encoder::resolve_ffmpeg(cfg);
    let mut cmd = Command::new(&ff);
    cmd.args(ffmpeg_input_args(s.url.trim(), rate)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
    let mut child = match cmd.spawn() { Ok(c) => c, Err(e) => return Outcome::Failed(format!("ffmpeg nelze spustit ({}): {e}", ff.display())) };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err_text = Arc::new(Mutex::new(String::new()));
    { let et = err_text.clone(); std::thread::spawn(move || { let mut s = String::new(); let _ = stderr.read_to_string(&mut s); *et.lock() = s; }); }
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(64);
    std::thread::Builder::new().name("orca-relay-read".into()).spawn(move || {
        let mut buf = vec![0u8; 8192];
        let mut carry: Vec<u8> = vec![];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    carry.extend_from_slice(&buf[..n]);
                    let whole = carry.len() / 4 * 4;
                    let v: Vec<f32> = carry[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                    carry.drain(..whole);
                    if tx.send(v).is_err() { break; }
                }
            }
        }
    }).ok();
    let gain = crate::engine::db_to_lin(s.gain_db);
    let mut w = Watch::new(s);
    let mut last_data = Instant::now();
    let res = loop {
        if sink.finished() { break Outcome::Finished; }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mut v) => {
                if gain != 1.0 { for x in v.iter_mut() { *x *= gain; } }
                if !sink.is_live() { w.last_loud = Instant::now(); }
                w.feed(&v, status_id, &s.name);
                sink.push(&v);
                last_data = Instant::now();
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if w.got_data && last_data.elapsed() > Duration::from_secs(8) { break Outcome::Failed("stream přestal posílat data".into()); }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(100));
                let e = err_text.lock().trim().lines().last().unwrap_or("").to_string();
                break Outcome::Failed(if e.is_empty() { "stream skončil".into() } else { e });
            }
        }
        if let Some(m) = w.check(sink.is_live()) { break Outcome::Failed(m); }
    };
    let _ = child.kill();
    let _ = child.wait();
    res
}

fn run_device(s: &RelaySource, rate: u32, sink: &mut Sink, status_id: &str) -> Outcome {
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(256);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32>>(1);
    {
        let name = s.device.clone();
        let stop = stop.clone();
        std::thread::Builder::new().name("orca-relay-input".into()).spawn(move || {
            let r = (|| -> Result<(cpal::Stream, u32)> {
                let host = cpal::default_host();
                let dev = if name.is_empty() { host.default_input_device().ok_or_else(|| anyhow!("žádný vstup zvukové karty"))? }
                    else { host.input_devices()?.find(|d| d.name().map(|n| n == name).unwrap_or(false)).ok_or_else(|| anyhow!("vstup '{name}' nenalezen"))? };
                let c = dev.default_input_config()?;
                let sc: cpal::StreamConfig = c.config();
                let ch = sc.channels as usize;
                let st = match c.sample_format() {
                    SampleFormat::F32 => build::<f32>(&dev, &sc, ch, tx)?,
                    SampleFormat::I16 => build::<i16>(&dev, &sc, ch, tx)?,
                    SampleFormat::I32 => build::<i32>(&dev, &sc, ch, tx)?,
                    SampleFormat::U16 => build::<u16>(&dev, &sc, ch, tx)?,
                    f => return Err(anyhow!("nepodporovaný formát {f:?}")),
                };
                st.play()?;
                Ok((st, sc.sample_rate.0))
            })();
            match r {
                Ok((stream, r)) => { let _ = ready_tx.send(Ok(r)); while !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(50)); } drop(stream); }
                Err(e) => { let _ = ready_tx.send(Err(e)); }
            }
        }).ok();
    }
    let in_rate = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Outcome::Failed(format!("vstup zvukové karty: {e}")),
        Err(_) => { stop.store(true, Ordering::SeqCst); return Outcome::Failed("vstup zvukové karty neodpovídá".into()); }
    };
    let gain = crate::engine::db_to_lin(s.gain_db);
    let mut w = Watch::new(s);
    let mut rs = Resampler::new(in_rate, rate);
    let res = loop {
        if sink.finished() { break Outcome::Finished; }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(v) => {
                let mut out = rs.process(&v);
                if gain != 1.0 { for x in out.iter_mut() { *x *= gain; } }
                if !sink.is_live() { w.last_loud = Instant::now(); }
                w.feed(&out, status_id, &s.name);
                sink.push(&out);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => break Outcome::Failed("vstup zvukové karty se odpojil".into()),
        }
        if let Some(m) = w.check(sink.is_live()) { break Outcome::Failed(m); }
    };
    stop.store(true, Ordering::SeqCst);
    res
}

fn build<T: SizedSample + Send + 'static>(dev: &cpal::Device, sc: &cpal::StreamConfig, ch: usize, tx: crossbeam_channel::Sender<Vec<f32>>) -> Result<cpal::Stream>
where f32: cpal::FromSample<T> {
    let st = dev.build_input_stream(sc, move |data: &[T], _| {
        let mut v = Vec::with_capacity(data.len() / ch.max(1) * 2);
        for fr in data.chunks(ch.max(1)) {
            let l: f32 = <f32 as cpal::FromSample<T>>::from_sample_(fr[0]);
            let r: f32 = if ch > 1 { <f32 as cpal::FromSample<T>>::from_sample_(fr[1]) } else { l };
            v.push(l); v.push(r);
        }
        let _ = tx.try_send(v);
    }, |e| log::error!("vstup rebroadcastu: {e}"), None)?;
    Ok(st)
}

/// Jednoduchý lineární převzorkovač (vstup zvukové karty → vzorkovací frekvence enginu).
struct Resampler { ratio: f64, pos: f64, last: [f32; 2] }

impl Resampler {
    fn new(inr: u32, outr: u32) -> Self { Self { ratio: inr as f64 / outr as f64, pos: 0.0, last: [0.0; 2] } }
    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if (self.ratio - 1.0).abs() < 1e-9 { return input.to_vec(); }
        let frames = input.len() / 2;
        let mut out = Vec::with_capacity((frames as f64 / self.ratio) as usize * 2 + 4);
        let get = |i: isize, c: usize, last: &[f32; 2]| -> f32 { if i < 0 { last[c] } else { input[i as usize * 2 + c] } };
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize;
            let f = (self.pos - i as f64) as f32;
            for c in 0..2 {
                let a = get(i - 1, c, &self.last);
                let b = get(i, c, &self.last);
                out.push(a + (b - a) * f);
            }
            self.pos += self.ratio;
        }
        self.pos -= frames as f64;
        if frames > 0 { self.last = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]]; }
        out
    }
}

/// Krátký test zdroje: připojí se a změří úroveň (bez vysílání).
pub fn probe(cfg: &Config, s: &RelaySource, seconds: u32) -> Result<serde_json::Value> {
    let rate = 48000;
    let (prod, mut cons) = rtrb::RingBuffer::<f32>::new(rate as usize * 2 * 2);
    let stop = Arc::new(AtomicBool::new(false));
    let mut sink = Sink { prod, stop: stop.clone(), written: 0, max_frames: u64::MAX / 4, live: Arc::new(AtomicBool::new(true)) };
    // pojistka: připojení + měření
    { let stop = stop.clone(); let secs = seconds.max(1) as u64 + s.connect_timeout_s.max(2) as u64 + 2; std::thread::spawn(move || { std::thread::sleep(Duration::from_secs(secs)); stop.store(true, Ordering::SeqCst); }); }
    let want = rate as u64 * seconds.max(1) as u64;
    let drain = { let stop = stop.clone(); std::thread::spawn(move || { let mut peak = 0f32; let mut n = 0u64;
        while !stop.load(Ordering::Relaxed) { let k = cons.slots(); if let Ok(c) = cons.read_chunk(k) { let (a, b) = c.as_slices(); for x in a.iter().chain(b) { peak = peak.max(x.abs()); } n += k as u64; c.commit_all(); }
        if n / 2 >= want { stop.store(true, Ordering::SeqCst); } std::thread::sleep(Duration::from_millis(20)); }
        (peak, n / 2) }) };
    let id = format!("probe-{}", s.id);
    let out = match s.typ.as_str() { "device" => run_device(s, rate, &mut sink, &id), _ => run_ffmpeg(cfg, s, rate, &mut sink, &id) };
    stop.store(true, Ordering::SeqCst);
    let (peak, frames) = drain.join().unwrap_or((0.0, 0));
    registry().lock().remove(&id);
    let peak_db = if peak > 0.0 { 20.0 * peak.log10() } else { -90.0 };
    Ok(match out {
        Outcome::Finished => serde_json::json!({ "ok": frames > 0 && peak_db > s.silence_db, "seconds": frames as f64 / rate as f64, "peak_db": peak_db, "message": if frames == 0 { "žádná data" } else if peak_db <= s.silence_db { "zdroj je připojený, ale hraje ticho" } else { "zdroj funguje" } }),
        Outcome::Failed(m) => serde_json::json!({ "ok": false, "seconds": frames as f64 / rate as f64, "peak_db": peak_db, "message": m }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampler_len() {
        let mut r = Resampler::new(44100, 48000);
        let out = r.process(&vec![0.5f32; 44100 * 2]);
        assert!((out.len() as i64 / 2 - 48000).abs() < 3, "{}", out.len());
    }
    #[test]
    fn chain_order() {
        let mut cfg = Config::default();
        let a = RelaySource { id: "a".into(), backup_ids: vec!["b".into()], ..Default::default() };
        let b = RelaySource { id: "b".into(), backup_ids: vec!["a".into(), "c".into()], ..Default::default() };
        let c = RelaySource { id: "c".into(), ..Default::default() };
        cfg.relay.sources = vec![a, b, c];
        let ids: Vec<String> = chain(&cfg, "a").into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
