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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// Opakovat právě hrající položku (Jazler Loop)
    #[serde(default)]
    pub loop_current: bool,
    #[serde(default)]
    pub talkover: bool,
    pub current: Option<VoiceStatus>,
    pub voices: Vec<VoiceStatus>,
    pub output: OutputStatus,
    /// Právě hrající hudební podkres
    pub bed: Option<VoiceStatus>,
}

/// Stav podkresu v mixeru (smyčka souboru až do značky OFF).
struct BedState { voice_id: String, track: Track }

pub enum Cmd {
    Play,
    Stop,
    Next,
    /// Přehrát hned: hrající položku (i se sweeperem přes ni) krátce stáhnout a novou spustit až po doznění –
    /// nic nehraje přes sebe. Podkres a instanty pokračují.
    PlayNow,
    Pause,
    StopAfterCurrent(bool),
    /// Opakovat právě hrající položku
    Loop(bool),
    /// Přehrát právě hrající položku znovu od začátku
    Restart,
    /// Mikrofon: ztlumit hudbu (talk-over) o duck_db
    TalkOver(bool),
    /// Okamžitě přehrát přes hlavní stopu (instant jingle).
    Instant(Track),
    /// Zastavit všechny instant jingly.
    StopInstants,
    ReopenOutput,
    /// Okamžitě ukončit hudební podkres (náběh doznění)
    BedOff,
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
    /// Vysílací processing: nové řetězce pro mixer (master + větev zvukové karty)
    proc_tx: Sender<ProcMsg>,
    proc_rx: Mutex<Option<Receiver<ProcMsg>>>,
    /// Generace processingu potvrzená mixerem (po dokončení prolnutí)
    pub proc_applied: AtomicU64,
    /// Bloky pro měření (0 = vstup processingu, 1 = výstup masteru)
    pub meter_tx: RwLock<Option<Sender<(u8, Arc<Vec<f32>>)>>>,
    /// Právě hrající hudební podkres (soubor z knihovny Podkresy)
    pub bed_active: RwLock<Option<Track>>,
    /// Historie úrovní po blocích 10 ms (špička a RMS L/R v dBFS) pro plynulé měřiče
    meter_hist: Mutex<MeterHist>,
    /// Vyrovnání hlasitosti: aktuální zisk a krátkodobá hlasitost vstupu (f64 bity)
    leveler_gain: AtomicU64,
    leveler_lufs: AtomicU64,
}

/// Jeden blok měření: [špička L, špička R, RMS L, RMS R] v dBFS (zaokrouhleno na 0,1 dB).
pub type MeterBlock = [f32; 4];
pub const METER_HIST_BLOCKS: usize = 12_000; // 2 minuty po 10 ms
#[derive(Default)]
struct MeterHist { seq: u64, blocks: VecDeque<MeterBlock>, block_ms: f32 }

/// Nastavení processingu pro mixer.
pub struct ProcMsg {
    pub master: Option<Box<crate::dsp::MasterChain>>,
    pub output_branch: Option<Box<crate::dsp::Branch>>,
    /// Servisní bypass celé DSP cesty (i výstupní ochrany)
    pub bypass: bool,
    pub generation: u64,
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
    /// Rebroadcast: přehrávač začal číst
    live: Arc<AtomicBool>,
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
        Prefetch { cons, done, stop, failed, live: Arc::new(AtomicBool::new(true)) }
    }

    /// Rebroadcast: živý zdroj (stream / vstup zvukové karty) se záložními zdroji.
    fn start_relay(cfg: Config, source_id: String, max_frames: u64, rate: u32) -> Prefetch {
        let (prod, cons) = rtrb::RingBuffer::<f32>::new(rate as usize * 2 * PREFETCH_SECONDS);
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let live = Arc::new(AtomicBool::new(false));
        let (d, s, f, l) = (done.clone(), stop.clone(), failed.clone(), live.clone());
        let _ = std::thread::Builder::new().name("orca-relay".into()).spawn(move || {
            let sink = crate::relay::Sink { prod, stop: s, written: 0, max_frames, live: l };
            if crate::relay::run(cfg, source_id, rate, sink) { f.store(true, Ordering::SeqCst); }
            d.store(true, Ordering::SeqCst);
        });
        Prefetch { cons, done, stop, failed, live }
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
    /// Tichý hlas (značka podkresu ON/OFF – čekání na intro / outro)
    silent: bool,
}

