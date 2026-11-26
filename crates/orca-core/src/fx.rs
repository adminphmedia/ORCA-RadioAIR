//! Externí efekty (VST / VST3 / AU / hardwarový procesor) – „návrat“ zpracovaného zvuku.
//!
//! ORCA zásuvné moduly nenačítá do sebe: chybný plugin by shodil celé vysílání. Efekty běží
//! v samostatném programu (plugin host – Element, Reaper, Carla, Cantabile, Gig Performer…) nebo
//! v hardwarovém procesoru. ORCA pošle program na výstup (do hostu / procesoru) a zpracovaný zvuk
//! si vezme zpět ze vstupu zvukové karty / virtuálního kabelu pro streamy a nahrávání vysílání.
//! Pojistka: když návrat ztichne nebo zmizí, streamy okamžitě jedou z programu ORCA (bez efektů)
//! a přijde upozornění; po obnovení se samy vrátí na zpracovaný zvuk.

use crate::config::Config;
use crate::engine::Engine;
use crate::events::EventBus;
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FxConfig {
    pub enabled: bool,
    /// host | hardware | fm_only – jen pro průvodce a nápovědu
    pub variant: String,
    /// vstup, ze kterého se vrací zpracovaný zvuk (virtuální kabel / vstup zvukové karty)
    pub return_device: String,
    /// streamy berou zpracovaný zvuk
    pub use_for_streams: bool,
    /// nahrávání vysílání (záznamník Program) bere zpracovaný zvuk
    pub use_for_logger: bool,
    /// při zpracovaném zvuku vynechat vestavěnou výstupní ochranu streamů (efekty už to udělaly)
    pub skip_internal: bool,
    /// po kolika sekundách ticha v návratu přepnout na program ORCA
    pub fallback_s: u32,
    /// práh ticha návratu (dBFS)
    pub silence_db: f32,
}

impl Default for FxConfig {
    fn default() -> Self { Self { enabled: false, variant: "host".into(), return_device: String::new(), use_for_streams: true, use_for_logger: false, skip_internal: true, fallback_s: 3, silence_db: -55.0 } }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FxStatus {
    pub enabled: bool,
    /// return = streamy jedou ze zpracovaného zvuku, program = z ORCA (vypnuto nebo pojistka)
    pub source: String,
    pub return_ok: bool,
    pub return_device: String,
    pub return_rate: u32,
    pub return_db: f32,
    pub program_db: f32,
    pub message: String,
    pub since: Option<String>,
    pub fallbacks: u64,
}

static RETURN_ACTIVE: AtomicBool = AtomicBool::new(false);
static FX: OnceLock<Arc<FxManager>> = OnceLock::new();

/// Streamy právě jedou ze zpracovaného návratu (a mají vynechat vestavěnou ochranu).
pub fn skip_internal_now() -> bool { RETURN_ACTIVE.load(Ordering::Relaxed) && FX.get().map(|f| f.cfg.read().fx.skip_internal).unwrap_or(false) }

/// Zdroj zvuku pro streamy / nahrávání: zpracovaný návrat s pojistkou, nebo přímo program.
pub fn tap_for(engine: &Engine, cfg: &Config, purpose: &str) -> Receiver<Arc<Vec<f32>>> {
    let use_fx = cfg.fx.enabled && match purpose { "streams" => cfg.fx.use_for_streams, "logger" => cfg.fx.use_for_logger, _ => false };
    if use_fx { if let Some(f) = FX.get() { return f.subscribe(); } }
    engine.add_tap()
}

pub fn status() -> FxStatus { FX.get().map(|f| f.status.lock().clone()).unwrap_or_default() }

pub struct FxManager {
    cfg: Arc<RwLock<Config>>,
    subs: Mutex<Vec<Sender<Arc<Vec<f32>>>>>,
    status: Mutex<FxStatus>,
}

fn peak_db(b: &[f32]) -> f32 { let p = b.iter().fold(0f32, |a, x| a.max(x.abs())); if p > 0.0 { 20.0 * p.log10() } else { -120.0 } }

impl FxManager {
    pub fn start(cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, events: EventBus) -> Arc<Self> {
        let me = Arc::new(Self { cfg, subs: Mutex::new(vec![]), status: Mutex::new(FxStatus { source: "program".into(), return_db: -120.0, program_db: -120.0, ..Default::default() }) });
        let _ = FX.set(me.clone());
        let m = me.clone();
        std::thread::Builder::new().name("orca-fx".into()).spawn(move || m.run(engine, events)).ok();
        me
    }

