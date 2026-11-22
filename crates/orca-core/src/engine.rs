//! Playout engine: fronta (decky), přednačítání, mixér s crossfadem podle cue bodů,
//! overlay stopa (instant jingly), výstup na zvukovku a odbočky pro streamovací encodery.

use crate::audio_out::{open_output, OutputHandle};
use crate::config::Config;
use crate::decode::StereoStream;
use crate::events::{Event, EventBus};
use crate::model::{QueueItem, Track};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const ENGINE_RATE: u32 = 48_000;
const BLOCK: usize = 480; // 10 ms při 48 kHz
const PREFETCH_SECONDS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayState { Stopped, Playing, Paused }

#[derive(Debug, Clone, Serialize)]
pub struct VoiceStatus {
    pub item: QueueItem,
    pub channel: String,
    pub position_ms: i64,
    pub length_ms: i64,
    pub remaining_ms: i64,
    pub mix_at_ms: i64,
    pub fading: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputStatus { pub device: String, pub ok: bool, pub sample_rate: u32, pub underruns: u64, pub message: String }

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub state: PlayState,
    pub stop_after_current: bool,
    pub current: Option<VoiceStatus>,
    pub voices: Vec<VoiceStatus>,
    pub output: OutputStatus,
}

pub enum Cmd {
    Play,
    Stop,
    Next,
    Pause,
    StopAfterCurrent(bool),
    /// Okamžitě přehrát přes hlavní stopu (instant jingle).
    Instant(Track),
    /// Zastavit všechny instant jingly.
    StopInstants,
    ReopenOutput,
    Shutdown,
}

pub struct Engine {
    pub queue: Mutex<VecDeque<QueueItem>>,
    pub status: RwLock<EngineStatus>,
    meter_l: AtomicU32,
    meter_r: AtomicU32,
    taps: Mutex<Vec<Sender<Arc<Vec<f32>>>>>,
    cmd_tx: Sender<Cmd>,
    cfg: Arc<RwLock<Config>>,
    events: EventBus,
    pub rate: AtomicU32,
    /// Načte aktuální verzi skladby z DB (cue body se mohly změnit po zařazení do fronty).
    resolver: RwLock<Option<TrackResolver>>,
    stitch_resolver: RwLock<Option<StitchResolver>>,
}

pub type TrackResolver = Arc<dyn Fn(i64) -> Option<Track> + Send + Sync>;
/// Složí Stitcher v okamžiku přehrání: (typ, položky ve frontě za ním) → plán.
pub type StitchResolver = Arc<dyn Fn(&str, &[QueueItem]) -> anyhow::Result<crate::stitcher::StitchPlan> + Send + Sync>;

/// Sweeper / voice track čekající na svou pozici ve skladbě.
struct PendingOverlay { host_id: String, trigger: u64, item: QueueItem }

// ---------------------------------------------------------------------------------------------
// Přednačítání (dekódování v samostatném vlákně do ring bufferu)

struct Prefetch {
    cons: rtrb::Consumer<f32>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl Drop for Prefetch { fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); } }

impl Prefetch {
    fn start(path: PathBuf, start_ms: i64, max_frames: u64, rate: u32) -> Prefetch {
        Self::start_segments(vec![(path, start_ms, max_frames)], rate)
    }

