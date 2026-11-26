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
    /// denní kopie záznamů předešlého dne na externí / síťový disk
    pub archive: ArchiveConfig,
}

impl Default for AirLogConfig {
    fn default() -> Self { Self { loggers: vec![LoggerDef::default()], shows: vec![], aircheck_pre_s: 3, aircheck_post_s: 2, archive: ArchiveConfig::default() } }
}

/// Archiv monitoringu: každý den v nastavený čas zkopíruje kompletní záznamy předešlého dne
/// do zvolené složky (externí disk, síťový disk / NAS) a ověří je kontrolním součtem SHA-256.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ArchiveConfig {
    pub enabled: bool,
    /// cílová složka (musí existovat – odpojený disk se nikdy nenahradí složkou na systémovém disku)
    pub dir: String,
    /// HH:MM – kdy kopírovat
    pub time: String,
    /// id záznamníků; prázdné = všechny
    pub loggers: Vec<String>,
    /// po ověřené kopii rovnou smazat ze systému (jinak se maže ručně po kontrole)
    pub delete_after: bool,
    /// kolik dní zpětně dohánět nezkopírované dny (počítač byl vypnutý, disk odpojený)
    pub catch_up_days: u32,
}

impl Default for ArchiveConfig {
    fn default() -> Self { Self { enabled: false, dir: String::new(), time: "02:30".into(), loggers: vec![], delete_after: false, catch_up_days: 7 } }
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
    archive_busy: Arc<AtomicBool>,
    /// den, kdy už automatické kopírování proběhlo úspěšně; čas dalšího pokusu po chybě
    archive_done: Mutex<(Option<NaiveDate>, Option<Instant>)>,
}

