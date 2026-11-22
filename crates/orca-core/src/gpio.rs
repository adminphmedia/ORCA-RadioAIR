//! Ovládání z mixážního pultu a externích ovladačů (Jazler: GPI / Studio module, „fader start“).
//! Zdroje: sériový port (kontakt na linkách CTS/DSR/DCD/RI – USB GPIO převodníky, výstupy pultů),
//! MIDI (fader / tlačítko pultu nebo kontroléru jako CC nebo nota), TCP textové příkazy a HTTP
//! (Bitfocus Companion, Stream Deck, skripty). Akce: pokračovat po živém vstupu, další, play, stop…

use crate::automation::{Automation, Mode};
use crate::config::{Config, GpiInput};
use crate::engine::{Cmd, Engine};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const ACTIONS: &[&str] = &["none", "continue", "next", "play", "stop", "pause", "mic_on", "mic_off", "live_insert", "stop_after", "auto", "manual", "stop_instants"];

#[derive(Debug, Clone, Serialize)]
pub struct GpiEvent { pub at: String, pub input: String, pub state: bool, pub action: String, pub result: String }

#[derive(Debug, Clone, Serialize, Default)]
pub struct GpiState { pub id: String, pub name: String, pub on: bool, pub connected: bool, pub message: String }

pub struct Gpio {
    engine: Arc<Engine>,
    automation: Arc<Automation>,
    cfg: Arc<RwLock<Config>>,
    states: RwLock<HashMap<String, GpiState>>,
    events: Mutex<VecDeque<GpiEvent>>,
    epoch: std::sync::atomic::AtomicU64,
}

impl Gpio {
    pub fn start(cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, automation: Arc<Automation>) -> Arc<Self> {
        let g = Arc::new(Self { engine, automation, cfg, states: Default::default(), events: Default::default(), epoch: Default::default() });
        g.restart();
        // TCP příkazy
        let g2 = g.clone();
        std::thread::Builder::new().name("orca-gpio-tcp".into()).spawn(move || g2.tcp_loop()).ok();
        g
    }

    /// Znovu otevře vstupy podle konfigurace (po uložení nastavení).
    pub fn restart(self: &Arc<Self>) {
        let epoch = self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let inputs = self.cfg.read().gpio.inputs.clone();
        self.states.write().clear();
        for inp in inputs.into_iter().filter(|i| i.enabled) {
            self.states.write().insert(inp.id.clone(), GpiState { id: inp.id.clone(), name: inp.name.clone(), ..Default::default() });
            let g = self.clone();
            match inp.source.as_str() {
                "serial" => { std::thread::Builder::new().name("orca-gpio-serial".into()).spawn(move || g.serial_loop(inp, epoch)).ok(); }
                "midi" => { std::thread::Builder::new().name("orca-gpio-midi".into()).spawn(move || g.midi_loop(inp, epoch)).ok(); }
                _ => { self.set_conn(&inp.id, true, "čeká na příkazy TCP / HTTP"); }
            }
        }
    }

    fn alive(&self, epoch: u64) -> bool { self.epoch.load(std::sync::atomic::Ordering::SeqCst) == epoch }
    fn set_conn(&self, id: &str, ok: bool, msg: &str) { if let Some(s) = self.states.write().get_mut(id) { s.connected = ok; s.message = msg.to_string(); } }

    pub fn status(&self) -> serde_json::Value {
        let mut st: Vec<GpiState> = self.states.read().values().cloned().collect();
        st.sort_by(|a, b| a.name.cmp(&b.name));
        serde_json::json!({ "inputs": st, "events": self.events.lock().iter().rev().cloned().collect::<Vec<_>>(), "live_active": self.engine.live_active(),
            "midi_devices": midi_devices(), "serial_ports": crate::rds::available_ports(), "actions": ACTIONS })
    }

    /// Změna stavu vstupu → akce podle konfigurace.
    pub fn input_changed(&self, id: &str, on: bool) {
        let inp = self.cfg.read().gpio.inputs.iter().find(|i| i.id == id || i.name.eq_ignore_ascii_case(id)).cloned();
        let Some(inp) = inp else { return };
        { let mut s = self.states.write(); if let Some(x) = s.get_mut(&inp.id) { if x.on == on && x.connected { return; } x.on = on; } }
        let action = if on { inp.on_action.clone() } else { inp.off_action.clone() };
        let result = self.run_action(&action);
        self.log(&inp.name, on, &action, &result);
    }