    /// Více souborů za sebou (Stitcher): (cesta, začátek ms, počet snímků)
    fn start_segments(segments: Vec<(PathBuf, i64, u64)>, rate: u32) -> Prefetch {
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(rate as usize * 2 * PREFETCH_SECONDS);
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (d, s, f) = (done.clone(), stop.clone(), failed.clone());
        let _ = std::thread::Builder::new().name("orca-decode".into()).spawn(move || {
          'segs: for (path, start_ms, max_frames) in segments {
            let mut stream = match StereoStream::open(&path, rate, start_ms) {
                Ok(s) => s,
                Err(e) => { log::error!("nelze přehrát {}: {e}", path.display()); f.store(true, Ordering::SeqCst); continue 'segs; }
            };
            let mut buf = Vec::new();
            let mut written: u64 = 0;
            'outer: while !s.load(Ordering::Relaxed) && written < max_frames {
                match stream.read(&mut buf) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => { log::error!("chyba dekódování {}: {e}", path.display()); break; }
                }
                let remaining = ((max_frames - written) * 2) as usize;
                let mut slice = &buf[..buf.len().min(remaining)];
                while !slice.is_empty() {
                    if s.load(Ordering::Relaxed) { break 'outer; }
                    let n = prod.slots().min(slice.len()) & !1;
                    if n == 0 { std::thread::sleep(Duration::from_millis(5)); continue; }
                    if let Ok(mut chunk) = prod.write_chunk_uninit(n) {
                        let (a, b) = chunk.as_mut_slices();
                        let (sa, sb) = slice[..n].split_at(a.len());
                        for (dst, src) in a.iter_mut().zip(sa) { dst.write(*src); }
                        for (dst, src) in b.iter_mut().zip(sb) { dst.write(*src); }
                        unsafe { chunk.commit_all(); }
                    }
                    written += (n / 2) as u64;
                    slice = &slice[n..];
                }
            }
            if s.load(Ordering::Relaxed) { break 'segs; }
          }
            d.store(true, Ordering::SeqCst);
        });
        Prefetch { cons, done, stop, failed }
    }

    fn buffered_frames(&self) -> usize { self.cons.slots() / 2 }
}

// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Fade { from: f32, to: f32, start: u64, len: u64 }

impl Fade {
    fn gain_at(&self, pos: u64) -> f32 {
        if pos <= self.start { return self.from; }
        let t = ((pos - self.start) as f32 / self.len.max(1) as f32).min(1.0);
        // kosinová křivka – přirozenější než lineární
        let s = 0.5 - 0.5 * (t * std::f32::consts::PI).cos();
        self.from + (self.to - self.from) * s
    }
    fn finished(&self, pos: u64) -> bool { pos >= self.start + self.len }
}

struct Voice {
    item: QueueItem,
    src: Prefetch,
    pos: u64,
    len: u64,
    mix_at: u64,
    gain: f32,
    fade: Option<Fade>,
    segued: bool,
    overlay: bool,
    kill_after_fade: bool,
    underruns: u32,
}

impl Voice {
    fn new(item: QueueItem, src: Prefetch, rate: u32, gain_db: f64, overlay: bool) -> Voice {
        let (cue_in, mix, cue_out) = item.track.effective_cues();
        let ms_to_f = |ms: i64| (ms.max(0) as u64 * rate as u64) / 1000;
        let len = ms_to_f(cue_out - cue_in);
        let mix_at = ms_to_f(mix - cue_in).min(len);
        Voice { item, src, pos: 0, len, mix_at, gain: db_to_lin(gain_db as f32), fade: None, segued: false, overlay, kill_after_fade: false, underruns: 0 }
    }

    fn status(&self, rate: u32) -> VoiceStatus {
        let f_to_ms = |f: u64| (f * 1000 / rate as u64) as i64;
        VoiceStatus {
            item: self.item.clone(),
            channel: if self.overlay { "overlay".into() } else { "main".into() },
            position_ms: f_to_ms(self.pos),
            length_ms: f_to_ms(self.len),
            remaining_ms: f_to_ms(self.len.saturating_sub(self.pos)),
            mix_at_ms: f_to_ms(self.mix_at),
            fading: self.fade.is_some(),
        }
    }

    fn is_finished(&self) -> bool {
        if self.pos >= self.len { return true; }
        if self.kill_after_fade { if let Some(f) = self.fade { if f.finished(self.pos) { return true; } } }
        self.src.done.load(Ordering::Acquire) && self.src.cons.is_empty()
    }