impl AirLogger {
    pub fn start(data_dir: PathBuf, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, events: EventBus) -> Arc<Self> {
        let me = Arc::new(Self { data_dir, cfg, engine, events, running: Mutex::new(HashMap::new()), pending_library: Mutex::new(vec![]), pending_podcast: Mutex::new(vec![]), archive_busy: Arc::new(AtomicBool::new(false)), archive_done: Mutex::new((None, None)) });
        let m = me.clone();
        std::thread::Builder::new().name("orca-airlog".into()).spawn(move || {
            let mut last_house = Instant::now() - Duration::from_secs(3600);
            loop {
                m.sync();
                if last_house.elapsed() > Duration::from_secs(600) { last_house = Instant::now(); m.housekeeping(); }
                m.shows_tick();
                m.archive_tick();
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

// ---------------------------------------------------------------------------------------------
// Archiv monitoringu – denní kopie na externí / síťový disk

/// Záznam o zkopírování jednoho dne jednoho záznamníku (Logger/archive.json).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ArchiveEntry {
    pub day: String,
    pub logger_id: String,
    pub logger_name: String,
    /// ok | error
    pub status: String,
    pub files: usize,
    pub bytes: u64,
    /// kdy proběhlo (RFC 3339)
    pub at: String,
    /// složka s kopií
    pub target: String,
    pub error: String,
    /// záznamy dne už byly smazány ze systému (zůstávají jen v archivu)
    pub deleted: bool,
    pub deleted_at: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ArchiveStatus {
    pub busy: bool,
    pub target_ok: bool,
    pub target_free: Option<u64>,
    pub next_run: Option<String>,
    pub entries: Vec<ArchiveEntry>,
    /// dny čekající na zkopírování: (logger_id, den)
    pub pending: Vec<(String, String)>,
}

fn archive_state_path(data_dir: &Path) -> PathBuf { data_dir.join("Logger").join("archive.json") }

fn load_archive_state(data_dir: &Path) -> Vec<ArchiveEntry> {
    std::fs::read_to_string(archive_state_path(data_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_archive_state(data_dir: &Path, v: &[ArchiveEntry]) {
    let p = archive_state_path(data_dir);
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(v).unwrap_or_default()).is_ok() { let _ = std::fs::rename(&tmp, &p); }
}

fn upsert_entry(data_dir: &Path, e: ArchiveEntry) {
    let mut v = load_archive_state(data_dir);
    v.retain(|x| !(x.day == e.day && x.logger_id == e.logger_id));
    v.push(e);
    v.sort_by(|a, b| b.day.cmp(&a.day).then(a.logger_name.cmp(&b.logger_name)));
    v.truncate(2000);
    save_archive_state(data_dir, &v);
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.1} GB", b as f64 / 1e9).replace('.', ",") } else { format!("{:.0} MB", b as f64 / 1e6) }
}

fn parse_hhmm(s: &str) -> (u32, u32) {
    let mut it = s.split(':');
    let h = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(2).min(23);
    let m = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(30).min(59);
    (h, m)
}

/// Kopie s ověřením: nejdřív do dočasného souboru, kontrolní součet kopie musí sedět se zdrojem, pak přejmenování.
fn copy_verified(src: &Path, dst: &Path, want: &str) -> Result<()> {
    if dst.is_file() && std::fs::metadata(dst).map(|m| m.len()).ok() == std::fs::metadata(src).map(|m| m.len()).ok() && sha256_file(dst).ok().as_deref() == Some(want) {
        return Ok(()); // už zkopírováno dříve
    }
    let name = dst.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dst.with_file_name(format!(".{name}.part"));
    std::fs::copy(src, &tmp).with_context(|| format!("kopírování {name}"))?;
    if let Ok(f) = std::fs::File::open(&tmp) { let _ = f.sync_all(); }
    let got = sha256_file(&tmp).with_context(|| format!("čtení kopie {name}"))?;
    if got != want { let _ = std::fs::remove_file(&tmp); return Err(anyhow!("kopie souboru {name} nesouhlasí (kontrolní součet)")); }
    let _ = std::fs::remove_file(dst);
    std::fs::rename(&tmp, dst).with_context(|| format!("uložení {name}"))?;
    Ok(())
}

impl AirLogger {
    fn archive_loggers(&self) -> (ArchiveConfig, Vec<LoggerDef>) {
        let c = self.cfg.read().airlog.clone();
        let l = c.loggers.into_iter().filter(|l| c.archive.loggers.is_empty() || c.archive.loggers.contains(&l.id)).collect();
        (c.archive, l)
    }

    /// Dny (před dneškem), které ještě nejsou úspěšně zkopírované.
    fn archive_pending(&self) -> Vec<(LoggerDef, NaiveDate)> {
        let (a, loggers) = self.archive_loggers();
        let state = load_archive_state(&self.data_dir);
        let today = Local::now().date_naive();
        let from = today - chrono::Duration::days(a.catch_up_days.clamp(1, 60) as i64);
        let mut out = vec![];
        for l in loggers {
            let dir = logger_dir(&self.data_dir, &l);
            let days: BTreeSet<String> = list_files(&dir, ext_of(&l.format)).into_iter().map(|f| f.0[..10].to_string()).collect();
            for d in days {
                let Ok(nd) = NaiveDate::parse_from_str(&d, "%Y-%m-%d") else { continue };
                if nd >= today || nd < from { continue; }
                if state.iter().any(|e| e.day == d && e.logger_id == l.id && e.status == "ok") { continue; }
                out.push((l.clone(), nd));
            }
        }
        out
    }

    pub fn archive_status(&self) -> ArchiveStatus {
        let (a, _) = self.archive_loggers();
        let target = PathBuf::from(a.dir.trim());
        let target_ok = !a.dir.trim().is_empty() && target.is_dir();
        let next_run = if a.enabled {
            let (h, m) = parse_hhmm(&a.time);
            let now = Local::now();
            let done = self.archive_done.lock().0 == Some(now.date_naive());
            let mut d = now.date_naive();
            if done { d = d.succ_opt().unwrap_or(d); }
            Local.from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap()).earliest().map(|x| x.to_rfc3339())
        } else { None };
        ArchiveStatus {
            busy: self.archive_busy.load(Ordering::SeqCst),
            target_ok,
            target_free: if target_ok { fs4::available_space(&target).ok() } else { None },
            next_run,
            entries: load_archive_state(&self.data_dir),
            pending: self.archive_pending().into_iter().map(|(l, d)| (l.id, d.format("%Y-%m-%d").to_string())).collect(),
        }
    }

    /// Každé 2 s ze smyčky monitoringu: v nastavený čas spustí kopírování (jednou denně, po chybě znovu za hodinu).
    fn archive_tick(self: &Arc<Self>) {
        let (a, _) = self.archive_loggers();
        if !a.enabled || a.dir.trim().is_empty() || self.archive_busy.load(Ordering::SeqCst) { return; }
        let now = Local::now();
        let (h, m) = parse_hhmm(&a.time);
        if now.naive_local().time() < chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap() { return; }
        {
            let st = self.archive_done.lock();
            if st.0 == Some(now.date_naive()) { return; }
            if let Some(t) = st.1 { if Instant::now() < t { return; } }
        }
        let me = self.clone();
        self.archive_busy.store(true, Ordering::SeqCst);
        std::thread::Builder::new().name("orca-airlog-archive".into()).spawn(move || {
            let ok = me.archive_run(None, true);
            let mut st = me.archive_done.lock();
            if ok { *st = (Some(Local::now().date_naive()), None); } else { st.1 = Some(Instant::now() + Duration::from_secs(3600)); }
            me.archive_busy.store(false, Ordering::SeqCst);
        }).map_err(|_| self.archive_busy.store(false, Ordering::SeqCst)).ok();
    }

    /// Ruční spuštění („Kopírovat teď“): den RRRR-MM-DD nebo vše čekající.
    pub fn archive_now(self: &Arc<Self>, day: Option<String>) -> Result<()> {
        let (a, _) = self.archive_loggers();
        if a.dir.trim().is_empty() { return Err(anyhow!("nejdřív vyberte cílovou složku archivu")); }
        let day = match day { Some(d) if !d.is_empty() => Some(NaiveDate::parse_from_str(&d, "%Y-%m-%d").map_err(|_| anyhow!("datum ve tvaru RRRR-MM-DD"))?), _ => None };
        if self.archive_busy.swap(true, Ordering::SeqCst) { return Err(anyhow!("kopírování už běží")); }
        let me = self.clone();
        std::thread::Builder::new().name("orca-airlog-archive".into()).spawn(move || {
            me.archive_run(day, false);
            me.archive_busy.store(false, Ordering::SeqCst);
        }).map_err(|e| { self.archive_busy.store(false, Ordering::SeqCst); anyhow!(e) })?;
        Ok(())
    }

    /// Zkopíruje čekající dny (nebo jeden zvolený den znovu). Vrací true, když vše proběhlo bez chyby.
    fn archive_run(&self, only: Option<NaiveDate>, scheduled: bool) -> bool {
        let (a, loggers) = self.archive_loggers();
        let jobs: Vec<(LoggerDef, NaiveDate)> = match only {
            Some(d) => loggers.into_iter().map(|l| (l, d)).collect(),
            None => self.archive_pending(),
        };
        if jobs.is_empty() {
            if !scheduled { self.events.alert("info", "airlog_archive_ok", "Archiv monitoringu: není co kopírovat – všechny dny už jsou zkopírované."); }
            return true;
        }
        let root = PathBuf::from(a.dir.trim());
        let mut oks: Vec<String> = vec![];
        let mut errs: Vec<String> = vec![];
        let (mut files_all, mut bytes_all) = (0usize, 0u64);
        for (l, d) in jobs {
            let day = d.format("%Y-%m-%d").to_string();
            let target = root.join(crate::library::sanitize(&l.name)).join(&day);
            let mut e = ArchiveEntry { day: day.clone(), logger_id: l.id.clone(), logger_name: l.name.clone(), at: Local::now().to_rfc3339(), target: target.to_string_lossy().to_string(), ..Default::default() };
            match self.archive_day(&l, d, &root, &target) {
                Ok((n, b)) if n > 0 => {
                    e.status = "ok".into(); e.files = n; e.bytes = b;
                    files_all += n; bytes_all += b;
                    oks.push(format!("{} {}", l.name, d.format("%-d. %-m.")));
                    if a.delete_after {
                        let dir = logger_dir(&self.data_dir, &l);
                        if kept(&dir).contains(&day) { log::info!("archiv: den {day} je zamčený – v systému zůstává"); }
                        else if self.delete_local_day(&l, &day).is_ok() { e.deleted = true; e.deleted_at = Local::now().to_rfc3339(); }
                    }
                }
                Ok(_) => continue, // den bez záznamů
                Err(err) => { e.status = "error".into(); e.error = format!("{err:#}"); errs.push(format!("{} {}: {err:#}", l.name, d.format("%-d. %-m."))); }
            }
            upsert_entry(&self.data_dir, e);
        }
        if !oks.is_empty() {
            self.events.alert("info", "airlog_archive_ok", format!("Monitoring zkopírován na archivní disk a ověřen: {} ({} {}, {}) → {}", oks.join(", "), files_all, match files_all { 1 => "soubor", 2..=4 => "soubory", _ => "souborů" }, fmt_bytes(bytes_all), root.display()));
        }
        if !errs.is_empty() {
            self.events.alert("error", "airlog_archive_failed", format!("Kopírování monitoringu na archivní disk se nezdařilo – {}", errs.join("; ")));
        }
        errs.is_empty()
    }

    /// Soubory jednoho dne (název, velikost), bez právě nahrávaného souboru.
    fn day_files(&self, l: &LoggerDef, day: &str) -> Vec<(String, u64)> {
        let dir = logger_dir(&self.data_dir, l);
        let mut files = list_files(&dir, ext_of(&l.format));
        let recording = self.running.lock().get(&l.id).map(|r| r.status.lock().state == "recording").unwrap_or(false);
        if recording { files.pop(); }
        files.into_iter().filter(|f| f.0.starts_with(day)).map(|f| (f.0, f.2)).collect()
    }

    fn archive_day(&self, l: &LoggerDef, d: NaiveDate, root: &Path, target: &Path) -> Result<(usize, u64)> {
        let day = d.format("%Y-%m-%d").to_string();
        let files = self.day_files(l, &day);
        if files.is_empty() { return Ok((0, 0)); }
        if !root.is_dir() { return Err(anyhow!("cílová složka {} není dostupná (disk odpojený, síť nedostupná?)", root.display())); }
        let need: u64 = files.iter().map(|f| f.1).sum();
        if let Ok(free) = fs4::available_space(root) { if free < need + 50_000_000 { return Err(anyhow!("na archivním disku není dost místa (potřeba {}, volno {})", fmt_bytes(need), fmt_bytes(free))); } }
        std::fs::create_dir_all(target).with_context(|| format!("nelze vytvořit složku {}", target.display()))?;
        let dir = logger_dir(&self.data_dir, l);
        let mut sums = String::new();
        let mut bytes = 0u64;
        for (name, size) in &files {
            let src = dir.join(name);
            let hp = dir.join(format!("{name}.sha256"));
            let want = match std::fs::read_to_string(&hp).ok().and_then(|t| t.split_whitespace().next().map(str::to_string)) {
                Some(h) => { if sha256_file(&src)? != h { return Err(anyhow!("záznam {name} v systému je poškozený (nesouhlasí kontrolní součet) – nekopíruji")); } h }
                None => sha256_file(&src)?,
            };
            copy_verified(&src, &target.join(name), &want)?;
            std::fs::write(target.join(format!("{name}.sha256")), format!("{want}  {name}\n"))?;
            sums.push_str(&format!("{want}  {name}\n"));
            bytes += size;
        }
        let marks = dir.join(format!("marks-{day}.jsonl"));
        if marks.is_file() { std::fs::copy(&marks, target.join(format!("marks-{day}.jsonl"))).context("kopírování značek")?; }
        std::fs::write(target.join("SHA256SUMS"), sums).context("zápis SHA256SUMS")?;
        Ok((files.len(), bytes))
    }

    fn delete_local_day(&self, l: &LoggerDef, day: &str) -> Result<usize> {
        let dir = logger_dir(&self.data_dir, l);
        let mut n = 0;
        for (name, _) in self.day_files(l, day) { if remove_seg(&dir, &name) { n += 1; } }
        let _ = std::fs::remove_file(dir.join(format!("marks-{day}.jsonl")));
        Ok(n)
    }

    /// Smazání dne ze systému po kontrole – jen když je ověřeně v archivu (kopie se znovu zkontroluje).
    pub fn archive_delete_local(&self, logger_id: &str, day: &str) -> Result<usize> {
        let l = self.def(logger_id)?;
        NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| anyhow!("datum ve tvaru RRRR-MM-DD"))?;
        let mut state = load_archive_state(&self.data_dir);
        let Some(e) = state.iter_mut().find(|e| e.day == day && e.logger_id == logger_id && e.status == "ok") else { return Err(anyhow!("den {day} není úspěšně zkopírovaný v archivu – nemažu")); };
        let dir = logger_dir(&self.data_dir, &l);
        if kept(&dir).contains(day) { return Err(anyhow!("den {day} je zamčený (spor, reklamace) – nejdřív ho odemkněte")); }
        let target = PathBuf::from(&e.target);
        if !target.is_dir() { return Err(anyhow!("archivní kopie {} teď není dostupná (disk odpojený?) – nemažu", target.display())); }
        let files = self.day_files(&l, day);
        for (name, size) in &files {
            let dst = target.join(name);
            let ds = std::fs::metadata(&dst).map(|m| m.len()).map_err(|_| anyhow!("v archivu chybí {name} – nemažu"))?;
            if ds != *size { return Err(anyhow!("v archivu nesouhlasí velikost {name} – nemažu")); }
            let want = std::fs::read_to_string(dir.join(format!("{name}.sha256"))).ok().and_then(|t| t.split_whitespace().next().map(str::to_string)).map(Ok).unwrap_or_else(|| sha256_file(&dir.join(name)))?;
            if sha256_file(&dst)? != want { return Err(anyhow!("archivní kopie {name} nesouhlasí (kontrolní součet) – nemažu")); }
        }
        let n = self.delete_local_day(&l, day)?;
        e.deleted = true;
        e.deleted_at = Local::now().to_rfc3339();
        save_archive_state(&self.data_dir, &state);
        log::info!("archiv: {} {day} smazán ze systému ({n} souborů), kopie v {}", l.name, target.display());
        Ok(n)
    }
}