    fn log(&self, input: &str, state: bool, action: &str, result: &str) {
        log::info!("GPI {input} {} → {action}: {result}", if state { "ON" } else { "OFF" });
        let mut e = self.events.lock();
        e.push_back(GpiEvent { at: chrono::Local::now().to_rfc3339(), input: input.into(), state, action: action.into(), result: result.into() });
        if e.len() > 40 { e.pop_front(); }
    }

    /// Provede akci (i z HTTP / TCP). Vrací popis výsledku.
    pub fn run_action(&self, action: &str) -> String {
        let e = &self.engine;
        match action {
            "none" | "" => "bez akce".into(),
            "continue" => if e.live_continue() { "pokračuje se po živém vstupu".into() } else { "živý vstup neběží – nic".into() },
            "next" => { e.send(Cmd::Next); "další položka".into() }
            "play" => { e.send(Cmd::Play); "play".into() }
            "stop" => { e.send(Cmd::Stop); "stop".into() }
            "pause" => { e.send(Cmd::Pause); "pauza".into() }
            "mic_on" => { e.send(Cmd::TalkOver(true)); "mikrofon – ztlumení hudby".into() }
            "mic_off" => { e.send(Cmd::TalkOver(false)); "mikrofon vypnut".into() }
            "stop_after" => { e.send(Cmd::StopAfterCurrent(true)); "stop po skladbě".into() }
            "stop_instants" => { e.send(Cmd::StopInstants); "instanty zastaveny".into() }
            "live_insert" => {
                if e.live_active() { return "živý vstup už běží".into(); }
                e.queue_push(crate::model::QueueItem::live_break("ŽIVÝ VSTUP", 0, false, "manual", "živý vstup z pultu (GPI)"), Some(0));
                e.send(Cmd::Next);
                "živý vstup spuštěn".into()
            }
            "auto" => { self.automation.set_mode(Mode::Auto); "režim AUTO".into() }
            "manual" => { self.automation.set_mode(Mode::Manual); "režim RUČNĚ".into() }
            x => format!("neznámá akce {x}"),
        }
    }

    pub fn manual_trigger(&self, id: &str, on: bool) { self.input_changed(id, on); }

    // --- sériový port: modemové linky jako kontaktní vstupy
    fn serial_loop(self: Arc<Self>, inp: GpiInput, epoch: u64) {
        while self.alive(epoch) {
            match serialport::new(&inp.port, 9600).timeout(Duration::from_millis(50)).open() {
                Ok(mut port) => {
                    self.set_conn(&inp.id, true, &format!("{} · {}", inp.port, inp.line.to_uppercase()));
                    let mut last: Option<bool> = None;
                    let mut changed_at = Instant::now();
                    while self.alive(epoch) {
                        let v = match inp.line.as_str() { "dsr" => port.read_data_set_ready(), "dcd" => port.read_carrier_detect(), "ri" => port.read_ring_indicator(), _ => port.read_clear_to_send() };
                        match v {
                            Ok(raw) => {
                                let on = raw != inp.invert;
                                if last != Some(on) && changed_at.elapsed() >= Duration::from_millis(inp.debounce_ms as u64) {
                                    if last.is_some() { self.input_changed(&inp.id, on); } else if let Some(s) = self.states.write().get_mut(&inp.id) { s.on = on; }
                                    last = Some(on); changed_at = Instant::now();
                                }
                            }
                            Err(e) => { self.set_conn(&inp.id, false, &format!("chyba čtení: {e}")); break; }
                        }
                        std::thread::sleep(Duration::from_millis(15));
                    }
                }
                Err(e) => { self.set_conn(&inp.id, false, &format!("{}: {e}", inp.port)); std::thread::sleep(Duration::from_secs(3)); }
            }
        }
    }