    /// Přimíchá `frames` snímků do `mix`.
    fn render(&mut self, mix: &mut [f32], frames: usize, extra_gain: f32) {
        let want = frames.min((self.len - self.pos.min(self.len)) as usize);
        let avail = self.src.buffered_frames().min(want);
        if avail < want && !self.src.done.load(Ordering::Acquire) { self.underruns += 1; }
        if let Ok(chunk) = self.src.cons.read_chunk(avail * 2) {
            let (a, b) = chunk.as_slices();
            let mut it = a.iter().chain(b.iter());
            for f in 0..avail {
                let g = self.gain * extra_gain * self.fade.map(|fd| fd.gain_at(self.pos + f as u64)).unwrap_or(1.0);
                let l = *it.next().unwrap_or(&0.0);
                let r = *it.next().unwrap_or(&0.0);
                mix[f * 2] += l * g;
                mix[f * 2 + 1] += r * g;
            }
            chunk.commit_all();
        }
        // při podtečení necháme ticho, ale pozici posuneme jen o skutečně přehrané vzorky
        self.pos += avail as u64;
        if avail < want && self.src.done.load(Ordering::Acquire) && self.src.cons.is_empty() {
            self.pos = self.len; // dekodér skončil dřív (kratší soubor než udávaly tagy)
        }
    }
}

pub fn db_to_lin(db: f32) -> f32 { 10f32.powf(db / 20.0) }

fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.9 { x } else { x.signum() * (0.9 + 0.1 * ((a - 0.9) / 0.1).tanh()) }
}

// ---------------------------------------------------------------------------------------------

impl Engine {
    pub fn start(cfg: Arc<RwLock<Config>>, events: EventBus) -> Arc<Engine> {
        let (cmd_tx, cmd_rx) = unbounded();
        let engine = Arc::new(Engine {
            queue: Mutex::new(VecDeque::new()),
            status: RwLock::new(EngineStatus {
                state: PlayState::Stopped,
                stop_after_current: false,
                current: None,
                voices: vec![],
                output: OutputStatus { device: String::new(), ok: false, sample_rate: ENGINE_RATE, underruns: 0, message: String::new() },
            }),
            meter_l: AtomicU32::new(0),
            meter_r: AtomicU32::new(0),
            taps: Mutex::new(vec![]),
            cmd_tx,
            cfg,
            events,
            rate: AtomicU32::new(ENGINE_RATE),
            resolver: RwLock::new(None),
            stitch_resolver: RwLock::new(None),
        });
        let e = engine.clone();
        std::thread::Builder::new().name("orca-mixer".into()).spawn(move || e.mixer_loop(cmd_rx)).expect("mixer thread");
        engine
    }

    pub fn send(&self, c: Cmd) { let _ = self.cmd_tx.send(c); }

    pub fn set_resolver(&self, r: TrackResolver) { *self.resolver.write() = Some(r); }
    pub fn set_stitch_resolver(&self, r: StitchResolver) { *self.stitch_resolver.write() = Some(r); }

    fn refresh(&self, item: &mut QueueItem) {
        if item.track.id <= 0 { return; }
        let r = self.resolver.read().clone();
        if let Some(t) = r.and_then(|r| r(item.track.id)) { item.track = t; }
    }

    pub fn sample_rate(&self) -> u32 { self.rate.load(Ordering::Relaxed) }

    /// Odbočka zvuku pro encoder (prokládané stereo f32 v `sample_rate()`).
    pub fn add_tap(&self) -> Receiver<Arc<Vec<f32>>> {
        let (tx, rx) = bounded(800);
        self.taps.lock().push(tx);
        rx
    }

    /// Úrovně (peak, lineárně 0..1).
    pub fn meters(&self) -> (f32, f32) {
        (f32::from_bits(self.meter_l.load(Ordering::Relaxed)), f32::from_bits(self.meter_r.load(Ordering::Relaxed)))
    }

    pub fn snapshot(&self) -> EngineStatus { self.status.read().clone() }

    fn try_open_output(&self) -> Option<OutputHandle> {
        let (dev, buf) = { let c = self.cfg.read(); (c.audio.output_device.clone(), c.audio.buffer_ms) };
        if dev == "none" {
            let mut st = self.status.write();
            st.output = OutputStatus { device: "none".into(), ok: true, sample_rate: self.sample_rate(), underruns: 0, message: "bez zvukové karty (jen stream)".into() };
            return None;
        }
        match open_output(&dev, ENGINE_RATE, buf) {
            Ok(h) => {
                if h.sample_rate != self.sample_rate() && self.status.read().state != PlayState::Stopped {
                    // za běhu nelze měnit frekvenci enginu – zařízení s jinou frekvencí použijeme až po restartu
                    self.events.alert("warning", "output_rate_mismatch", format!("{} běží na {} Hz", h.device_name, h.sample_rate));
                    return None;
                }
                self.rate.store(h.sample_rate, Ordering::Relaxed);
                let mut st = self.status.write();
                st.output = OutputStatus { device: h.device_name.clone(), ok: true, sample_rate: h.sample_rate, underruns: 0, message: String::new() };
                log::info!("zvukový výstup: {} @ {} Hz", h.device_name, h.sample_rate);
                Some(h)
            }
            Err(e) => {
                let mut st = self.status.write();
                st.output = OutputStatus { device: dev.clone(), ok: false, sample_rate: self.sample_rate(), underruns: 0, message: e.to_string() };
                drop(st);
                self.events.alert("error", "output_unavailable", format!("Zvuková karta nedostupná: {e}. Vysílání do streamu pokračuje."));
                None
            }
        }
    }