impl Voice {
    fn new(item: QueueItem, src: Prefetch, rate: u32, gain_db: f64, overlay: bool) -> Voice {
        let (cue_in, mix, cue_out) = item.track.effective_cues();
        let ms_to_f = |ms: i64| (ms.max(0) as u64 * rate as u64) / 1000;
        let len = ms_to_f(cue_out - cue_in);
        let mix_at = ms_to_f(mix - cue_in).min(len);
        Voice { item, src, pos: 0, len, mix_at, gain: db_to_lin(gain_db as f32), fade: None, segued: false, overlay, kill_after_fade: false, underruns: 0, silent: false }
    }

    /// Tichý hlas dané délky (značka podkresu).
    fn silent(item: QueueItem, len_ms: i64, rate: u32) -> Voice {
        let f = (len_ms.max(0) as u64 * rate as u64) / 1000;
        let mut v = Voice::new(item, Prefetch::start_segments(vec![], rate), rate, 0.0, false);
        v.len = f; v.mix_at = f; v.silent = true;
        v
    }

    fn status(&self, rate: u32) -> VoiceStatus {
        let f_to_ms = |f: u64| (f * 1000 / rate as u64) as i64;
        VoiceStatus {
            item: self.item.clone(),
            channel: if self.item.source == "bed" { "bed".into() } else if self.overlay { "overlay".into() } else { "main".into() },
            position_ms: f_to_ms(self.pos),
            length_ms: f_to_ms(self.len),
            remaining_ms: f_to_ms(self.len.saturating_sub(self.pos)),
            mix_at_ms: f_to_ms(self.mix_at),
            fading: self.kill_after_fade || self.fade.map(|f| f.to < f.from && self.pos >= f.start).unwrap_or(false),
        }
    }

    fn is_finished(&self) -> bool {
        if self.pos >= self.len { return true; }
        if self.silent { return false; }
        if self.kill_after_fade { if let Some(f) = self.fade { if f.finished(self.pos) { return true; } } }
        self.src.done.load(Ordering::Acquire) && self.src.cons.is_empty()
    }