    // --- MIDI: fader (CC nad prahem = otevřeno) nebo tlačítko (nota)
    fn midi_loop(self: Arc<Self>, inp: GpiInput, epoch: u64) {
        while self.alive(epoch) {
            let Ok(mut mi) = midir::MidiInput::new("ORCA RadioAIR") else { self.set_conn(&inp.id, false, "MIDI není dostupné"); std::thread::sleep(Duration::from_secs(5)); continue };
            mi.ignore(midir::Ignore::All);
            let ports = mi.ports();
            let port = ports.iter().find(|p| mi.port_name(p).map(|n| inp.midi_device.is_empty() || n.to_lowercase().contains(&inp.midi_device.to_lowercase())).unwrap_or(false)).cloned();
            let Some(port) = port else { self.set_conn(&inp.id, false, &format!("MIDI zařízení „{}“ nenalezeno", inp.midi_device)); std::thread::sleep(Duration::from_secs(3)); continue };
            let name = mi.port_name(&port).unwrap_or_default();
            let (tx, rx) = crossbeam_channel::unbounded::<(u8, u8, u8)>();
            let conn = mi.connect(&port, "orca-gpi", move |_, msg, _| { if msg.len() >= 3 { let _ = tx.send((msg[0], msg[1], msg[2])); } }, ());
            let Ok(_conn) = conn else { self.set_conn(&inp.id, false, "MIDI připojení selhalo"); std::thread::sleep(Duration::from_secs(3)); continue };
            self.set_conn(&inp.id, true, &format!("{name} · {} {}", inp.midi_type.to_uppercase(), inp.midi_number));
            let mut last: Option<bool> = None;
            while self.alive(epoch) {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok((st, d1, d2)) => {
                        let (kind, ch) = (st & 0xF0, (st & 0x0F) + 1);
                        if inp.midi_channel != 0 && ch != inp.midi_channel { continue; }
                        if d1 != inp.midi_number { continue; }
                        let on = match (inp.midi_type.as_str(), kind) {
                            ("cc", 0xB0) => d2 > inp.threshold,
                            ("note", 0x90) => d2 > 0,
                            ("note", 0x80) => false,
                            _ => continue,
                        } != inp.invert;
                        if last != Some(on) { last = Some(on); self.input_changed(&inp.id, on); }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // zařízení odpojeno?
                        if !midir::MidiInput::new("ORCA check").map(|m| m.ports().iter().any(|p| m.port_name(p).map(|n| n == name).unwrap_or(false))).unwrap_or(true) {
                            self.set_conn(&inp.id, false, "MIDI zařízení odpojeno"); break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    // --- TCP: „ACTION continue“, „GPI <název> ON|OFF“, „STATUS“
    fn tcp_loop(self: Arc<Self>) {
        let mut bound: Option<(u16, std::net::TcpListener)> = None;
        loop {
            let port = self.cfg.read().gpio.tcp_port;
            if bound.as_ref().map(|b| b.0) != Some(port) {
                bound = None;
                if port != 0 {
                    match std::net::TcpListener::bind(("0.0.0.0", port)) {
                        Ok(l) => { let _ = l.set_nonblocking(true); log::info!("GPI TCP na portu {port}"); bound = Some((port, l)); }
                        Err(e) => log::warn!("GPI TCP port {port}: {e}"),
                    }
                }
            }
            if let Some((_, l)) = &bound {
                if let Ok((s, _)) = l.accept() {
                    let g = self.clone();
                    std::thread::spawn(move || {
                        let _ = s.set_nonblocking(false);
                        let mut w = match s.try_clone() { Ok(w) => w, Err(_) => return };
                        for line in BufReader::new(s).lines().map_while(Result::ok) {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            let reply = match parts.as_slice() {
                                [a, act] if a.eq_ignore_ascii_case("ACTION") => g.run_action(&act.to_lowercase()),
                                [a, name, st] if a.eq_ignore_ascii_case("GPI") => { g.input_changed(name, st.eq_ignore_ascii_case("ON") || *st == "1"); "OK".into() }
                                [a] if a.eq_ignore_ascii_case("STATUS") => if g.engine.live_active() { "LIVE".into() } else { "PLAYING".into() },
                                _ => "ERR příkazy: ACTION <akce> | GPI <název> ON|OFF | STATUS".into(),
                            };
                            if writeln!(w, "{reply}").is_err() { break; }
                        }
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn midi_devices() -> Vec<String> {
    midir::MidiInput::new("ORCA list").map(|m| m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()).unwrap_or_default()
}