    pub fn subscribe(&self) -> Receiver<Arc<Vec<f32>>> { let (tx, rx) = bounded(800); self.subs.lock().push(tx); rx }

    fn push(&self, b: Arc<Vec<f32>>) { self.subs.lock().retain(|s| !matches!(s.try_send(b.clone()), Err(crossbeam_channel::TrySendError::Disconnected(_)))); }

    fn run(self: Arc<Self>, engine: Arc<Engine>, events: EventBus) {
        let mut prog_tap: Option<Receiver<Arc<Vec<f32>>>> = None;
        loop {
            let fc = self.cfg.read().fx.clone();
            // vypnuto: jen přeposílat program odběratelům vzniklým při zapnutí (odpojí se restartem streamu);
            // bez odběratelů nic neodebírat – ať mixér zbytečně nekopíruje zvuk
            if !fc.enabled {
                RETURN_ACTIVE.store(false, Ordering::Relaxed);
                { let mut s = self.status.lock(); if s.enabled || s.return_ok { *s = FxStatus { source: "program".into(), return_db: -120.0, program_db: -120.0, ..Default::default() }; } }
                if self.subs.lock().is_empty() { prog_tap = None; std::thread::sleep(Duration::from_millis(500)); continue; }
                let prog = prog_tap.get_or_insert_with(|| engine.add_tap());
                if let Ok(b) = prog.recv_timeout(Duration::from_millis(500)) { self.push(b); }
                continue;
            }
            let prog = prog_tap.get_or_insert_with(|| engine.add_tap()).clone();
            let opened = crate::airlog::open_input(&fc.return_device).and_then(|(rx, g)| Ok((rx, g, crate::airlog::input_rate(&fc.return_device)?)));
            let (ret, _guard, in_rate) = match opened {
                Ok(x) => x,
                Err(e) => {
                    let dev = if fc.return_device.is_empty() { "výchozí vstup".to_string() } else { format!("„{}“", fc.return_device) };
                    { let mut s = self.status.lock(); s.enabled = true; s.return_ok = false; s.source = "program".into(); s.return_device = fc.return_device.clone(); s.return_db = -120.0; s.message = format!("Návrat z efektů nelze otevřít – {dev} nenalezen nebo odpojený ({e})"); }
                    RETURN_ACTIVE.store(false, Ordering::Relaxed);
                    // mezitím jede program, za 5 s další pokus
                    let until = Instant::now() + Duration::from_secs(5);
                    let mut pdb = -120f32;
                    while Instant::now() < until { if let Ok(b) = prog.recv_timeout(Duration::from_millis(200)) { pdb = peak_db(&b).max(pdb - 1.0); self.status.lock().program_db = pdb; self.push(b); } }
                    continue;
                }
            };
            let out_rate = engine.sample_rate().max(8000);
            let mut rs = Resampler::new(in_rate, out_rate);
            let mut using_return = false;
            let mut last_ret = Instant::now();
            let mut quiet_since: Option<Instant> = None;
            let mut good_since: Option<Instant> = None;
            let (mut ret_db, mut prog_db) = (-120f32, -120f32);
            { let mut s = self.status.lock(); s.enabled = true; s.return_device = fc.return_device.clone(); s.return_rate = in_rate; s.message = "Čekám na zpracovaný zvuk…".into(); }
            loop {
                if self.cfg.read().fx != fc { break; } // změna nastavení → znovu otevřít
                // program (vždy – kvůli měření a pojistce)
                let mut prog_blocks = vec![];
                while let Ok(b) = prog.try_recv() { prog_db = peak_db(&b).max(prog_db - 1.0); prog_blocks.push(b); }
                // návrat
                let mut got = false;
                match ret.recv_timeout(Duration::from_millis(30)) {
                    Ok(b) => {
                        got = true; last_ret = Instant::now();
                        ret_db = peak_db(&b).max(ret_db - 1.0);
                        let conv = rs.process(&b);
                        if using_return && !conv.is_empty() { self.push(Arc::new(conv)); }
                        while let Ok(b2) = ret.try_recv() { ret_db = peak_db(&b2).max(ret_db - 1.0); let c2 = rs.process(&b2); if using_return && !c2.is_empty() { self.push(Arc::new(c2)); } }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
                if !using_return { for b in prog_blocks { self.push(b); } }
                // zdraví návratu: proudí data a není ticho, když program hraje
                let program_live = prog_db > fc.silence_db + 15.0;
                let missing = last_ret.elapsed() > Duration::from_millis(1500);
                let silent = ret_db < fc.silence_db && program_live;
                let bad = missing || silent;
                if bad { quiet_since.get_or_insert_with(Instant::now); good_since = None; } else { quiet_since = None; if got { good_since.get_or_insert_with(Instant::now); } }
                if using_return && quiet_since.map(|t| t.elapsed() >= Duration::from_secs(fc.fallback_s.max(1) as u64)).unwrap_or(false) {
                    using_return = false;
                    RETURN_ACTIVE.store(false, Ordering::Relaxed);
                    let why = if missing { "návrat z efektů neposílá zvuk" } else { "návrat z efektů je tichý, i když ORCA hraje" };
                    events.alert("warning", "fx_fallback", format!("Externí efekty: {why} – streamy jedou bez efektů přímo z ORCA."));
                    let mut s = self.status.lock(); s.fallbacks += 1; s.source = "program".into(); s.message = format!("Pojistka: {why}"); s.since = Some(chrono::Local::now().to_rfc3339());
                } else if !using_return && good_since.map(|t| t.elapsed() >= Duration::from_secs(3)).unwrap_or(false) && (program_live || ret_db > fc.silence_db) {
                    using_return = true;
                    RETURN_ACTIVE.store(true, Ordering::Relaxed);
                    let was_fallback = self.status.lock().fallbacks > 0;
                    if was_fallback { events.alert("info", "fx_restored", "Externí efekty: zpracovaný zvuk je zpět – streamy jedou s efekty."); }
                    let mut s = self.status.lock(); s.source = "return".into(); s.message.clear(); s.since = Some(chrono::Local::now().to_rfc3339());
                } else if !using_return && !program_live && ret_db < fc.silence_db {
                    // ORCA nehraje a návrat je tichý – nelze rozhodnout, zůstává program
                    self.status.lock().message = "ORCA nehraje – návrat nelze ověřit".into();
                }
                { let mut s = self.status.lock(); s.return_ok = !bad; s.return_db = ret_db; s.program_db = prog_db; }
                ret_db = (ret_db - 0.2).max(-120.0); prog_db = (prog_db - 0.2).max(-120.0);
            }
            RETURN_ACTIVE.store(false, Ordering::Relaxed);
        }
    }
}

/// Jednoduchý lineární převzorkovač prokládaného sterea (když má návrat jinou frekvenci než ORCA).
struct Resampler { ratio: f64, pos: f64, last: [f32; 2], same: bool }
impl Resampler {
    fn new(inr: u32, outr: u32) -> Self { Self { ratio: inr as f64 / outr as f64, pos: 0.0, last: [0.0; 2], same: inr == outr } }
    fn process(&mut self, b: &[f32]) -> Vec<f32> {
        if self.same { return b.to_vec(); }
        let n = b.len() / 2;
        let mut out = Vec::with_capacity(((n as f64) / self.ratio) as usize * 2 + 4);
        let get = |i: isize, c: usize, last: &[f32; 2]| if i < 0 { last[c] } else { b[i as usize * 2 + c] };
        while self.pos < n as f64 {
            let i = self.pos.floor() as isize; let f = (self.pos - i as f64) as f32;
            for c in 0..2 { let a = get(i - 1, c, &self.last); let z = get(i, c, &self.last); out.push(a + (z - a) * f); }
            self.pos += self.ratio;
        }
        self.pos -= n as f64;
        if n > 0 { self.last = [b[(n - 1) * 2], b[(n - 1) * 2 + 1]]; }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Resampler;
    #[test]
    fn resampler_keeps_duration() {
        let mut r = Resampler::new(44100, 48000);
        let block = vec![0.5f32; 4410 * 2];
        let mut n = 0; for _ in 0..10 { n += r.process(&block).len(); }
        // 1 s při 44,1 kHz → ~1 s při 48 kHz
        assert!((n as i64 / 2 - 48000).abs() < 10, "{n}");
    }
}
