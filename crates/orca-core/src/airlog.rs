//! Monitoring – nahrávání vysílání (logger / archiv vysílání).
//!
//! Každý záznamník (logger) nahrává nepřetržitě 24/7 do souborů po N minutách zarovnaných na celé hodiny
//! (např. 2026-11-26_13-00-00.mp3). Zdroj: program ORCA (přesně to, co jde do vysílání), vstup zvukové
//! karty (např. FM přijímač – „co opravdu šlo do éteru“) nebo internetový stream (URL).
//! K souborům: kontrolní součty SHA-256 (důkaz neporušenosti), značky (ticho, mikrofon, start/stop),
//! automatické mazání po N dnech / nad limit místa, zamčené dny (spor s klientem), výřezy, aircheck
//! (jen mluvené vstupy) a pravidelné záznamy pořadů (i rovnou do knihovny pro reprízu).

use crate::config::Config;
use crate::encoder::resolve_ffmpeg;
use crate::engine::Engine;
use crate::events::EventBus;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Nastavení

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LoggerDef {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// program | input | url
    pub source: String,
    /// vstup zvukové karty (source = input); prázdné = výchozí vstup
    pub device: String,
    /// adresa streamu (source = url)
    pub url: String,
    /// mp3 | aac | opus | flac | wav
    pub format: String,
    pub bitrate_kbps: u32,
    pub mono: bool,
    pub sample_rate: u32,
    /// délka souboru v minutách (zarovnáno na celé hodiny): 15 | 30 | 60 | 120 | 1440
    pub split_min: u32,
    /// mazat záznamy starší než N dní (0 = nemazat)
    pub retention_days: u32,
    /// limit místa v GB (0 = bez limitu) – při překročení se mažou nejstarší (ne zamčené)
    pub max_gb: f64,
    /// složka; prázdné = složka dat / Logger / <id>
    pub dir: String,
    /// ticho: práh (dBFS) a doba (s) – značka + upozornění
    pub silence_db: f32,
    pub silence_s: u32,
    pub silence_alert: bool,
    /// kontrolní součty SHA-256 dokončených souborů
    pub hash: bool,
}

