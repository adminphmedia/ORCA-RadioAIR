//! Streamovací encodery: výstup enginu → ffmpeg → Icecast (MP3, AAC, Ogg Vorbis, Opus),
//! automatické znovupřipojení a aktualizace metadat skladby.

use crate::config::{Config, EncoderConfig, StreamFormat};
use crate::engine::Engine;
use crate::events::{Event, EventBus};
use base64::Engine as _;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
pub struct EncoderStatus {
    pub id: String,
    pub name: String,
    pub state: String, // stopped | connecting | live | error
    pub message: String,
    pub since: Option<String>,
    pub bytes_in: u64,
    pub url: String,
}

struct Runtime { stop: Arc<AtomicBool>, status: Arc<Mutex<EncoderStatus>> }

pub struct EncoderManager {
    engine: Arc<Engine>,
    cfg: Arc<RwLock<Config>>,
    events: EventBus,
    running: Mutex<HashMap<String, Runtime>>,
    last_meta: Mutex<String>,
}

pub fn resolve_ffmpeg(cfg: &Config) -> PathBuf {
    if !cfg.ffmpeg_path.is_empty() { return PathBuf::from(&cfg.ffmpeg_path); }
    if let Some(p) = std::env::var_os("ORCA_FFMPEG") { return PathBuf::from(p); }
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            for cand in [dir.join(exe), dir.join("../Resources").join(exe), dir.join("resources").join(exe)] {
                if cand.is_file() { return cand; }
            }
        }
    }
    // aplikace spuštěné z Finderu nemají PATH z Homebrew
    for cand in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
        if std::path::Path::new(cand).is_file() { return PathBuf::from(cand); }
    }
    PathBuf::from(exe)
}

pub fn public_url(e: &EncoderConfig) -> String {
    let mount = if e.mount.starts_with('/') { e.mount.clone() } else { format!("/{}", e.mount) };
    format!("http://{}:{}{}", e.host, e.port, mount)
}

fn pct(s: &str) -> String { urlencoding::encode(s).into_owned() }

pub fn ffmpeg_args(e: &EncoderConfig, in_rate: u32) -> Vec<String> {
    let mount = if e.mount.starts_with('/') { e.mount.clone() } else { format!("/{}", e.mount) };
    let (codec, fmt, ctype) = match e.format {
        StreamFormat::Mp3 => ("libmp3lame", "mp3", "audio/mpeg"),
        StreamFormat::Aac => ("aac", "adts", "audio/aac"),
        StreamFormat::Ogg => ("libvorbis", "ogg", "audio/ogg"),
        StreamFormat::Opus => ("libopus", "ogg", "audio/ogg"),
    };
    let out_rate = if e.format == StreamFormat::Opus { 48000 } else { e.sample_rate };
    let mut a: Vec<String> = vec![
        "-hide_banner", "-nostdin", "-loglevel", "warning",
        "-f", "f32le", "-ar", &in_rate.to_string(), "-ac", "2", "-i", "pipe:0",
        "-ar", &out_rate.to_string(), "-ac", &e.channels.clamp(1, 2).to_string(),
        "-c:a", codec, "-b:a", &format!("{}k", e.bitrate_kbps),
        "-content_type", ctype,
        "-ice_name", &e.stream_name, "-ice_genre", &e.genre, "-ice_description", &e.description, "-ice_url", &e.url,
        "-ice_public", if e.public { "1" } else { "0" },
        "-f", fmt,
    ].into_iter().map(String::from).collect();
    a.push(format!("icecast://{}:{}@{}:{}{}", pct(&e.username), pct(&e.password), e.host, e.port, mount));
    a
}

impl EncoderManager {
    pub fn new(engine: Arc<Engine>, cfg: Arc<RwLock<Config>>, events: EventBus) -> Arc<Self> {
        Arc::new(Self { engine, cfg, events, running: Mutex::new(HashMap::new()), last_meta: Mutex::new(String::new()) })
    }

    pub fn autostart(self: &Arc<Self>) {
        let ids: Vec<String> = self.cfg.read().encoders.iter().filter(|e| e.enabled && e.autostart).map(|e| e.id.clone()).collect();
        for id in ids { self.start(&id); }
    }

    pub fn statuses(&self) -> Vec<EncoderStatus> {
        let cfg = self.cfg.read();
        let run = self.running.lock();
        cfg.encoders.iter().map(|e| match run.get(&e.id) {
            Some(r) => r.status.lock().clone(),
            None => EncoderStatus { id: e.id.clone(), name: e.name.clone(), state: "stopped".into(), message: String::new(), since: None, bytes_in: 0, url: public_url(e) },
        }).collect()
    }