    fn make_voice(&self, item: QueueItem, overlay: bool, preloaded: Option<Prefetch>) -> Voice {
        self.make_voice_full(item, overlay, preloaded).0
    }

    /// Hlas + případný podkres (Stitcher).
    fn make_voice_full(&self, mut item: QueueItem, overlay: bool, preloaded: Option<Prefetch>) -> (Voice, Option<Voice>) {
        let rate = self.sample_rate();
        if let Some(kind) = item.stitch.clone() {
            let upcoming = self.queue_snapshot();
            let plan = self.stitch_resolver.read().clone().map(|r| r(&kind, &upcoming));
            let ms_f = |ms: i64| (ms.max(0) as u64 * rate as u64) / 1000;
            return match plan {
                Some(Ok(plan)) => {
                    let total = plan.total_ms();
                    item.track.title = plan.title.clone();
                    item.track.duration_ms = total; item.track.cue_in_ms = 0; item.track.mix_ms = total; item.track.cue_out_ms = total;
                    item.track.lufs = None; item.track.gain_db = 0.0;
                    let segs = plan.segments.iter().map(|sg| (PathBuf::from(&sg.path), sg.start_ms, ms_f(sg.len_ms()))).collect();
                    let v = Voice::new(item.clone(), Prefetch::start_segments(segs, rate), rate, 0.0, overlay);
                    let bed = plan.bed.map(|(sg, gain, len_ms)| {
                        let len_ms = len_ms.min(sg.len_ms());
                        let mut bt = item.track.clone();
                        bt.title = "Stitcher – podkres".into(); bt.path = sg.path.clone(); bt.kind = Some(crate::model::TrackKind::Asset);
                        bt.cue_in_ms = sg.start_ms; bt.cue_out_ms = sg.start_ms + len_ms; bt.mix_ms = bt.cue_out_ms; bt.duration_ms = bt.cue_out_ms;
                        let bi = QueueItem::new(bt, "stitcher-bed", "podkres stitcheru");
                        let mut bv = Voice::new(bi, Prefetch::start(PathBuf::from(&sg.path), sg.start_ms, ms_f(len_ms), rate), rate, gain as f64, true);
                        let fade = ms_f(800).min(bv.len);
                        bv.fade = Some(Fade { from: 1.0, to: 0.0, start: bv.len - fade, len: fade });
                        bv
                    });
                    (v, bed)
                }
                Some(Err(e)) => {
                    self.events.alert("warning", "stitcher_failed", format!("Stitcher ({kind}): {e}"));
                    item.track.duration_ms = 0; item.track.cue_out_ms = 0; item.track.mix_ms = 0;
                    (Voice::new(item, Prefetch::start_segments(vec![], rate), rate, 0.0, overlay), None)
                }
                None => (Voice::new(item, Prefetch::start_segments(vec![], rate), rate, 0.0, overlay), None),
            };
        }
        let (normalize, target) = { let c = self.cfg.read(); (c.audio.normalize, c.audio.target_lufs) };
        let gain_db = item.track.playback_gain_db(normalize, target);
        let (cue_in, _, cue_out) = item.track.effective_cues();
        let max_frames = ((cue_out - cue_in).max(0) as u64 * rate as u64) / 1000;
        let src = preloaded.unwrap_or_else(|| Prefetch::start(PathBuf::from(&item.track.path), cue_in, max_frames, rate));
        // krátké čekání na první data, aby začátek nebyl useknutý
        let t0 = Instant::now();
        while src.buffered_frames() < BLOCK * 4 && !src.done.load(Ordering::Acquire) && t0.elapsed() < Duration::from_millis(40) {
            std::thread::sleep(Duration::from_millis(2));
        }
        (Voice::new(item, src, rate, gain_db, overlay), None)
    }