    /// Přimíchá `frames` snímků do `mix`.
    fn render(&mut self, mix: &mut [f32], frames: usize, extra_gain: f32) {
        if !self.src.live.load(Ordering::Relaxed) { self.src.live.store(true, Ordering::Relaxed); }
        let want = frames.min((self.len - self.pos.min(self.len)) as usize);
        if self.silent { self.pos += want as u64; return; }
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
        let (proc_tx, proc_rx) = unbounded();
        let engine = Arc::new(Engine {
            queue: Mutex::new(VecDeque::new()),
            status: RwLock::new(EngineStatus {
                state: PlayState::Stopped,
                stop_after_current: false,
                loop_current: false,
                talkover: false,
                current: None,
                voices: vec![],
                bed: None,
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
            proc_tx, proc_rx: Mutex::new(Some(proc_rx)), proc_applied: AtomicU64::new(0), meter_tx: RwLock::new(None), bed_active: RwLock::new(None), meter_hist: Mutex::new(MeterHist::default()), leveler_gain: AtomicU64::new(0f64.to_bits()), leveler_lufs: AtomicU64::new((-70f64).to_bits()),
        });
        let e = engine.clone();
        std::thread::Builder::new().name("orca-mixer".into()).spawn(move || e.mixer_loop(cmd_rx)).expect("mixer thread");
        engine
    }

    pub fn send(&self, c: Cmd) { let _ = self.cmd_tx.send(c); }

    /// Předá mixeru nový processing (převezme se v příštím bloku s plynulým prolnutím).
    pub fn proc_apply(&self, m: ProcMsg) -> bool { self.proc_tx.send(m).is_ok() }

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
    /// Bloky měření od pořadového čísla `since` (vrací poslední seq, délku bloku v ms, bloky).
    pub fn meter_blocks(&self, since: u64, max: usize) -> (u64, f32, Vec<MeterBlock>) {
        let h = self.meter_hist.lock();
        let avail = h.blocks.len() as u64;
        let first_seq = h.seq.saturating_sub(avail);
        let from = since.max(first_seq).max(h.seq.saturating_sub(max as u64));
        let skip = (from - first_seq) as usize;
        (h.seq, h.block_ms, h.blocks.iter().skip(skip).copied().collect())
    }

    /// Vyrovnání hlasitosti: (zisk dB, hlasitost vstupu LUFS)
    pub fn leveler_state(&self) -> (f64, f64) { (f64::from_bits(self.leveler_gain.load(Ordering::Relaxed)), f64::from_bits(self.leveler_lufs.load(Ordering::Relaxed))) }

    pub fn meters(&self) -> (f32, f32) {
        (f32::from_bits(self.meter_l.load(Ordering::Relaxed)), f32::from_bits(self.meter_r.load(Ordering::Relaxed)))
    }

    /// Cesta po mapování složek (Obecná nastavení → mapování).
    fn mp(&self, p: &str) -> PathBuf { PathBuf::from(crate::general::map_path(&self.cfg.read(), p)) }

    pub fn snapshot(&self) -> EngineStatus { self.status.read().clone() }

    /// Stav přehrávání bez kopírování celého stavu (pro časté dotazy).
    pub fn play_state(&self) -> PlayState { self.status.read().state }

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
        if item.bed.is_none() && item.track.kind() == crate::model::TrackKind::Bed && item.track.id > 0 { item.bed = Some("on".into()); }
        if item.live {
            // tichý hlas: neomezeně (čeká na Pokračovat), nebo plánovaná délka při automatickém pokračování
            let len = if item.live_auto { item.track.duration_ms } else { 12 * 3600 * 1000 };
            return (Voice::silent(item, len, rate), None);
        }
        match item.bed.as_deref() {
            Some("off") => {
                let outro = self.bed_active.read().as_ref().map(|t| t.bed_outro_ms).unwrap_or(0);
                return (Voice::silent(item, outro, rate), None);
            }
            Some(_) => {
                let t = item.track.clone();
                let fade = (t.bed_fade_ms.clamp(0, 20_000) as u64 * rate as u64) / 1000;
                let bed = self.make_bed_voice(&t, fade);
                let mut marker = item;
                marker.track.title = format!("Podkres ON: {}", t.display());
                return (Voice::silent(marker, t.bed_intro_ms, rate), Some(bed));
            }
            None => {}
        }
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
                    let segs = plan.segments.iter().map(|sg| (self.mp(&sg.path), sg.start_ms, ms_f(sg.len_ms()))).collect();
                    let v = Voice::new(item.clone(), Prefetch::start_segments(segs, rate), rate, 0.0, overlay);
                    let bed = plan.bed.map(|(sg, gain, len_ms)| {
                        let len_ms = len_ms.min(sg.len_ms());
                        let mut bt = item.track.clone();
                        bt.title = "Stitcher – podkres".into(); bt.path = sg.path.clone(); bt.kind = Some(crate::model::TrackKind::Asset);
                        bt.cue_in_ms = sg.start_ms; bt.cue_out_ms = sg.start_ms + len_ms; bt.mix_ms = bt.cue_out_ms; bt.duration_ms = bt.cue_out_ms;
                        let bi = QueueItem::new(bt, "stitcher-bed", "podkres stitcheru");
                        let mut bv = Voice::new(bi, Prefetch::start(self.mp(&sg.path), sg.start_ms, ms_f(len_ms), rate), rate, gain as f64, true);
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
        if let Some(rid) = item.relay.clone() {
            let (_, _, co) = item.track.effective_cues();
            let max_frames = (co.max(0) as u64 * rate as u64) / 1000;
            let src = preloaded.unwrap_or_else(|| Prefetch::start_relay(self.cfg.read().clone(), rid, max_frames, rate));
            return (Voice::new(item, src, rate, 0.0, overlay), None);
        }
        let (normalize, target) = { let c = self.cfg.read(); (c.audio.normalize, c.audio.target_lufs) };
        let kind_trim = self.cfg.read().audio.kind_trim_db.get(&item.track.kind()).copied().unwrap_or(0.0) as f64;
        let gain_db = item.track.playback_gain_db(normalize, target) + kind_trim.clamp(-12.0, 12.0);
        let (cue_in, _, cue_out) = item.track.effective_cues();
        let max_frames = ((cue_out - cue_in).max(0) as u64 * rate as u64) / 1000;
        let src = preloaded.unwrap_or_else(|| Prefetch::start(self.mp(&item.track.path), cue_in, max_frames, rate));
        // krátké čekání na první data, aby začátek nebyl useknutý
        let t0 = Instant::now();
        while src.buffered_frames() < BLOCK * 4 && !src.done.load(Ordering::Acquire) && t0.elapsed() < Duration::from_millis(40) {
            std::thread::sleep(Duration::from_millis(2));
        }
        (Voice::new(item, src, rate, gain_db, overlay), None)
    }

    /// Hlas podkresu: hraje přes ostatní položky ve vlastní hlasitosti, náběh `fade_in` snímků.
    fn make_bed_voice(&self, t: &Track, fade_in: u64) -> Voice {
        let rate = self.sample_rate();
        let (normalize, target) = { let c = self.cfg.read(); (c.audio.normalize, c.audio.target_lufs) };
        let gain_db = t.playback_gain_db(normalize, target) + t.bed_level_db.clamp(-60.0, 12.0);
        let (cue_in, _, cue_out) = t.effective_cues();
        let max_frames = ((cue_out - cue_in).max(0) as u64 * rate as u64) / 1000;
        let item = QueueItem::new(t.clone(), "bed", "hudební podkres");
        let mut v = Voice::new(item, Prefetch::start(self.mp(&t.path), cue_in, max_frames, rate), rate, gain_db, true);
        if fade_in > 0 { v.fade = Some(Fade { from: 0.0, to: 1.0, start: 0, len: fade_in.min(v.len.max(1)) }); }
        v
    }

    /// Zařadí právě spuštěnou položku do mixu (a obslouží značky podkresu ON/OFF).
    fn push_started(&self, voices: &mut Vec<Voice>, v: Voice, extra: Option<Voice>, bed: &mut Option<BedState>) {
        let rate = self.sample_rate() as u64;
        let fr = |ms: i64| (ms.max(0) as u64 * rate) / 1000;
        match v.item.bed.as_deref() {
            Some("on") => {
                for b in voices.iter_mut().filter(|x| x.item.source == "bed" && !x.kill_after_fade) {
                    let len = fr(b.item.track.bed_fade_ms).max(fr(200));
                    b.fade = Some(Fade { from: b.fade.map(|f| f.gain_at(b.pos)).unwrap_or(1.0), to: 0.0, start: b.pos, len });
                    b.kill_after_fade = true;
                }
                if let Some(bv) = extra {
                    *bed = Some(BedState { voice_id: bv.item.id.clone(), track: bv.item.track.clone() });
                    *self.bed_active.write() = Some(bv.item.track.clone());
                    self.events.emit(Event::TrackStarted { item: bv.item.clone(), channel: "bed".into() });
                    voices.push(bv);
                }
                voices.push(v);
            }
            Some("off") => {
                for b in voices.iter_mut().filter(|x| x.item.source == "bed" && !x.kill_after_fade) {
                    let t = &b.item.track;
                    let fade = fr(t.bed_fade_ms).max(fr(200));
                    let wait = fr(t.bed_outro_ms).saturating_sub(fade);
                    b.fade = Some(Fade { from: b.fade.map(|f| f.gain_at(b.pos)).unwrap_or(1.0), to: 0.0, start: b.pos + wait, len: fade });
                    b.kill_after_fade = true;
                }
                *bed = None;
                *self.bed_active.write() = None;
                voices.push(v);
            }
            _ => { voices.push(v); if let Some(b) = extra { voices.push(b); } }
        }
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
        crate::general::boost_current_thread();
        let mut output = self.try_open_output();
        let mut last_reopen = Instant::now();
        let mut voices: Vec<Voice> = Vec::new();
        let mut preloaded: Option<(String, Prefetch)> = None;
        let mut state = if self.cfg.read().auto.start_in_auto && self.cfg.read().link.role != "backup" { PlayState::Playing } else { PlayState::Stopped };
        let mut stop_after = false;
        let mut loop_current = false;
        let mut talkover = false;
        let mut mix = vec![0f32; BLOCK * 2];
        let mut clock_start = Instant::now();
        let mut frames_rendered: u64 = 0;
        let (mut lvl_l, mut lvl_r) = (0f32, 0f32);
        let mut duck = 1.0f32;
        let mut pending: Vec<PendingOverlay> = Vec::new();
        let mut bed: Option<BedState> = None;
        let mut leveler = crate::dsp::leveler::Leveler::new(self.sample_rate());
        let mut instants_out: Option<OutputHandle> = None;
        let mut instants_dev = String::new();
        let mut inst_mix = vec![0f32; BLOCK * 2];
        let proc_rx = self.proc_rx.lock().take().expect("proc rx");
        let mut master: Option<Box<crate::dsp::MasterChain>> = None;
        let mut fade_old: Option<Box<crate::dsp::MasterChain>> = None;
        let mut out_branch: Option<Box<crate::dsp::Branch>> = None;
        let mut proc_bypass = false;
        let mut xfade_pos: usize = 0;
        let mut pending_ack: Option<u64> = None;
        // po „Přehrát hned“ se další položka spustí až po doznění předchozí
        let mut hold_until: Option<Instant> = None;
        let mut old_buf = vec![0f32; BLOCK * 2];
        let mut out_buf: Vec<f32> = Vec::with_capacity(BLOCK * 4);
        const XFADE: usize = 2400; // 50 ms při 48 kHz

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
                        bed = None;
                        *self.bed_active.write() = None;
                        for v in voices.iter_mut().filter(|v| !v.overlay || v.item.source == "bed") {
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
                        if let Some((v, x)) = self.start_next(&mut preloaded) { self.push_started(&mut voices, v, x, &mut bed); }
                    }
                    Cmd::PlayNow => {
                        state = PlayState::Playing;
                        let len_ms = manual_fade.clamp(150, 600);
                        for v in voices.iter_mut().filter(|v| !v.overlay || (v.item.source != "instant" && v.item.source != "bed")) {
                            v.fade = Some(Fade { from: v.fade.map(|f| f.gain_at(v.pos)).unwrap_or(1.0), to: 0.0, start: v.pos, len: len_ms * rate / 1000 });
                            v.kill_after_fade = true;
                            v.segued = true;
                        }
                        stop_after = false;
                        pending.clear();
                        hold_until = Some(Instant::now() + Duration::from_millis(len_ms));
                    }
                    Cmd::StopAfterCurrent(b) => stop_after = b,
                    Cmd::Loop(b) => loop_current = b,
                    Cmd::TalkOver(b) => talkover = b,
                    Cmd::Restart => {
                        if let Some(cur) = voices.iter().rev().find(|v| !v.overlay && !v.kill_after_fade) {
                            let mut it = cur.item.clone();
                            it.id = uuid::Uuid::new_v4().to_string();
                            self.queue_push(it, Some(0));
                            let _ = self.cmd_tx.send(Cmd::Next);
                        }
                    }
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
                    Cmd::BedOff => {
                        for b in voices.iter_mut().filter(|x| x.item.source == "bed" && !x.kill_after_fade) {
                            let len = (b.item.track.bed_fade_ms.max(200) as u64 * rate) / 1000;
                            b.fade = Some(Fade { from: b.fade.map(|f| f.gain_at(b.pos)).unwrap_or(1.0), to: 0.0, start: b.pos, len });
                            b.kill_after_fade = true;
                        }
                        bed = None;
                        *self.bed_active.write() = None;
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
            if state == PlayState::Playing && loop_current {
                // smyčka: před bodem mixu vložit stejnou položku znovu na začátek fronty
                if let Some(cur) = voices.iter().rev().find(|v| !v.overlay && !v.segued) {
                    if cur.pos + BLOCK as u64 * 2 >= cur.mix_at && self.queue.lock().front().map(|f| f.track.id != cur.item.track.id || f.relay.is_some()).unwrap_or(true) && cur.item.relay.is_none() && cur.item.stitch.is_none() {
                        let mut it = cur.item.clone();
                        it.id = uuid::Uuid::new_v4().to_string();
                        it.reason = "smyčka (loop)".into();
                        self.queue_push(it, Some(0));
                    }
                }
            }
            if hold_until.map(|t| Instant::now() >= t).unwrap_or(false) { hold_until = None; }
            if state == PlayState::Playing && hold_until.is_none() {
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
                    if let Some((v, x)) = self.start_next(&mut preloaded) { self.push_started(&mut voices, v, x, &mut bed); }
                }
            }

            // --- sweepery / voice tracky na pozici přes skladbu
            loop {
                let front = { let q = self.queue.lock(); q.front().filter(|f| f.is_attached_overlay()).cloned() };
                let Some(mut ov) = front else { break };
                let Some(host) = voices.iter().rev().find(|v| !v.overlay && !v.kill_after_fade && !v.segued) else { break };
                // dlouhá položka (reportáž, dlouhý voice track) by hrála přes skladbu – nikdy nepřekrývat,
                // zahraje se normálně po skladbě
                if let Err(why) = overlay_fits(&host.item.track, &ov) {
                    let mut q = self.queue.lock();
                    if let Some(f) = q.front_mut() { if f.id == ov.id { f.position = None; f.reason = format!("{} · hraje samostatně ({why})", f.reason).trim_start_matches(" · ").to_string(); } }
                    drop(q);
                    log::info!("„{}“ nepřekrývá skladbu: {why}", ov.track.display());
                    self.events.emit(Event::QueueChanged);
                    break;
                }
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

            // --- podkres: smyčka souboru s prolnutím, dokud nepřijde značka OFF
            if state == PlayState::Playing {
                if let Some(b) = bed.as_mut() {
                    match voices.iter().position(|v| v.item.id == b.voice_id) {
                        Some(i) => {
                            let xf = ((b.track.bed_fade_ms.clamp(300, 5000) as u64 * rate as u64) / 1000).min(voices[i].len / 3).max(BLOCK as u64);
                            if voices[i].len.saturating_sub(voices[i].pos) <= xf {
                                let cur = &mut voices[i];
                                cur.fade = Some(Fade { from: cur.fade.map(|f| f.gain_at(cur.pos)).unwrap_or(1.0), to: 0.0, start: cur.pos, len: xf });
                                cur.kill_after_fade = true;
                                let mut nv = self.make_bed_voice(&b.track, xf);
                                nv.item.reason = "hudební podkres (opakování)".into();
                                b.voice_id = nv.item.id.clone();
                                voices.push(nv);
                            }
                        }
                        None => { bed = None; *self.bed_active.write() = None; }
                    }
                }
            }

            // --- mix
            mix.iter_mut().for_each(|s| *s = 0.0);
            let duck_target = if talkover || voices.iter().any(|v| v.overlay && v.item.track.kind().is_overlay_capable()) { db_to_lin(self.cfg.read().audio.duck_db) } else { 1.0 };
            duck += (duck_target - duck) * 0.15;
            inst_mix.iter_mut().for_each(|s| *s = 0.0);
            for v in voices.iter_mut() {
                if state == PlayState::Paused && (!v.overlay || v.item.source == "bed") { continue; }
                let extra = if v.overlay { 1.0 } else { duck };
                if v.item.source == "instant" && instants_out.is_some() { v.render(&mut inst_mix, BLOCK, 1.0); }
                else { v.render(&mut mix, BLOCK, extra); }
            }
            // mikrofon přes počítač (režim „mix“) – přimíchá se při zapnutém mikrofonu
            crate::mic::mix_into(&mut mix, talkover);
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
            // --- vysílací processing (master) – převzetí nového nastavení s prolnutím
            while let Ok(msg) = proc_rx.try_recv() {
                let mut nm = msg.master;
                if let (Some(n), Some(o)) = (nm.as_mut(), master.as_ref()) { n.warm_from(o); }
                fade_old = if nm.is_some() || master.is_some() { master.take() } else { None };
                xfade_pos = 0;
                master = nm;
                out_branch = msg.output_branch;
                proc_bypass = msg.bypass;
                pending_ack = Some(msg.generation);
            }
            // --- vyrovnání hlasitosti programu (LUFS) – všechny zdroje na cílovou hlasitost
            {
                let (lc, target) = { let c = self.cfg.read(); (c.audio.leveler.clone(), c.audio.target_lufs) };
                if lc.enabled {
                    let p = crate::dsp::leveler::LevelerParams::for_speed(target, lc.max_boost_db, lc.max_cut_db, &lc.speed);
                    leveler.process(&mut mix, &p, talkover || state != PlayState::Playing);
                } else if leveler.gain_db() != 0.0 { leveler.reset_gain(); }
                self.leveler_gain.store(leveler.gain_db().to_bits(), Ordering::Relaxed);
                self.leveler_lufs.store(leveler.loudness().to_bits(), Ordering::Relaxed);
            }
            let meter_tx = self.meter_tx.read().clone();
            if let Some(tx) = &meter_tx { let _ = tx.try_send((0, Arc::new(mix.clone()))); }
            let mut processed = false;
            if !proc_bypass && (master.is_some() || fade_old.is_some()) {
                let t0 = Instant::now();
                if let Some(old) = fade_old.as_mut() { old_buf.copy_from_slice(&mix); old.process(&mut old_buf); }
                if let Some(m) = master.as_mut() { m.process(&mut mix); }
                if fade_old.is_some() {
                    let frames = mix.len() / 2;
                    for i in 0..frames {
                        let w = ((xfade_pos + i) as f32 / XFADE as f32).min(1.0);
                        mix[i * 2] = old_buf[i * 2] * (1.0 - w) + mix[i * 2] * w;
                        mix[i * 2 + 1] = old_buf[i * 2 + 1] * (1.0 - w) + mix[i * 2 + 1] * w;
                    }
                    xfade_pos += frames;
                    if xfade_pos >= XFADE { fade_old = None; }
                }
                if mix.iter().any(|x| !x.is_finite()) {
                    // selhání DSP: zahodit řetězec, vrátit se k bezpečné cestě (soft clip), udržet časování
                    master = None; fade_old = None;
                    mix.iter_mut().for_each(|x| if !x.is_finite() { *x = 0.0 });
                    crate::dsp::stats().errors.fetch_add(1, Ordering::Relaxed);
                    self.events.alert("error", "dsp_failed", "Processing vrátil neplatná data – vysílání pokračuje bez processingu (bezpečná cesta)".to_string());
                }
                let us = t0.elapsed().as_micros() as u32;
                let st = crate::dsp::stats();
                st.cpu_us_last.store(us, Ordering::Relaxed);
                if us > st.cpu_us_max.load(Ordering::Relaxed) { st.cpu_us_max.store(us, Ordering::Relaxed); }
                st.blocks.fetch_add(1, Ordering::Relaxed);
                processed = master.is_some();
                if let Some(tx) = &meter_tx { let _ = tx.try_send((1, Arc::new(mix.clone()))); }
            }
            if fade_old.is_none() { if let Some(g) = pending_ack.take() { self.proc_applied.store(g, Ordering::Release); } }
            // výstup zvukové karty: výstupní větev (trim + true-peak limiter), jinak soft clip
            out_buf.clear();
            match out_branch.as_mut() {
                Some(b) if !proc_bypass => { b.process(&mix, &mut out_buf); }
                _ => out_buf.extend(mix.iter().map(|x| soft_clip(*x))),
            }
            // bez processingu zůstává původní chování (soft clip i do streamů)
            if !processed { for x in mix.iter_mut() { *x = soft_clip(*x); } }
            let (mut pk_l, mut pk_r) = (0f32, 0f32);
            let (mut sq_l, mut sq_r) = (0f64, 0f64);
            for fr in out_buf.chunks_exact(2) {
                pk_l = pk_l.max(fr[0].abs());
                pk_r = pk_r.max(fr[1].abs());
                sq_l += (fr[0] as f64) * (fr[0] as f64);
                sq_r += (fr[1] as f64) * (fr[1] as f64);
            }
            {
                let n = (out_buf.len() / 2).max(1) as f64;
                let db = |x: f64| { let d = 20.0 * x.max(1e-7).log10(); ((d.max(-120.0) * 10.0).round() / 10.0) as f32 };
                let blk: MeterBlock = [db(pk_l as f64), db(pk_r as f64), db((sq_l / n).sqrt()), db((sq_r / n).sqrt())];
                let mut h = self.meter_hist.lock();
                h.block_ms = BLOCK as f32 * 1000.0 / rate as f32;
                h.blocks.push_back(blk);
                if h.blocks.len() > METER_HIST_BLOCKS { h.blocks.pop_front(); }
                h.seq += 1;
            }
            if pk_l > 1.0 || pk_r > 1.0 { crate::dsp::stats().clip_samples.fetch_add(1, Ordering::Relaxed); }
            lvl_l = pk_l.max(lvl_l * 0.93);
            lvl_r = pk_r.max(lvl_r * 0.93);
            self.meter_l.store(lvl_l.to_bits(), Ordering::Relaxed);
            self.meter_r.store(lvl_r.to_bits(), Ordering::Relaxed);

            // --- výstupy
            if let Some(o) = output.as_mut() {
                if let Ok(mut chunk) = o.producer.write_chunk_uninit(out_buf.len()) {
                    let (a, b) = chunk.as_mut_slices();
                    let (sa, sb) = out_buf.split_at(a.len());
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
                st.loop_current = loop_current;
                st.talkover = talkover;
                st.voices = voices.iter().map(|v| v.status(rate)).collect();
                st.bed = voices.iter().rev().find(|v| v.item.source == "bed" && !v.kill_after_fade).or_else(|| voices.iter().rev().find(|v| v.item.source == "bed")).map(|v| v.status(rate));
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
            (Some(h), _) if h.note.is_some() || h.stitch.is_some() || h.is_attached_overlay() || h.bed.is_some() || h.live || h.track.kind() == crate::model::TrackKind::Bed => *preloaded = None,
            (Some(h), _) if h.relay.is_some() => {
                let rate = self.sample_rate();
                let max_frames = (h.track.effective_cues().2.max(0) as u64 * rate as u64) / 1000;
                *preloaded = Some((h.id.clone(), Prefetch::start_relay(self.cfg.read().clone(), h.relay.clone().unwrap_or_default(), max_frames, rate)));
            }
            (Some(h), _) => {
                let mut h = h.clone();
                self.refresh(&mut h);
                if let Some(front) = self.queue.lock().front_mut() { if front.id == h.id { front.track = h.track.clone(); } }
                let rate = self.sample_rate();
                let (cue_in, _, cue_out) = h.track.effective_cues();
                let max_frames = ((cue_out - cue_in).max(0) as u64 * rate as u64) / 1000;
                *preloaded = Some((h.id.clone(), Prefetch::start(self.mp(&h.track.path), cue_in, max_frames, rate)));
            }
            (None, _) => *preloaded = None,
        }
    }

    fn start_next(&self, preloaded: &mut Option<(String, Prefetch)>) -> Option<(Voice, Option<Voice>)> {
        // poznámky pro moderátora se nepřehrávají – při dojetí se tiše odeberou
        let mut item = loop {
            let it = self.queue.lock().pop_front()?;
            if it.note.is_some() { self.events.emit(Event::QueueChanged); continue; }
            break it;
        };
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
        if logged.bed.is_none() && !logged.live && logged.track.kind() != crate::model::TrackKind::Bed {
            self.events.emit(Event::TrackStarted { item: logged, channel: "main".into() });
        }
        self.events.emit(Event::QueueChanged);
        Some((v, bed))
    }

    // --- operace s frontou (decky)

    /// Ukončit podkres hned (doznění podle nastavení podkresu), přehrávání pokračuje.
    pub fn bed_stop_now(&self) { let _ = self.cmd_tx.send(Cmd::BedOff); }

    /// Právě běží živý vstup (čeká se na pokračování)?
    /// Moderátor mluví (talkover / MIC v režimu přes pult).
    pub fn talkover(&self) -> bool { self.status.read().talkover }

    pub fn live_active(&self) -> bool { self.status.read().current.as_ref().map(|c| c.item.live && !c.fading).unwrap_or(false) }

    /// Pokračovat po živém vstupu (jen když živý vstup právě běží). Vrací, zda se pokračovalo.
    pub fn live_continue(&self) -> bool { if self.live_active() { self.send(Cmd::Next); true } else { false } }

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

    /// Vícestopý editor: značky jen pro toto zařazení ve frontě (skladba v knihovně se nemění).
    pub fn queue_set_cues(&self, id: &str, cue_in: Option<i64>, intro: Option<i64>, mix: Option<i64>, cue_out: Option<i64>, position: Option<Option<String>>) -> bool {
        let mut hit = false;
        for it in self.queue.lock().iter_mut().filter(|it| it.id == id) {
            if let Some(v) = cue_in { it.track.cue_in_ms = v.max(0); }
            if let Some(v) = intro { it.track.intro_ms = v.max(0); }
            if let Some(v) = mix { it.track.mix_ms = v.max(0); }
            if let Some(v) = cue_out { it.track.cue_out_ms = v.max(0); }
            if let Some(p) = position.clone() { it.position = p; }
            it.track.manual_cues = true;
            hit = true;
        }
        if hit { self.events.emit(Event::QueueChanged); }
        hit
    }

    /// Soubor se v knihovně přesunul / změnil druh či kategorii → aktualizovat čekající položky fronty.
    pub fn queue_refresh_track(&self, t: &Track) {
        let mut hit = false;
        for it in self.queue.lock().iter_mut() {
            if it.track.id == t.id && t.id > 0 {
                it.track.path = t.path.clone(); it.track.filename = t.filename.clone(); it.track.kind = t.kind; it.track.category = t.category.clone();
                hit = true;
            }
        }
        if hit { self.events.emit(Event::QueueChanged); }
    }

    pub fn queue_clear(&self) { self.queue.lock().clear(); self.events.emit(Event::QueueChanged); }
}


/// Smí položka (sweeper / voice track) hrát přes hostitelskou skladbu? Překrytí je jen tam, kde se nezpívá
/// (intro, outro) a jen pro krátké položky – jinak by hrály dva zvuky přes sebe.
pub fn overlay_fits(host: &crate::model::Track, ov: &QueueItem) -> Result<(), String> {
    const MAX_OVERLAY_MS: i64 = 60_000;
    let (ci, _, co) = ov.track.effective_cues();
    let len = (co - ci).max(0);
    if len > MAX_OVERLAY_MS { return Err(format!("délka {} s – přes skladbu se kladou jen položky do 60 s", len / 1000)); }
    let (hci, hmix, _) = host.effective_cues();
    let intro = (host.intro_ms - hci).max(0);
    let outro_win = if host.outro_ms > hci && host.outro_ms < hmix { hmix - host.outro_ms } else { 10_000 };
    let grace = 3_000;
    let pos = ov.position.as_deref().unwrap_or("start");
    let ok = match pos {
        p if p.starts_with("custom:") => true, // ručně nastavené v editoru přechodů
        "start" | "before_intro" => len <= intro + grace || (ov.track.kind() == crate::model::TrackKind::Sweeper && len <= 8_000),
        "before_end" => len <= outro_win + grace,
        "bridge" => len / 2 <= outro_win + grace,
        _ => true,
    };
    if ok { Ok(()) } else {
        Err(match pos {
            "start" | "before_intro" => format!("{} s je delší než intro skladby ({} s)", len / 1000, intro / 1000),
            _ => format!("{} s je delší než konec skladby bez zpěvu ({} s)", len / 1000, outro_win / 1000),
        })
    }
}