    pub fn stop(&self, id: &str) {
        if let Some(r) = self.running.lock().remove(id) { r.stop.store(true, Ordering::SeqCst); }
        self.events.emit(Event::EncoderStatus { id: id.into(), state: "stopped".into(), message: String::new() });
    }

    pub fn stop_all(&self) {
        let ids: Vec<String> = self.running.lock().keys().cloned().collect();
        for id in ids { self.stop(&id); }
    }

    pub fn start(self: &Arc<Self>, id: &str) -> bool {
        let Some(ec) = self.cfg.read().encoders.iter().find(|e| e.id == id).cloned() else { return false };
        self.stop(id);
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(EncoderStatus { id: ec.id.clone(), name: ec.name.clone(), state: "connecting".into(), message: String::new(), since: None, bytes_in: 0, url: public_url(&ec) }));
        self.running.lock().insert(ec.id.clone(), Runtime { stop: stop.clone(), status: status.clone() });
        let me = self.clone();
        std::thread::Builder::new().name(format!("orca-enc-{}", ec.name)).spawn(move || me.run(ec, stop, status)).ok();
        true
    }

    fn set_state(&self, st: &Arc<Mutex<EncoderStatus>>, state: &str, msg: &str) {
        let mut s = st.lock();
        if s.state != state || s.message != msg {
            s.state = state.into();
            s.message = msg.into();
            s.since = Some(chrono::Local::now().to_rfc3339());
            self.events.emit(Event::EncoderStatus { id: s.id.clone(), state: state.into(), message: msg.into() });
        }
    }

    fn run(self: Arc<Self>, ec: EncoderConfig, stop: Arc<AtomicBool>, status: Arc<Mutex<EncoderStatus>>) {
        let ffmpeg = resolve_ffmpeg(&self.cfg.read());
        while !stop.load(Ordering::Relaxed) {
            self.set_state(&status, "connecting", "");
            let engine_rate = self.engine.sample_rate();
            // výstupní větev processingu (převzorkování → trim → true-peak limiter) přímo před enkodérem
            let out_rate = if ec.format == StreamFormat::Opus { 48000 } else { ec.sample_rate };
            let mut branch = crate::dsp::branch_for(&ec.id, engine_rate, out_rate);
            let rate = branch.as_ref().map(|b| b.fs_out).unwrap_or(engine_rate);
            let mut seen_gen = crate::dsp::runtime_gen().load(Ordering::Relaxed);
            let mut meter = branch.as_ref().map(|b| crate::dsp::Meter::new(&b.key, b.fs_out));
            let mut meter_reset = crate::dsp::meter_reset_gen().load(Ordering::Relaxed);
            let mut pending: Option<String> = None;
            let mut processed_buf: Vec<f32> = Vec::with_capacity(4096);
            let mut since_pub = Instant::now();
            let mut cmd = Command::new(&ffmpeg);
            cmd.args(ffmpeg_args(&ec, rate)).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
            #[cfg(windows)]
            { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
            let mut child: Child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    self.set_state(&status, "error", &format!("ffmpeg nelze spustit ({}): {e}", ffmpeg.display()));
                    self.wait_or_stop(&stop, ec.reconnect_s.max(2));
                    continue;
                }
            };
            let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
            if let Some(err) = child.stderr.take() {
                let tail = tail.clone();
                std::thread::spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        log::debug!("ffmpeg: {line}");
                        let mut t = tail.lock(); t.push_back(line); if t.len() > 8 { t.pop_front(); }
                    }
                });
            }
            let mut stdin = child.stdin.take().unwrap();
            let rx = self.engine.add_tap();
            let started = Instant::now();
            let mut live = false;
            let bytes = AtomicU64::new(0);
            let mut bytebuf: Vec<u8> = Vec::with_capacity(4096);
            let reason = loop {
                if stop.load(Ordering::Relaxed) { break String::new(); }
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(block) => {
                        // změny processingu zaživa (trim, strop); změna topologie čeká na restart streamu
                        let g = crate::dsp::runtime_gen().load(Ordering::Relaxed);
                        if g != seen_gen {
                            seen_gen = g;
                            let rt = crate::dsp::runtime().read().clone();
                            let want = rt.branches.iter().find(|b| b.target == ec.id && b.enabled && !rt.bypass).cloned();
                            pending = match (branch.as_mut(), want) {
                                (Some(b), Some(c)) => if b.set_conf(c) { None } else { Some("změna vzorkovací frekvence se projeví po restartu streamu".into()) },
                                (None, None) => None,
                                (Some(_), None) => Some("vypnutí výstupní ochrany se projeví po restartu streamu".into()),
                                (None, Some(_)) => Some("zapnutí výstupní ochrany se projeví po restartu streamu".into()),
                            };
                        }
                        processed_buf.clear();
                        match branch.as_mut() {
                            Some(b) => b.process(&block, &mut processed_buf),
                            None => {
                                // master processing bez výstupní větve: bezpečnostní soft clip
                                let rt_on = crate::dsp::runtime().read().enabled;
                                if rt_on { processed_buf.extend(block.iter().map(|x| { let a = x.abs(); if a <= 0.9 { *x } else { x.signum() * (0.9 + 0.1 * ((a - 0.9) / 0.1).tanh()) } })); }
                                else { processed_buf.extend_from_slice(&block); }
                            }
                        }
                        if let (Some(m), Some(b)) = (meter.as_mut(), branch.as_ref()) {
                            let rg = crate::dsp::meter_reset_gen().load(Ordering::Relaxed);
                            if rg != meter_reset { meter_reset = rg; m.reset(); }
                            m.feed(&processed_buf);
                            if since_pub.elapsed() > Duration::from_millis(300) {
                                since_pub = Instant::now();
                                let mut s = m.snap.clone();
                                s.limiter_gr_db = Some(b.limiter_gr_db());
                                s.latency_ms = Some(b.latency_ms());
                                s.pending = pending.clone();
                                crate::dsp::publish(&s);
                            }
                        }
                        bytebuf.clear();
                        for s in processed_buf.iter() { bytebuf.extend_from_slice(&s.to_le_bytes()); }
                        if let Err(e) = stdin.write_all(&bytebuf) { break format!("spojení přerušeno: {e}"); }
                        bytes.fetch_add(bytebuf.len() as u64, Ordering::Relaxed);
                    }
                    Err(_) => {} // engine dočasně neposílá – ffmpeg necháme běžet
                }
                if let Ok(Some(code)) = child.try_wait() { break format!("ffmpeg skončil ({code})"); }
                if !live && started.elapsed() > Duration::from_secs(3) {
                    live = true;
                    self.set_state(&status, "live", "");
                    // po připojení pošli aktuální metadata
                    let m = self.last_meta.lock().clone();
                    if !m.is_empty() && ec.send_metadata { let ec2 = ec.clone(); std::thread::spawn(move || { let _ = send_metadata(&ec2, &m); }); }
                }
                status.lock().bytes_in = bytes.load(Ordering::Relaxed);
            };
            drop(stdin);
            drop(rx);
            let _ = child.kill();
            let _ = child.wait();
            if stop.load(Ordering::Relaxed) { break; }
            let detail = tail.lock().iter().cloned().collect::<Vec<_>>().join(" | ");
            let msg = if detail.is_empty() { reason } else { format!("{reason}: {detail}") };
            self.set_state(&status, "error", &msg);
            self.events.alert("warning", "encoder_disconnected", format!("{}: {msg}", ec.name));
            self.wait_or_stop(&stop, ec.reconnect_s.max(2));
        }
        status.lock().state = "stopped".into();
    }

    fn wait_or_stop(&self, stop: &AtomicBool, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < until && !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(200)); }
    }

    /// Aktualizace "StreamTitle" na všech běžících encoderech.
    pub fn update_metadata(&self, text: &str) {
        *self.last_meta.lock() = text.to_string();
        let encs: Vec<EncoderConfig> = {
            let run = self.running.lock();
            self.cfg.read().encoders.iter().filter(|e| e.send_metadata && run.get(&e.id).map(|r| r.status.lock().state == "live").unwrap_or(false)).cloned().collect()
        };
        for e in encs {
            let t = text.to_string();
            std::thread::spawn(move || { if let Err(err) = send_metadata(&e, &t) { log::warn!("metadata {}: {err}", e.name); } });
        }
    }
}

/// Icecast admin API: /admin/metadata?mode=updinfo (MP3/AAC; u Ogg nese metadata stream sám).
pub fn send_metadata(e: &EncoderConfig, text: &str) -> anyhow::Result<()> {
    if matches!(e.format, StreamFormat::Ogg | StreamFormat::Opus) { return Ok(()); }
    let mount = if e.mount.starts_with('/') { e.mount.clone() } else { format!("/{}", e.mount) };
    let url = format!("http://{}:{}/admin/metadata?mount={}&mode=updinfo&charset=UTF-8&song={}", e.host, e.port, pct(&mount), pct(text));
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", e.username, e.password));
    ureq::get(&url).set("Authorization", &format!("Basic {auth}")).timeout(Duration::from_secs(5)).call()?;
    Ok(())
}