    /// Kdy (snímek hostitelské skladby) začne sweeper/voice track.
    fn overlay_trigger(host: &Voice, ov: &QueueItem, rate: u32) -> u64 {
        let ms_f = |ms: i64| (ms.max(0) as u64 * rate as u64) / 1000;
        let (ci, _, co) = ov.track.effective_cues();
        let ov_len = ms_f(co - ci);
        let (hci, _, _) = host.item.track.effective_cues();
        let pos = ov.position.as_deref().unwrap_or("start");
        let t = match pos {
            "before_intro" => ms_f(host.item.track.intro_ms - hci).saturating_sub(ov_len),
            "before_end" => host.mix_at.saturating_sub(ov_len),
            "bridge" => host.mix_at.saturating_sub(ov_len / 2),
            p if p.starts_with("custom:") => ms_f(p[7..].trim().parse::<i64>().unwrap_or(0)),
            _ => 0,
        };
        t.max(host.pos)
    }

    fn mixer_loop(self: Arc<Self>, cmd_rx: Receiver<Cmd>) {
        let mut output = self.try_open_output();
        let mut last_reopen = Instant::now();
        let mut voices: Vec<Voice> = Vec::new();
        let mut preloaded: Option<(String, Prefetch)> = None;
        let mut state = if self.cfg.read().auto.start_in_auto { PlayState::Playing } else { PlayState::Stopped };
        let mut stop_after = false;
        let mut mix = vec![0f32; BLOCK * 2];
        let mut clock_start = Instant::now();
        let mut frames_rendered: u64 = 0;
        let (mut lvl_l, mut lvl_r) = (0f32, 0f32);
        let mut duck = 1.0f32;
        let mut pending: Vec<PendingOverlay> = Vec::new();
        let mut instants_out: Option<OutputHandle> = None;
        let mut instants_dev = String::new();
        let mut inst_mix = vec![0f32; BLOCK * 2];

        loop {
            // --- příkazy
            while let Ok(cmd) = cmd_rx.try_recv() {
                let rate = self.sample_rate() as u64;
                let (manual_fade, stop_fade) = { let c = self.cfg.read(); (c.audio.manual_fade_ms as u64, c.audio.stop_fade_ms as u64) };
                match cmd {
                    Cmd::Play => {
                        if state == PlayState::Paused { state = PlayState::Playing; }
                        else if state == PlayState::Stopped { state = PlayState::Playing; stop_after = false; }
                    }
                    Cmd::Pause => { if state == PlayState::Playing { state = PlayState::Paused; } else if state == PlayState::Paused { state = PlayState::Playing; } }
                    Cmd::Stop => {
                        state = PlayState::Stopped;
                        for v in voices.iter_mut().filter(|v| !v.overlay) {
                            v.fade = Some(Fade { from: v.fade.map(|f| f.gain_at(v.pos)).unwrap_or(1.0), to: 0.0, start: v.pos, len: stop_fade * rate / 1000 });
                            v.kill_after_fade = true;
                        }
                    }
                    Cmd::Next => {
                        if state != PlayState::Playing { state = PlayState::Playing; }
                        for v in voices.iter_mut().filter(|v| !v.overlay) {
                            v.fade = Some(Fade { from: v.fade.map(|f| f.gain_at(v.pos)).unwrap_or(1.0), to: 0.0, start: v.pos, len: manual_fade * rate / 1000 });
                            v.kill_after_fade = true;
                            v.segued = true;
                        }
                        stop_after = false;
                        pending.clear();
                        if let Some((v, bed)) = self.start_next(&mut preloaded) { voices.push(v); if let Some(b) = bed { voices.push(b); } }
                    }
                    Cmd::StopAfterCurrent(b) => stop_after = b,
                    Cmd::Instant(t) => {
                        let mut item = QueueItem::new(t, "instant", "instant");
                        self.refresh(&mut item);
                        let v = self.make_voice(item.clone(), true, None);
                        // instant má zahrát vždy, i když je engine zastavený
                        self.events.emit(Event::TrackStarted { item, channel: "overlay".into() });
                        voices.push(v);
                    }
                    Cmd::StopInstants => {
                        for v in voices.iter_mut().filter(|v| v.overlay && v.item.source == "instant") {
                            v.fade = Some(Fade { from: 1.0, to: 0.0, start: v.pos, len: 200 * rate / 1000 });
                            v.kill_after_fade = true;
                        }
                    }
                    Cmd::ReopenOutput => { drop(output.take()); output = self.try_open_output(); clock_start = Instant::now(); frames_rendered = 0; }
                    Cmd::Shutdown => return,
                }
            }

            // --- výpadek zvukovky → pokračujeme podle systémových hodin a zkoušíme znovu
            if let Some(o) = &output {
                if o.failed.load(Ordering::Relaxed) {
                    self.events.alert("error", "output_failed", format!("Zvukový výstup {} selhal, zkouším znovu…", o.device_name));
                    self.status.write().output.ok = false;
                    output = None;
                    clock_start = Instant::now();
                    frames_rendered = 0;
                    last_reopen = Instant::now();
                }
            } else if self.cfg.read().audio.output_device != "none" && last_reopen.elapsed() > Duration::from_secs(5) {
                last_reopen = Instant::now();
                if let Some(o) = self.try_open_output() { output = Some(o); }
            }

            // --- je čas vyrobit další blok?
            let rate = self.sample_rate();
            let can_render = match &output {
                Some(o) => o.producer.slots() >= BLOCK * 2,
                None => {
                    let due = clock_start.elapsed().as_secs_f64() * rate as f64;
                    (frames_rendered as f64) + (BLOCK as f64) <= due + (rate as f64 * 0.05)
                }
            };
            if !can_render {
                // údržba přednačtení během čekání
                self.ensure_preload(&mut preloaded);
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }

            // --- segue / start
            if state == PlayState::Playing {
                let main_count = voices.iter().filter(|v| !v.overlay && !v.kill_after_fade).count();
                let need_next = match voices.iter().rev().find(|v| !v.overlay && !v.segued) {
                    Some(cur) => cur.pos + BLOCK as u64 >= cur.mix_at && !stop_after,
                    None => main_count == 0,
                };
                if need_next && !self.queue.lock().is_empty() {
                    let fade_on_segue = self.cfg.read().audio.fade_on_segue;
                    if let Some(cur) = voices.iter_mut().rev().find(|v| !v.overlay && !v.segued) {
                        cur.segued = true;
                        if fade_on_segue && cur.len > cur.mix_at + BLOCK as u64 {
                            cur.fade = Some(Fade { from: 1.0, to: 0.0, start: cur.mix_at, len: cur.len - cur.mix_at });
                        }
                    }
                    if let Some((v, bed)) = self.start_next(&mut preloaded) { voices.push(v); if let Some(b) = bed { voices.push(b); } }
                }
            }

            // --- sweepery / voice tracky na pozici přes skladbu
            loop {
                let front = { let q = self.queue.lock(); q.front().filter(|f| f.is_attached_overlay()).cloned() };
                let Some(mut ov) = front else { break };
                let Some(host) = voices.iter().rev().find(|v| !v.overlay && !v.kill_after_fade && !v.segued) else { break };
                self.queue.lock().pop_front();
                self.refresh(&mut ov);
                let trigger = Self::overlay_trigger(host, &ov, rate);
                pending.push(PendingOverlay { host_id: host.item.id.clone(), trigger, item: ov });
                self.events.emit(Event::QueueChanged);
            }
            if state == PlayState::Playing {
                let mut k = 0;
                while k < pending.len() {
                    let host = voices.iter().find(|v| v.item.id == pending[k].host_id);
                    let due = match host { Some(h) => h.pos + BLOCK as u64 >= pending[k].trigger, None => true };
                    if due {
                        let p = pending.remove(k);
                        let v = self.make_voice(p.item.clone(), true, None);
                        self.events.emit(Event::TrackStarted { item: p.item, channel: "overlay".into() });
                        voices.push(v);
                    } else { k += 1; }
                }
            }

            // --- samostatný výstup pro instant jingly (Jazler: Instants, Output 3)
            {
                let want = self.cfg.read().audio.instants_device.clone();
                if want != instants_dev {
                    instants_dev = want.clone();
                    instants_out = None;
                    if !want.is_empty() && want != "none" && want != self.cfg.read().audio.output_device {
                        match open_output(&want, rate, self.cfg.read().audio.buffer_ms) {
                            Ok(h) if h.sample_rate == rate => instants_out = Some(h),
                            Ok(h) => self.events.alert("warning", "instants_rate", format!("Výstup instantů {} běží na {} Hz – instanty hrají do hlavního výstupu", h.device_name, h.sample_rate)),
                            Err(e) => self.events.alert("warning", "instants_output_unavailable", format!("Výstup instantů nedostupný: {e}")),
                        }
                    }
                }
                if instants_out.as_ref().map(|o| o.failed.load(Ordering::Relaxed)).unwrap_or(false) { instants_out = None; }
            }

            // --- mix
            mix.iter_mut().for_each(|s| *s = 0.0);
            let duck_target = if voices.iter().any(|v| v.overlay && v.item.track.kind().is_overlay_capable()) { db_to_lin(self.cfg.read().audio.duck_db) } else { 1.0 };
            duck += (duck_target - duck) * 0.15;
            inst_mix.iter_mut().for_each(|s| *s = 0.0);
            for v in voices.iter_mut() {
                if state == PlayState::Paused && !v.overlay { continue; }
                let extra = if v.overlay { 1.0 } else { duck };
                if v.item.source == "instant" && instants_out.is_some() { v.render(&mut inst_mix, BLOCK, 1.0); }
                else { v.render(&mut mix, BLOCK, extra); }
            }
            if let Some(o) = instants_out.as_mut() {
                for x in inst_mix.iter_mut() { *x = soft_clip(*x); }
                if o.producer.slots() >= inst_mix.len() {
                    if let Ok(mut chunk) = o.producer.write_chunk_uninit(inst_mix.len()) {
                        let (a, b) = chunk.as_mut_slices();
                        let (sa, sb) = inst_mix.split_at(a.len());
                        for (d, s) in a.iter_mut().zip(sa) { d.write(*s); }
                        for (d, s) in b.iter_mut().zip(sb) { d.write(*s); }
                        unsafe { chunk.commit_all(); }
                    }
                }
            }
            let (mut pk_l, mut pk_r) = (0f32, 0f32);
            for fr in mix.chunks_exact_mut(2) {
                fr[0] = soft_clip(fr[0]);
                fr[1] = soft_clip(fr[1]);
                pk_l = pk_l.max(fr[0].abs());
                pk_r = pk_r.max(fr[1].abs());
            }
            lvl_l = pk_l.max(lvl_l * 0.93);
            lvl_r = pk_r.max(lvl_r * 0.93);
            self.meter_l.store(lvl_l.to_bits(), Ordering::Relaxed);
            self.meter_r.store(lvl_r.to_bits(), Ordering::Relaxed);

            // --- výstupy
            if let Some(o) = output.as_mut() {
                if let Ok(mut chunk) = o.producer.write_chunk_uninit(mix.len()) {
                    let (a, b) = chunk.as_mut_slices();
                    let (sa, sb) = mix.split_at(a.len());
                    for (d, s) in a.iter_mut().zip(sa) { d.write(*s); }
                    for (d, s) in b.iter_mut().zip(sb) { d.write(*s); }
                    unsafe { chunk.commit_all(); }
                }
            }
            frames_rendered += BLOCK as u64;
            {
                let block = Arc::new(mix.clone());
                self.taps.lock().retain(|t| !matches!(t.try_send(block.clone()), Err(crossbeam_channel::TrySendError::Disconnected(_))));
            }

            // --- dohrané hlasy
            let mut i = 0;
            while i < voices.len() {
                if voices[i].is_finished() {
                    let v = voices.remove(i);
                    if v.src.failed.load(Ordering::Acquire) {
                        self.events.alert("error", "playback_failed", format!("Soubor nelze přehrát: {}", v.item.track.path));
                    }
                    if v.underruns > 3 { log::warn!("{}: {} podtečení při čtení souboru (pomalý disk/síť?)", v.item.track.path, v.underruns); }
                    let played_ms = (v.pos * 1000 / rate as u64) as i64;
                    self.events.emit(Event::TrackEnded { item_id: v.item.id.clone(), played_ms });
                } else { i += 1; }
            }
            if stop_after && state == PlayState::Playing && !voices.iter().any(|v| !v.overlay) {
                state = PlayState::Stopped;
                stop_after = false;
            }

            // --- stav pro API
            {
                let mut st = self.status.write();
                st.state = state;
                st.stop_after_current = stop_after;
                st.voices = voices.iter().map(|v| v.status(rate)).collect();
                st.current = voices.iter().rev().find(|v| !v.overlay && !v.kill_after_fade).or_else(|| voices.iter().rev().find(|v| !v.overlay)).map(|v| v.status(rate));
                if let Some(o) = &output { st.output.underruns = o.underruns.load(Ordering::Relaxed); }
            }
        }
    }