impl Default for LoggerDef {
    fn default() -> Self {
        Self {
            id: "program".into(), name: "Program (vysílání)".into(), enabled: false, source: "program".into(), device: String::new(), url: String::new(),
            format: "mp3".into(), bitrate_kbps: 64, mono: true, sample_rate: 44100, split_min: 60, retention_days: 90, max_gb: 0.0, dir: String::new(),
            silence_db: -50.0, silence_s: 30, silence_alert: true, hash: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShowRecording {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// dny v týdnu 1 = pondělí … 7 = neděle
    pub days: Vec<u8>,
    /// HH:MM
    pub start: String,
    pub end: String,
    /// ze kterého záznamníku vyříznout
    pub logger_id: String,
    /// jen mluvené vstupy (aircheck) místo celého pořadu
    pub aircheck: bool,
    /// přidat do knihovny (prázdné = ne): druh a kategorie
    pub library_kind: String,
    pub library_category: String,
    /// mazat výřezy starší než N dní (0 = nemazat)
    pub keep_days: u32,
    /// vytvořit z nahrávky úpravu pro podcast (s výchozí znělkou a sweeperem)
    pub podcast: bool,
    /// rovnou vyrobit epizodu a zveřejnit (jinak čeká na kontrolu v editoru)
    pub podcast_auto_publish: bool,
}

impl Default for ShowRecording {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), enabled: true, days: vec![1, 2, 3, 4, 5], start: "06:00".into(), end: "09:00".into(), logger_id: "program".into(), aircheck: false, library_kind: String::new(), library_category: String::new(), keep_days: 30, podcast: false, podcast_auto_publish: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AirLogConfig {
    pub loggers: Vec<LoggerDef>,
    pub shows: Vec<ShowRecording>,
    /// aircheck: kolik sekund před a po mluveném vstupu přidat
    pub aircheck_pre_s: u32,
    pub aircheck_post_s: u32,
}

impl Default for AirLogConfig {
    fn default() -> Self { Self { loggers: vec![LoggerDef::default()], shows: vec![], aircheck_pre_s: 3, aircheck_post_s: 2 } }
}

pub fn ext_of(format: &str) -> &'static str {
    match format { "aac" => "m4a", "opus" => "opus", "flac" => "flac", "wav" => "wav", _ => "mp3" }
}

/// Odhad místa na den (bajty) – pro nastavení.
pub fn bytes_per_day(l: &LoggerDef) -> u64 {
    let ch = if l.mono { 1 } else { 2 };
    match l.format.as_str() {
        "wav" => l.sample_rate as u64 * ch * 2 * 86400,
        "flac" => l.sample_rate as u64 * ch * 2 * 86400 * 6 / 10,
        _ => l.bitrate_kbps as u64 * 1000 / 8 * 86400,
    }
}

pub fn logger_dir(data_dir: &Path, l: &LoggerDef) -> PathBuf {
    if l.dir.trim().is_empty() { data_dir.join("Logger").join(crate::library::sanitize(&l.id)) } else { PathBuf::from(l.dir.trim()) }
}

// ---------------------------------------------------------------------------------------------
// Stav

#[derive(Debug, Clone, Serialize, Default)]
pub struct LoggerStatus {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// off | starting | recording | error
    pub state: String,
    pub message: String,
    pub dir: String,
    pub current_file: String,
    pub since: Option<String>,
    pub level_db: f32,
    pub silent_since: Option<String>,
    pub files: usize,
    pub total_bytes: u64,
    pub today_bytes: u64,
    pub oldest: Option<String>,
    pub bytes_per_day: u64,
    pub disk_free: Option<u64>,
    pub kept_days: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mark { pub t: String, pub kind: String, #[serde(default)] pub text: String }

#[derive(Debug, Clone, Serialize)]
pub struct Segment { pub name: String, pub start: String, pub end: String, pub bytes: u64, pub hash: Option<String>, pub hash_ok: Option<bool>, pub recording: bool }

struct Running { def: LoggerDef, stop: Arc<AtomicBool>, status: Arc<Mutex<LoggerStatus>> }

pub struct AirLogger {
    data_dir: PathBuf,
    cfg: Arc<RwLock<Config>>,
    engine: Arc<Engine>,
    events: EventBus,
    running: Mutex<HashMap<String, Running>>,
    pending_library: Mutex<Vec<(PathBuf, crate::model::TrackKind, String)>>,
    pending_podcast: Mutex<Vec<(PathBuf, String, bool)>>,
}

impl AirLogger {
    pub fn start(data_dir: PathBuf, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, events: EventBus) -> Arc<Self> {
        let me = Arc::new(Self { data_dir, cfg, engine, events, running: Mutex::new(HashMap::new()), pending_library: Mutex::new(vec![]), pending_podcast: Mutex::new(vec![]) });
        let m = me.clone();
        std::thread::Builder::new().name("orca-airlog".into()).spawn(move || {
            let mut last_house = Instant::now() - Duration::from_secs(3600);
            loop {
                m.sync();
                if last_house.elapsed() > Duration::from_secs(600) { last_house = Instant::now(); m.housekeeping(); }
                m.shows_tick();
                std::thread::sleep(Duration::from_secs(2));
            }
        }).ok();
        me
    }

    /// Spustí / zastaví / restartuje záznamníky podle nastavení.
    fn sync(self: &Arc<Self>) {
        let defs = self.cfg.read().airlog.loggers.clone();
        let mut run = self.running.lock();
        run.retain(|id, r| {
            let keep = defs.iter().any(|d| &d.id == id && d.enabled && *d == r.def);
            if !keep { r.stop.store(true, Ordering::SeqCst); }
            keep
        });
        for d in defs.into_iter().filter(|d| d.enabled) {
            if run.contains_key(&d.id) { continue; }
            let stop = Arc::new(AtomicBool::new(false));
            let status = Arc::new(Mutex::new(LoggerStatus { id: d.id.clone(), name: d.name.clone(), enabled: true, state: "starting".into(), ..Default::default() }));
            let (me, dd, st, sp) = (self.clone(), d.clone(), status.clone(), stop.clone());
            std::thread::Builder::new().name(format!("orca-airlog-{}", d.id)).spawn(move || me.run_logger(dd, st, sp)).ok();
            run.insert(d.id.clone(), Running { def: d, stop, status });
        }
    }

    pub fn status(&self) -> Vec<LoggerStatus> {
        let defs = self.cfg.read().airlog.loggers.clone();
        let run = self.running.lock();
        defs.iter().map(|d| {
            let mut s = run.get(&d.id).map(|r| r.status.lock().clone()).unwrap_or_else(|| LoggerStatus { id: d.id.clone(), name: d.name.clone(), state: "off".into(), ..Default::default() });
            s.name = d.name.clone(); s.enabled = d.enabled;
            let dir = logger_dir(&self.data_dir, d);
            s.dir = dir.to_string_lossy().to_string();
            let files = list_files(&dir, ext_of(&d.format));
            s.files = files.len();
            s.total_bytes = files.iter().map(|f| f.2).sum();
            let today = Local::now().format("%Y-%m-%d").to_string();
            s.today_bytes = files.iter().filter(|f| f.0.starts_with(&today)).map(|f| f.2).sum();
            s.oldest = files.first().map(|f| f.1.to_rfc3339());
            s.bytes_per_day = bytes_per_day(d);
            s.disk_free = fs4::available_space(if dir.exists() { &dir } else { &self.data_dir }).ok();
            s.kept_days = kept(&dir).into_iter().collect();
            s
        }).collect()
    }

    fn def(&self, id: &str) -> Result<LoggerDef> {
        self.cfg.read().airlog.loggers.iter().find(|l| l.id == id).cloned().ok_or_else(|| anyhow!("záznamník {id} neexistuje"))
    }

    // -----------------------------------------------------------------------------------------
    // Nahrávání

    fn run_logger(self: Arc<Self>, d: LoggerDef, status: Arc<Mutex<LoggerStatus>>, stop: Arc<AtomicBool>) {
        let dir = logger_dir(&self.data_dir, &d);
        let mut fails = 0u32;
        while !stop.load(Ordering::Relaxed) {
            let r = self.record_once(&d, &dir, &status, &stop);
            if stop.load(Ordering::Relaxed) { break; }
            let msg = match r { Ok(()) => "záznam se přerušil".to_string(), Err(e) => e.to_string() };
            { let mut s = status.lock(); s.state = "error".into(); s.message = msg.clone(); }
            add_mark(&dir, "gap", &msg);
            fails += 1;
            if fails == 1 || fails % 30 == 0 { self.events.alert("warning", "airlog", format!("Nahrávání vysílání „{}“: {msg}", d.name)); }
            let until = Instant::now() + Duration::from_secs(if fails < 3 { 3 } else { 10 });
            while Instant::now() < until && !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(200)); }
        }
        add_mark(&dir, "stop", "");
        let mut s = status.lock(); s.state = "off".into(); s.message.clear(); s.current_file.clear();
    }

    fn record_once(&self, d: &LoggerDef, dir: &Path, status: &Arc<Mutex<LoggerStatus>>, stop: &Arc<AtomicBool>) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("složku {} nelze vytvořit", dir.display()))?;
        let ff = resolve_ffmpeg(&self.cfg.read());
        let ext = ext_of(&d.format);
        let rate = self.engine.sample_rate().max(8000);
        let mut cmd = Command::new(&ff);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        // vstup
        let (pcm_in, in_rate) = match d.source.as_str() {
            "url" => {
                if d.url.trim().is_empty() { return Err(anyhow!("chybí adresa streamu")); }
                cmd.args(["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "10", "-i", d.url.trim()]);
                (false, 0)
            }
            "input" => { let r = input_rate(&d.device)?; cmd.args(["-f", "f32le", "-ar", &r.to_string(), "-ac", "2", "-i", "pipe:0"]); (true, r) }
            _ => { cmd.args(["-f", "f32le", "-ar", &rate.to_string(), "-ac", "2", "-i", "pipe:0"]); (true, rate) }
        };
        let _ = in_rate;
        // kódování
        cmd.args(["-vn", "-ac", if d.mono { "1" } else { "2" }, "-ar", &d.sample_rate.clamp(8000, 96000).to_string()]);
        match d.format.as_str() {
            "aac" => { cmd.args(["-c:a", "aac", "-b:a", &format!("{}k", d.bitrate_kbps)]); }
            "opus" => { cmd.args(["-c:a", "libopus", "-b:a", &format!("{}k", d.bitrate_kbps)]); }
            "flac" => { cmd.args(["-c:a", "flac"]); }
            "wav" => { cmd.args(["-c:a", "pcm_s16le"]); }
            _ => { cmd.args(["-c:a", "libmp3lame", "-b:a", &format!("{}k", d.bitrate_kbps)]); }
        }
        let seg = (d.split_min.clamp(5, 1440) as u64) * 60;
        let list = dir.join(".segments.csv");
        cmd.args(["-f", "segment", "-segment_time", &seg.to_string(), "-segment_atclocktime", "1", "-reset_timestamps", "1", "-strftime", "1",
            "-segment_list_type", "csv", "-segment_list"]).arg(&list);
        if ext == "m4a" { cmd.args(["-segment_format", "ipod"]); }
        cmd.arg(dir.join(format!("%Y-%m-%d_%H-%M-%S.{ext}")));
        cmd.stdin(if pcm_in { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::null()).stderr(Stdio::piped());
        #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
        let mut child: Child = cmd.spawn().map_err(|e| anyhow!("ffmpeg nelze spustit ({}): {e}", ff.display()))?;
        let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        if let Some(err) = child.stderr.take() {
            let tail = tail.clone();
            std::thread::spawn(move || for line in BufReader::new(err).lines().map_while(Result::ok) { let mut t = tail.lock(); t.push(line); if t.len() > 6 { t.remove(0); } });
        }
        {
            let mut s = status.lock();
            s.state = "recording".into(); s.message.clear(); s.since = Some(Local::now().to_rfc3339());
        }
        add_mark(dir, "start", match d.source.as_str() { "url" => "stream", "input" => "vstup", _ => "program" });
        let mut sil = Silence::new(d.silence_db, d.silence_s);
        let mut last_mic = false;
        let mut last_hash = Instant::now();
        let mut done_hash: BTreeSet<String> = BTreeSet::new();
        let res: Result<()> = (|| {
            if pcm_in {
                let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("ffmpeg bez vstupu"))?;
                let (rx, _guard) = if d.source == "input" { let (rx, g) = open_input(&d.device)?; (rx, Some(g)) } else { ({ let c = self.cfg.read().clone(); crate::fx::tap_for(&self.engine, &c, "logger") }, None) };
                let mut bytes: Vec<u8> = Vec::with_capacity(16384);
                loop {
                    if stop.load(Ordering::Relaxed) { return Ok(()); }
                    if let Ok(Some(code)) = child.try_wait() { return Err(anyhow!("ffmpeg skončil ({code}): {}", tail.lock().join(" | "))); }
                    match rx.recv_timeout(Duration::from_secs(3)) {
                        Ok(block) => {
                            let lvl = sil.feed(&block, rate_of(d, rate));
                            status.lock().level_db = lvl;
                            bytes.clear();
                            for x in block.iter() { bytes.extend_from_slice(&x.to_le_bytes()); }
                            stdin.write_all(&bytes).map_err(|e| anyhow!("zápis do ffmpeg: {e} {}", tail.lock().join(" | ")))?;
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            // engine neposílá zvuk (nehraje) – zapsat ticho, aby časová osa seděla
                            let n = (rate_of(d, rate) as usize) * 2 * 3;
                            bytes.clear(); bytes.resize(n * 4, 0);
                            stdin.write_all(&bytes).map_err(|e| anyhow!("zápis do ffmpeg: {e}"))?;
                            sil.feed(&vec![0.0; n], rate_of(d, rate));
                        }
                        Err(_) => return Err(anyhow!("zdroj zvuku se odpojil")),
                    }
                    self.tick(d, dir, status, &mut sil, &mut last_mic, &mut last_hash, &mut done_hash);
                }
            } else {
                loop {
                    if stop.load(Ordering::Relaxed) { return Ok(()); }
                    if let Ok(Some(code)) = child.try_wait() { return Err(anyhow!("ffmpeg skončil ({code}): {}", tail.lock().join(" | "))); }
                    std::thread::sleep(Duration::from_millis(500));
                    self.tick(d, dir, status, &mut sil, &mut last_mic, &mut last_hash, &mut done_hash);
                }
            }
        })();
        let _ = child.kill();
        let _ = child.wait();
        self.hash_new(d, dir, &mut done_hash);
        res
    }

    #[allow(clippy::too_many_arguments)]
    fn tick(&self, d: &LoggerDef, dir: &Path, status: &Arc<Mutex<LoggerStatus>>, sil: &mut Silence, last_mic: &mut bool, last_hash: &mut Instant, done: &mut BTreeSet<String>) {
        // ticho
        match sil.edge() {
            Some(true) => {
                add_mark(dir, "silence", "");
                status.lock().silent_since = Some(Local::now().to_rfc3339());
                if d.silence_alert { self.events.alert("warning", "airlog_silence", format!("{}: ticho déle než {} s", d.name, d.silence_s)); }
            }
            Some(false) => { add_mark(dir, "silence_end", ""); status.lock().silent_since = None; }
            None => {}
        }
        // mikrofon / živý vstup (pro aircheck)
        let mic = crate::mic::status().on_air || self.engine.live_active() || self.engine.talkover();
        if mic != *last_mic { *last_mic = mic; add_mark(dir, if mic { "mic_on" } else { "mic_off" }, ""); }
        // aktuální soubor
        if last_hash.elapsed() > Duration::from_secs(5) {
            *last_hash = Instant::now();
            let files = list_files(dir, ext_of(&d.format));
            if let Some(f) = files.last() { status.lock().current_file = f.0.clone(); }
            if d.hash { self.hash_new(d, dir, done); }
        }
    }

    /// SHA-256 dokončených souborů (podle seznamu, který ffmpeg doplňuje po uzavření souboru).
    fn hash_new(&self, d: &LoggerDef, dir: &Path, done: &mut BTreeSet<String>) {
        if !d.hash { return; }
        let Ok(text) = std::fs::read_to_string(dir.join(".segments.csv")) else { return };
        for line in text.lines() {
            let name = line.split(',').next().unwrap_or("").trim().trim_matches('"').to_string();
            if name.is_empty() || done.contains(&name) { continue; }
            let p = dir.join(&name);
            let hp = dir.join(format!("{name}.sha256"));
            if p.is_file() && !hp.exists() {
                if let Ok(h) = sha256_file(&p) { let _ = std::fs::write(&hp, format!("{h}  {name}\n")); }
            }
            done.insert(name);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Úklid: mazání po N dnech a nad limit místa (zamčené dny se nemažou)

    pub fn housekeeping(&self) {
        let cfg = self.cfg.read().airlog.clone();
        for d in &cfg.loggers {
            let dir = logger_dir(&self.data_dir, d);
            if !dir.is_dir() { continue; }
            let keep = kept(&dir);
            let mut files = list_files(&dir, ext_of(&d.format));
            if files.len() > 1 { files.pop(); } // právě nahrávaný soubor nikdy
            let now = Local::now();
            let mut removed = 0;
            if d.retention_days > 0 {
                let limit = now - chrono::Duration::days(d.retention_days as i64);
                for f in files.iter().filter(|f| f.1 < limit && !keep.contains(&f.0[..10])) { if remove_seg(&dir, &f.0) { removed += 1; } }
            }
            if d.max_gb > 0.0 {
                let mut files = list_files(&dir, ext_of(&d.format)); if files.len() > 1 { files.pop(); }
                let mut total: u64 = files.iter().map(|f| f.2).sum();
                let max = (d.max_gb * 1e9) as u64;
                for f in files.iter().filter(|f| !keep.contains(&f.0[..10])) { if total <= max { break; } if remove_seg(&dir, &f.0) { total = total.saturating_sub(f.2); removed += 1; } }
            }
            // staré značky
            if d.retention_days > 0 {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    let limit = (now - chrono::Duration::days(d.retention_days as i64 + 1)).format("%Y-%m-%d").to_string();
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().to_string();
                        if let Some(day) = n.strip_prefix("marks-").and_then(|x| x.strip_suffix(".jsonl")) { if day < limit.as_str() && !keep.contains(day) { let _ = std::fs::remove_file(e.path()); } }
                    }
                }
            }
            if removed > 0 { log::info!("nahrávání vysílání {}: smazáno {removed} starých souborů", d.name); }
        }
        for s in &cfg.shows {
            if s.keep_days == 0 { continue; }
            let dir = self.data_dir.join("Logger").join("Pořady").join(crate::library::sanitize(&s.name));
            let limit = std::time::SystemTime::now() - Duration::from_secs(s.keep_days as u64 * 86400);
            if let Ok(rd) = std::fs::read_dir(&dir) { for e in rd.flatten() { if e.metadata().and_then(|m| m.modified()).map(|m| m < limit).unwrap_or(false) { let _ = std::fs::remove_file(e.path()); } } }
        }
    }

    pub fn set_kept(&self, id: &str, day: &str, keep: bool) -> Result<()> {
        let d = self.def(id)?;
        let dir = logger_dir(&self.data_dir, &d);
        std::fs::create_dir_all(&dir)?;
        NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| anyhow!("datum ve tvaru RRRR-MM-DD"))?;
        let mut k = kept(&dir);
        if keep { k.insert(day.to_string()); } else { k.remove(day); }
        std::fs::write(dir.join(".keep"), k.into_iter().collect::<Vec<_>>().join("\n"))?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Prohlížení

    pub fn day(&self, id: &str, day: NaiveDate) -> Result<(Vec<Segment>, Vec<Mark>)> {
        let d = self.def(id)?;
        let dir = logger_dir(&self.data_dir, &d);
        let files = list_files(&dir, ext_of(&d.format));
        let day0 = Local.from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap()).single().ok_or_else(|| anyhow!("neplatné datum"))?;
        let day1 = day0 + chrono::Duration::days(1);
        let cur = files.last().map(|f| f.0.clone());
        let recording = self.running.lock().get(id).map(|r| r.status.lock().state == "recording").unwrap_or(false);
        let mut out = vec![];
        for (i, f) in files.iter().enumerate() {
            let end = files.get(i + 1).map(|n| n.1).filter(|n| *n - f.1 <= chrono::Duration::minutes(d.split_min as i64 + 1))
                .unwrap_or_else(|| { let m = std::fs::metadata(dir.join(&f.0)).and_then(|m| m.modified()).map(DateTime::<Local>::from).unwrap_or(f.1); m.max(f.1) });
            if end <= day0 || f.1 >= day1 { continue; }
            let hp = dir.join(format!("{}.sha256", f.0));
            let hash = std::fs::read_to_string(&hp).ok().and_then(|t| t.split_whitespace().next().map(str::to_string));
            out.push(Segment { name: f.0.clone(), start: f.1.to_rfc3339(), end: end.to_rfc3339(), bytes: f.2, hash, hash_ok: None, recording: recording && Some(&f.0) == cur.as_ref() });
        }
        let mut marks = vec![];
        for dd in [day.pred_opt().unwrap_or(day), day] {
            if let Ok(t) = std::fs::read_to_string(dir.join(format!("marks-{}.jsonl", dd.format("%Y-%m-%d")))) {
                marks.extend(t.lines().filter_map(|l| serde_json::from_str::<Mark>(l).ok()));
            }
        }
        marks.retain(|m| DateTime::parse_from_rfc3339(&m.t).map(|t| t >= day0 - chrono::Duration::hours(6) && t < day1).unwrap_or(false));
        Ok((out, marks))
    }

    /// Ověří kontrolní součet souboru.
    pub fn verify(&self, id: &str, name: &str) -> Result<bool> {
        let d = self.def(id)?;
        let dir = logger_dir(&self.data_dir, &d);
        let p = safe_child(&dir, name)?;
        let want = std::fs::read_to_string(dir.join(format!("{name}.sha256"))).ok().and_then(|t| t.split_whitespace().next().map(str::to_string)).ok_or_else(|| anyhow!("soubor nemá kontrolní součet"))?;
        Ok(sha256_file(&p)? == want)
    }

    pub fn file_path(&self, id: &str, name: &str) -> Result<PathBuf> {
        let d = self.def(id)?;
        safe_child(&logger_dir(&self.data_dir, &d), name)
    }

    /// Soubor a pozice v něm pro daný okamžik.
    pub fn locate(&self, id: &str, at: DateTime<Local>) -> Result<(PathBuf, i64)> {
        let d = self.def(id)?;
        let dir = logger_dir(&self.data_dir, &d);
        let files = list_files(&dir, ext_of(&d.format));
        let f = files.iter().rev().find(|f| f.1 <= at).ok_or_else(|| anyhow!("pro tento čas není záznam"))?;
        Ok((dir.join(&f.0), (at - f.1).num_milliseconds().max(0)))
    }

    // -----------------------------------------------------------------------------------------
    // Výřezy, aircheck

    /// Vyřízne úsek [from, to) do souboru ve složce Exports/Logger. aircheck = jen mluvené vstupy (podle značek mikrofonu).
    pub fn export(&self, id: &str, from: DateTime<Local>, to: DateTime<Local>, format: &str, aircheck: bool, name_hint: &str, out_dir: Option<PathBuf>) -> Result<PathBuf> {
        if to <= from { return Err(anyhow!("konec musí být po začátku")); }
        if to - from > chrono::Duration::hours(25) { return Err(anyhow!("výřez může mít nejvýš 25 hodin")); }
        let d = self.def(id)?;
        let dir = logger_dir(&self.data_dir, &d);
        let files = list_files(&dir, ext_of(&d.format));
        let first = files.iter().rposition(|f| f.1 <= from).unwrap_or(0);
        let parts: Vec<&(String, DateTime<Local>, u64)> = files.iter().skip(first).filter(|f| f.1 < to).collect();
        if parts.is_empty() { return Err(anyhow!("v tomto čase není žádný záznam")); }
        let base = parts[0].1;
        // úseky k vyříznutí (sekundy od začátku prvního souboru)
        let mut ranges: Vec<(f64, f64)> = vec![];
        let s = |t: DateTime<Local>| (t - base).num_milliseconds() as f64 / 1000.0;
        if aircheck {
            let cfg = self.cfg.read().airlog.clone();
            let mut on: Option<DateTime<Local>> = None;
            let mut day = from.date_naive();
            let mut marks: Vec<Mark> = vec![];
            while day <= to.date_naive() {
                if let Ok(t) = std::fs::read_to_string(dir.join(format!("marks-{}.jsonl", day.format("%Y-%m-%d")))) { marks.extend(t.lines().filter_map(|l| serde_json::from_str::<Mark>(l).ok())); }
                day = day.succ_opt().unwrap_or(day); if day == to.date_naive().succ_opt().unwrap_or(day) { break; }
            }
            for m in marks {
                let Ok(t) = DateTime::parse_from_rfc3339(&m.t) else { continue };
                let t = t.with_timezone(&Local);
                match m.kind.as_str() {
                    "mic_on" => if on.is_none() { on = Some(t) },
                    "mic_off" => if let Some(a) = on.take() {
                        let a = (a - chrono::Duration::seconds(cfg.aircheck_pre_s as i64)).max(from);
                        let b = (t + chrono::Duration::seconds(cfg.aircheck_post_s as i64)).min(to);
                        if b > a { ranges.push((s(a), s(b))); }
                    },
                    _ => {}
                }
            }
            if let Some(a) = on { let a = (a - chrono::Duration::seconds(cfg.aircheck_pre_s as i64)).max(from); if to > a { ranges.push((s(a), s(to))); } }
            if ranges.is_empty() { return Err(anyhow!("v tomto úseku nebyl žádný mluvený vstup (mikrofon)")); }
        }
        let out_dir = out_dir.unwrap_or_else(|| self.data_dir.join("Exports").join("Logger"));
        std::fs::create_dir_all(&out_dir)?;
        let ext = match format { "wav" => "wav", "flac" => "flac", _ => "mp3" };
        let stem = crate::library::sanitize(&format!("{}{}–{}{}", if name_hint.is_empty() { String::new() } else { format!("{name_hint}_") }, from.format("%Y-%m-%d_%H-%M-%S"), to.format("%H-%M-%S"), if aircheck { "_aircheck" } else { "" }));
        let out = out_dir.join(format!("{stem}.{ext}"));
        let list = std::env::temp_dir().join(format!("orca-airlog-{}.txt", uuid::Uuid::new_v4().simple()));
        {
            let mut f = std::fs::File::create(&list)?;
            for p in &parts { writeln!(f, "file '{}'", dir.join(&p.0).to_string_lossy().replace('\'', "'\\''"))?; }
        }
        let ff = resolve_ffmpeg(&self.cfg.read());
        let mut cmd = Command::new(ff);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"]).arg(&list);
        if aircheck {
            let expr = ranges.iter().map(|(a, b)| format!("between(t,{a:.2},{b:.2})")).collect::<Vec<_>>().join("+");
            cmd.args(["-af", &format!("aselect='{expr}',asetpts=N/SR/TB")]);
        } else {
            cmd.args(["-ss", &format!("{:.3}", s(from).max(0.0)), "-t", &format!("{:.3}", (to - from).num_milliseconds() as f64 / 1000.0)]);
        }
        match ext { "wav" => { cmd.args(["-c:a", "pcm_s16le"]); } "flac" => { cmd.args(["-c:a", "flac"]); } _ => { cmd.args(["-c:a", "libmp3lame", "-b:a", "192k"]); } }
        cmd.arg(&out);
        #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
        let o = cmd.output()?;
        let _ = std::fs::remove_file(&list);
        if !o.status.success() { return Err(anyhow!("výřez se nepodařil: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""))); }
        Ok(out)
    }

    /// Pravidelné záznamy pořadů: po skončení pořadu vyřízne úsek (případně aircheck) a volitelně přidá do knihovny.
    fn shows_tick(&self) {
        let shows = self.cfg.read().airlog.shows.clone();
        if shows.is_empty() { return; }
        let now = Local::now();
        for s in shows.iter().filter(|s| s.enabled && !s.name.trim().is_empty()) {
            let (Ok(st), Ok(en)) = (chrono::NaiveTime::parse_from_str(&s.start, "%H:%M"), chrono::NaiveTime::parse_from_str(&s.end, "%H:%M")) else { continue };
            // pořad, který skončil v posledních 6 hodinách (i přes půlnoc)
            for back in 0..2 {
                let day = now.date_naive() - chrono::Duration::days(back);
                if !s.days.contains(&(day.weekday().number_from_monday() as u8)) { continue; }
                let Some(from) = Local.from_local_datetime(&day.and_time(st)).single() else { continue };
                let mut to = Local.from_local_datetime(&day.and_time(en)).single().unwrap_or(from);
                if to <= from { to += chrono::Duration::days(1); }
                if now < to + chrono::Duration::seconds(90) || now > to + chrono::Duration::hours(6) { continue; }
                let dir = self.data_dir.join("Logger").join("Pořady").join(crate::library::sanitize(&s.name));
                let flag = dir.join(format!(".hotovo-{}", from.format("%Y-%m-%d_%H-%M")));
                if flag.exists() { continue; }
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(&flag, "");
                match self.export(&s.logger_id, from, to, "mp3", s.aircheck, &s.name, Some(dir.clone())) {
                    Ok(p) => {
                        log::info!("záznam pořadu {}: {}", s.name, p.display());
                        if s.podcast { self.pending_podcast.lock().push((p.clone(), format!("{} {}", s.name, from.format("%-d. %-m. %Y")), s.podcast_auto_publish)); }
                        if let Some(k) = crate::model::TrackKind::parse(&s.library_kind) { self.pending_library.lock().push((p, k, s.library_category.clone())); }
                    }
                    Err(e) => self.events.alert("warning", "airlog_show", format!("Záznam pořadu {}: {e}", s.name)),
                }
            }
        }
    }
}

// Záznamy pořadů určené do knihovny vyzvedne server (má přístup ke knihovně).
impl AirLogger {
    pub fn take_library_items(&self) -> Vec<(PathBuf, crate::model::TrackKind, String)> { std::mem::take(&mut *self.pending_library.lock()) }
    pub fn take_podcast_items(&self) -> Vec<(PathBuf, String, bool)> { std::mem::take(&mut *self.pending_podcast.lock()) }
}

// ---------------------------------------------------------------------------------------------
// Pomocné

fn rate_of(d: &LoggerDef, engine_rate: u32) -> u32 { if d.source == "input" { input_rate(&d.device).unwrap_or(48000) } else { engine_rate } }

/// Soubory záznamu seřazené podle času: (název, začátek, velikost)
pub fn list_files(dir: &Path, ext: &str) -> Vec<(String, DateTime<Local>, u64)> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let Some(stem) = n.strip_suffix(&format!(".{ext}")) else { continue };
            let Ok(nt) = NaiveDateTime::parse_from_str(stem, "%Y-%m-%d_%H-%M-%S") else { continue };
            let Some(t) = Local.from_local_datetime(&nt).earliest() else { continue };
            v.push((n, t, e.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }
    v.sort_by_key(|x| x.1);
    v
}

fn remove_seg(dir: &Path, name: &str) -> bool {
    let ok = std::fs::remove_file(dir.join(name)).is_ok();
    let _ = std::fs::remove_file(dir.join(format!("{name}.sha256")));
    ok
}

fn kept(dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(dir.join(".keep")).map(|t| t.lines().map(|l| l.trim().to_string()).filter(|l| l.len() == 10).collect()).unwrap_or_default()
}

fn add_mark(dir: &Path, kind: &str, text: &str) {
    let now = Local::now();
    let _ = std::fs::create_dir_all(dir);
    let line = serde_json::to_string(&Mark { t: now.to_rfc3339(), kind: kind.into(), text: text.into() }).unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("marks-{}.jsonl", now.format("%Y-%m-%d")))) { let _ = writeln!(f, "{line}"); }
}

fn safe_child(dir: &Path, name: &str) -> Result<PathBuf> {
    if name.contains('/') || name.contains('\\') || name.contains("..") { return Err(anyhow!("neplatný název souboru")); }
    let p = dir.join(name);
    if !p.is_file() { return Err(anyhow!("soubor neexistuje")); }
    Ok(p)
}

pub fn sha256_file(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Detektor ticha (špička pod prahem déle než N s).
struct Silence { thr: f32, need_s: f64, quiet_s: f64, silent: bool, changed: Option<bool>, level: f32 }
impl Silence {
    fn new(db: f32, s: u32) -> Self { Self { thr: 10f32.powf(db / 20.0), need_s: s.max(3) as f64, quiet_s: 0.0, silent: false, changed: None, level: -90.0 } }
    fn feed(&mut self, block: &[f32], rate: u32) -> f32 {
        let pk = block.iter().fold(0f32, |a, x| a.max(x.abs()));
        let secs = block.len() as f64 / 2.0 / rate.max(1) as f64;
        self.level = if pk > 0.0 { 20.0 * pk.log10() } else { -90.0 };
        if pk < self.thr { self.quiet_s += secs; if !self.silent && self.quiet_s >= self.need_s { self.silent = true; self.changed = Some(true); } }
        else { self.quiet_s = 0.0; if self.silent { self.silent = false; self.changed = Some(false); } }
        self.level
    }
    fn edge(&mut self) -> Option<bool> { self.changed.take() }
}

// Vstup zvukové karty (FM přijímač, odposlech éteru)
type InputGuard = Arc<AtomicBool>;
pub(crate) struct InputStop(InputGuard);
impl Drop for InputStop { fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); } }

fn find_input(name: &str) -> Result<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if name.is_empty() { return host.default_input_device().ok_or_else(|| anyhow!("v počítači není žádný zvukový vstup")); }
    host.input_devices()?.find(|d| d.name().map(|n| n == name).unwrap_or(false)).ok_or_else(|| anyhow!("vstup „{name}“ nenalezen"))
}
pub(crate) fn input_rate(name: &str) -> Result<u32> {
    use cpal::traits::DeviceTrait;
    Ok(find_input(name)?.default_input_config()?.sample_rate().0)
}