    /// Udržuje přednačtenou první položku fronty.
    fn ensure_preload(&self, preloaded: &mut Option<(String, Prefetch)>) {
        let head = self.queue.lock().front().cloned();
        match (&head, preloaded.as_ref()) {
            (Some(h), Some((id, _))) if *id == h.id => {}
            (Some(h), _) if h.stitch.is_some() || h.is_attached_overlay() => *preloaded = None,
            (Some(h), _) => {
                let mut h = h.clone();
                self.refresh(&mut h);
                if let Some(front) = self.queue.lock().front_mut() { if front.id == h.id { front.track = h.track.clone(); } }
                let rate = self.sample_rate();
                let (cue_in, _, cue_out) = h.track.effective_cues();
                let max_frames = ((cue_out - cue_in).max(0) as u64 * rate as u64) / 1000;
                *preloaded = Some((h.id.clone(), Prefetch::start(PathBuf::from(&h.track.path), cue_in, max_frames, rate)));
            }
            (None, _) => *preloaded = None,
        }
    }

    fn start_next(&self, preloaded: &mut Option<(String, Prefetch)>) -> Option<(Voice, Option<Voice>)> {
        let mut item = self.queue.lock().pop_front()?;
        let mut pre = match preloaded.take() { Some((id, p)) if id == item.id => Some(p), _ => None };
        let before = item.track.effective_cues();
        self.refresh(&mut item);
        // analýza mohla mezitím doběhnout a změnit začátek → přednačtení zahodit
        if item.track.effective_cues().0 != before.0 { pre = None; }
        let (v, bed) = self.make_voice_full(item.clone(), false, pre);
        // u stitcheru logujeme skutečně složený název
        let mut logged = item;
        logged.track.title = v.item.track.title.clone();
        logged.track.duration_ms = v.item.track.duration_ms;
        self.events.emit(Event::TrackStarted { item: logged, channel: "main".into() });
        self.events.emit(Event::QueueChanged);
        Some((v, bed))
    }

    // --- operace s frontou (decky)

    pub fn queue_snapshot(&self) -> Vec<QueueItem> { self.queue.lock().iter().cloned().collect() }

    pub fn queue_push(&self, item: QueueItem, position: Option<usize>) {
        {
            let mut q = self.queue.lock();
            match position { Some(p) if p < q.len() => q.insert(p, item), _ => q.push_back(item) }
        }
        self.events.emit(Event::QueueChanged);
    }

    pub fn queue_remove(&self, id: &str) -> bool {
        let removed = { let mut q = self.queue.lock(); let before = q.len(); q.retain(|i| i.id != id); before != q.len() };
        if removed { self.events.emit(Event::QueueChanged); }
        removed
    }

    pub fn queue_move(&self, id: &str, to: usize) -> bool {
        let ok = {
            let mut q = self.queue.lock();
            if let Some(pos) = q.iter().position(|i| i.id == id) {
                let it = q.remove(pos).unwrap();
                let to = to.min(q.len());
                q.insert(to, it);
                true
            } else { false }
        };
        if ok { self.events.emit(Event::QueueChanged); }
        ok
    }

    pub fn queue_clear(&self) { self.queue.lock().clear(); self.events.emit(Event::QueueChanged); }
}