pub(crate) fn open_input(name: &str) -> Result<(crossbeam_channel::Receiver<Arc<Vec<f32>>>, InputStop)> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use cpal::SampleFormat;
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(400);
    let (otx, orx) = crossbeam_channel::bounded::<Arc<Vec<f32>>>(400);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
    let (name, st2) = (name.to_string(), stop.clone());
    std::thread::Builder::new().name("orca-airlog-in".into()).spawn(move || {
        let r = (|| -> Result<cpal::Stream> {
            let dev = find_input(&name)?;
            let c = dev.default_input_config()?;
            let sc: cpal::StreamConfig = c.config();
            let ch = sc.channels as usize;
            let st = match c.sample_format() {
                SampleFormat::F32 => crate::relay::build_input::<f32>(&dev, &sc, ch, tx)?,
                SampleFormat::I16 => crate::relay::build_input::<i16>(&dev, &sc, ch, tx)?,
                SampleFormat::I32 => crate::relay::build_input::<i32>(&dev, &sc, ch, tx)?,
                SampleFormat::U16 => crate::relay::build_input::<u16>(&dev, &sc, ch, tx)?,
                f => return Err(anyhow!("nepodporovaný formát {f:?}")),
            };
            st.play()?;
            Ok(st)
        })();
        match r {
            Ok(stream) => { let _ = ready_tx.send(Ok(())); while !st2.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(50)); } drop(stream); }
            Err(e) => { let _ = ready_tx.send(Err(e)); }
        }
    })?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => { stop.store(true, Ordering::SeqCst); return Err(anyhow!("vstup zvukové karty neodpovídá")); }
    }
    let st3 = stop.clone();
    std::thread::spawn(move || { while !st3.load(Ordering::Relaxed) { match rx.recv_timeout(Duration::from_millis(500)) { Ok(v) => { let _ = otx.try_send(Arc::new(v)); } Err(crossbeam_channel::RecvTimeoutError::Timeout) => {} Err(_) => break } } });
    Ok((orx, InputStop(stop)))
}
