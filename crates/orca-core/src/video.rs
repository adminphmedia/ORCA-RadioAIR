//! Video → zvuk pro vysílání: videa vložená do složky Video se (ručně nebo automaticky) převedou na zvukový
//! soubor připravený pro vysílání a uloží do knihovny nebo do zvolené složky. Videa lze po převodu mazat.
//!
//! Úprava souboru je jen „příprava“ (oříznutí ticha, sjednocení hlasitosti na cílové LUFS, volitelně úprava řeči).
//! Vysílací processing (komprese, limiter, zvuk stanice) se NEaplikuje do souboru – běží až na výstupu ORCA,
//! jinak by se zvuk zpracoval dvakrát.

use crate::config::Config;
use crate::model::TrackKind;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

pub const VIDEO_EXT: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mts", "m2ts", "ts", "mpg", "mpeg", "3gp"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// Složka pro videa; prázdné = <data>/Video
    pub folder: PathBuf,
    /// Převádět nová videa automaticky
    pub auto_export: bool,
    /// Kam uložit zvuk: "library" (knihovna – typ + kategorie) | "folder" (vlastní složka)
    pub target: String,
    pub kind: TrackKind,
    pub category: String,
    pub out_dir: PathBuf,
    /// flac | wav | mp3
    pub format: String,
    pub trim_silence: bool,
    pub normalize: bool,
    pub lufs: f32,
    /// Úprava řeči: odstranění hukotu (high-pass) a jemná komprese – pro rozhovory, reportáže
    pub speech: bool,
    /// keep | after_export | after_days
    pub delete_video: String,
    pub delete_days: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self { folder: PathBuf::new(), auto_export: false, target: "library".into(), kind: TrackKind::VoiceTrack, category: "Video".into(), out_dir: PathBuf::new(),
            format: "flac".into(), trim_silence: true, normalize: true, lufs: -16.0, speech: false, delete_video: "keep".into(), delete_days: 7 }
    }
}

pub fn video_dir(cfg: &Config, data_dir: &Path) -> PathBuf {
    if cfg.video.folder.as_os_str().is_empty() { data_dir.join("Video") } else { cfg.video.folder.clone() }
}

pub fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).map(|e| VIDEO_EXT.contains(&e.to_lowercase().as_str())).unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Entry {
    /// new | queued | exporting | done | error
    pub status: String,
    pub output: String,
    pub track_id: Option<i64>,
    pub exported_at: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoItem {
    pub name: String,
    pub size: u64,
    pub modified: Option<String>,
    pub status: String,
    pub output: String,
    pub track_id: Option<i64>,
    pub exported_at: Option<String>,
    pub error: String,
    pub progress: Option<u8>,
}

pub struct VideoManager {
    cfg: Arc<RwLock<Config>>,
    data_dir: PathBuf,
    library: Arc<crate::library::Library>,
    events: crate::events::EventBus,
    state: Mutex<BTreeMap<String, Entry>>,
    queue: Mutex<VecDeque<String>>,
    progress: Mutex<HashMap<String, u8>>,
    sizes: Mutex<HashMap<String, (u64, u32)>>,
}

impl VideoManager {
    pub fn start(cfg: Arc<RwLock<Config>>, data_dir: PathBuf, library: Arc<crate::library::Library>, events: crate::events::EventBus) -> Arc<Self> {
        let me = Arc::new(Self { cfg, data_dir, library, events, state: Mutex::new(BTreeMap::new()), queue: Mutex::new(VecDeque::new()), progress: Mutex::new(HashMap::new()), sizes: Mutex::new(HashMap::new()) });
        me.load();
        {
            // přerušený převod z minula → zpět do fronty / jako nový
            let mut st = me.state.lock();
            for e in st.values_mut() { if e.status == "exporting" || e.status == "queued" { e.status = "new".into(); } }
        }
        let m = me.clone();
        std::thread::Builder::new().name("orca-video".into()).spawn(move || loop {
            m.scan();
            let next = m.queue.lock().pop_front();
            if let Some(n) = next { m.export_one(&n); continue; }
            std::thread::sleep(Duration::from_secs(5));
        }).ok();
        me
    }

    pub fn dir(&self) -> PathBuf { video_dir(&self.cfg.read(), &self.data_dir) }
    fn state_path(&self) -> PathBuf { self.dir().join(".orca-video.json") }
    fn load(&self) {
        if let Ok(s) = std::fs::read_to_string(self.state_path()) { if let Ok(m) = serde_json::from_str(&s) { *self.state.lock() = m; } }
    }
    fn save(&self) {
        let s = serde_json::to_string_pretty(&*self.state.lock()).unwrap_or_default();
        let _ = std::fs::create_dir_all(self.dir());
        let _ = std::fs::write(self.state_path(), s);
    }

    fn files(&self) -> Vec<(String, std::fs::Metadata)> {
        let dir = self.dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut v: Vec<_> = std::fs::read_dir(&dir).map(|rd| rd.flatten().filter_map(|e| {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !p.is_file() || !is_video(&p) { return None; }
            e.metadata().ok().map(|m| (name, m))
        }).collect()).unwrap_or_default();
        v.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        v
    }

    /// Najde nová videa (stabilní velikost = dokopírovaná), automatický převod a mazání po N dnech.
    fn scan(&self) {
        let c = self.cfg.read().video.clone();
        let files = self.files();
        let mut changed = false;
        let mut sizes = self.sizes.lock();
        let mut st = self.state.lock();
        for (name, m) in &files {
            let len = m.len();
            let stable = match sizes.get(name) { Some((l, n)) if *l == len => { let n = *n + 1; sizes.insert(name.clone(), (len, n)); n >= 2 } _ => { sizes.insert(name.clone(), (len, 0)); false } };
            if !st.contains_key(name) && stable {
                st.insert(name.clone(), Entry { status: "new".into(), ..Default::default() });
                changed = true;
                if c.auto_export { self.queue.lock().push_back(name.clone()); st.get_mut(name).unwrap().status = "queued".into(); }
            }
        }
        // odstraněná videa zmizí ze seznamu
        let names: std::collections::HashSet<&String> = files.iter().map(|(n, _)| n).collect();
        let before = st.len();
        st.retain(|k, _| names.contains(k));
        changed |= st.len() != before;
        sizes.retain(|k, _| names.contains(k));
        // mazání videí N dní po převodu
        if c.delete_video == "after_days" {
            let limit = chrono::Local::now() - chrono::Duration::days(c.delete_days as i64);
            let old: Vec<String> = st.iter().filter(|(_, e)| e.status == "done" && e.exported_at.as_deref().and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()).map(|d| d < limit).unwrap_or(false)).map(|(k, _)| k.clone()).collect();
            for n in old { if std::fs::remove_file(self.dir().join(&n)).is_ok() { st.remove(&n); changed = true; } }
        }
        drop(st); drop(sizes);
        if changed { self.save(); }
    }

    pub fn list(&self) -> Vec<VideoItem> {
        let st = self.state.lock().clone();
        let pr = self.progress.lock().clone();
        self.files().into_iter().map(|(name, m)| {
            let e = st.get(&name).cloned().unwrap_or(Entry { status: "new".into(), ..Default::default() });
            VideoItem {
                size: m.len(), modified: m.modified().ok().map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339()),
                status: e.status, output: e.output, track_id: e.track_id, exported_at: e.exported_at, error: e.error, progress: pr.get(&name).copied(), name,
            }
        }).collect()
    }

    /// Zařadí videa k převodu (prázdné = všechna nová / s chybou).
    pub fn enqueue(&self, names: &[String]) -> usize {
        let mut st = self.state.lock();
        let all = self.files();
        let pick: Vec<String> = if names.is_empty() {
            all.iter().map(|(n, _)| n.clone()).filter(|n| st.get(n).map(|e| e.status == "new" || e.status == "error").unwrap_or(true)).collect()
        } else { names.iter().filter(|n| all.iter().any(|(x, _)| x == *n)).cloned().collect() };
        let mut q = self.queue.lock();
        let mut n = 0;
        for name in pick {
            if q.contains(&name) || st.get(&name).map(|e| e.status == "exporting").unwrap_or(false) { continue; }
            st.entry(name.clone()).or_default().status = "queued".into();
            q.push_back(name); n += 1;
        }
        drop(q); drop(st);
        self.save();
        n
    }

    /// Smaže videa (soubory). `exported_only` = jen ta, která už jsou převedená.
    pub fn delete(&self, names: &[String], exported_only: bool) -> (usize, u64) {
        let mut st = self.state.lock();
        let mut n = 0; let mut bytes = 0;
        for (name, m) in self.files() {
            let sel = if names.is_empty() { exported_only } else { names.contains(&name) };
            if !sel { continue; }
            let e = st.get(&name);
            if e.map(|e| e.status == "exporting" || e.status == "queued").unwrap_or(false) { continue; }
            if exported_only && e.map(|e| e.status != "done").unwrap_or(true) { continue; }
            if std::fs::remove_file(self.dir().join(&name)).is_ok() { n += 1; bytes += m.len(); st.remove(&name); }
        }
        drop(st);
        self.save();
        (n, bytes)
    }

    fn set(&self, name: &str, f: impl FnOnce(&mut Entry)) { { let mut st = self.state.lock(); f(st.entry(name.to_string()).or_default()); } self.save(); }

    fn export_one(&self, name: &str) {
        let src = self.dir().join(name);
        if !src.is_file() { self.state.lock().remove(name); self.save(); return; }
        self.set(name, |e| { e.status = "exporting".into(); e.error.clear(); });
        self.progress.lock().insert(name.to_string(), 0);
        let r = self.convert(name, &src);
        self.progress.lock().remove(name);
        match r {
            Ok((out, track)) => {
                let del = self.cfg.read().video.delete_video == "after_export";
                self.set(name, |e| { e.status = "done".into(); e.output = out.clone(); e.track_id = track; e.exported_at = Some(chrono::Local::now().to_rfc3339()); });
                self.events.alert("info", "video_exported", format!("Video převedeno na zvuk: {name} → {out}"));
                if del && std::fs::remove_file(&src).is_ok() { self.state.lock().remove(name); self.save(); }
            }
            Err(e) => {
                let msg = e.to_string();
                self.set(name, |x| { x.status = "error".into(); x.error = msg.clone(); });
                self.events.alert("warning", "video_export_failed", format!("Převod videa {name} se nepovedl: {msg}"));
            }
        }
    }

    fn convert(&self, name: &str, src: &Path) -> anyhow::Result<(String, Option<i64>)> {
        let cfg = self.cfg.read().clone();
        let c = cfg.video.clone();
        let ffmpeg = crate::encoder::resolve_ffmpeg(&cfg);
        let dur = probe_duration(&ffmpeg, src);
        let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "video".into());
        let ext = match c.format.as_str() { "wav" => "wav", "mp3" => "mp3", _ => "flac" };
        let fname = crate::library::sanitize(&format!("{stem}.{ext}"));
        let dir = if c.target == "folder" && !c.out_dir.as_os_str().is_empty() { c.out_dir.clone() } else { self.library.target_dir(c.kind, &c.category) };
        std::fs::create_dir_all(&dir)?;
        let policy = cfg.library.on_duplicate.clone();
        let dest = crate::library::unique_path(&dir, &fname, if policy == "skip" { "rename" } else { &policy }).ok_or_else(|| anyhow::anyhow!("cílový soubor už existuje"))?;
        let tmp = dir.join(format!(".orca-video-{}.{ext}", uuid::Uuid::new_v4().simple()));
        let args = ffmpeg_args(&c, src, &tmp, &stem, dur.unwrap_or(0.0));
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd.spawn()
            .map_err(|e| anyhow::anyhow!("ffmpeg nelze spustit ({}): {e}", ffmpeg.display()))?;
        let err_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        if let Some(se) = child.stderr.take() {
            let t = err_tail.clone();
            std::thread::spawn(move || for l in std::io::BufReader::new(se).lines().map_while(Result::ok) { let mut q = t.lock(); q.push_back(l); if q.len() > 6 { q.pop_front(); } });
        }
        if let Some(so) = child.stdout.take() {
            for l in std::io::BufReader::new(so).lines().map_while(Result::ok) {
                if let (Some(v), Some(d)) = (l.strip_prefix("out_time_us="), dur) {
                    if let Ok(us) = v.trim().parse::<f64>() { if d > 0.0 { self.progress.lock().insert(name.to_string(), ((us / 1e6 / d) * 100.0).clamp(0.0, 99.0) as u8); } }
                }
            }
        }
        let ok = child.wait()?.success();
        if !ok || !tmp.is_file() {
            let _ = std::fs::remove_file(&tmp);
            let d = err_tail.lock().iter().cloned().collect::<Vec<_>>().join(" | ");
            anyhow::bail!(if d.contains("does not contain any stream") || d.contains("Output file #0 does not contain") { "video nemá zvukovou stopu".to_string() } else { format!("ffmpeg: {d}") });
        }
        std::fs::rename(&tmp, &dest)?;
        let out = dest.to_string_lossy().to_string();
        let mut track = None;
        if c.target != "folder" || self.library.folder_for_public(&dest).is_some() {
            if let Some(t) = self.library.register_file(&dest, &c.category)? {
                track = Some(t.id);
                self.events.emit(crate::events::Event::LibraryChanged { added: 1, updated: 0, missing: 0 });
            }
        }
        Ok((out, track))
    }
}

/// Windows: ffmpeg bez okna konzole.
fn no_window(_cmd: &mut Command) {
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; _cmd.creation_flags(0x0800_0000); }
}

/// Délka videa (s) z výpisu ffmpeg -i.
fn probe_duration(ffmpeg: &Path, src: &Path) -> Option<f64> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-i"]).arg(src).stdin(Stdio::null());
    no_window(&mut cmd);
    let out = cmd.output().ok()?;
    let s = String::from_utf8_lossy(&out.stderr);
    let i = s.find("Duration: ")?;
    let t = s[i + 10..].split(',').next()?.trim();
    let p: Vec<f64> = t.split(':').filter_map(|x| x.parse().ok()).collect();
    (p.len() == 3).then(|| p[0] * 3600.0 + p[1] * 60.0 + p[2])
}

pub fn filter_chain(c: &VideoConfig, duration_s: f64) -> String {
    let mut f: Vec<String> = vec![];
    if c.speech { f.push("highpass=f=80".into()); f.push("acompressor=threshold=-20dB:ratio=3:attack=10:release=250:makeup=2".into()); }
    if c.trim_silence {
        let s = "silenceremove=start_periods=1:start_duration=0.05:start_threshold=-50dB";
        f.push(s.into());
        // ticho na konci: přes otočení zvuku – jen u kratších videí (celý zvuk se drží v paměti)
        if duration_s > 0.0 && duration_s <= 1200.0 { f.push("areverse".into()); f.push(s.into()); f.push("areverse".into()); }
    }
    if c.normalize { f.push(format!("loudnorm=I={:.1}:TP=-1.5:LRA=11", c.lufs.clamp(-30.0, -8.0))); }
    f.join(",")
}

pub fn ffmpeg_args(c: &VideoConfig, src: &Path, dst: &Path, title: &str, duration_s: f64) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:1", "-nostats", "-i"].iter().map(|s| s.to_string()).collect();
    a.push(src.to_string_lossy().to_string());
    a.extend(["-vn", "-sn", "-dn", "-map", "0:a:0"].iter().map(|s| s.to_string()));
    let chain = filter_chain(c, duration_s);
    if !chain.is_empty() { a.push("-af".into()); a.push(chain); }
    a.extend(["-ar", "48000", "-ac", "2"].iter().map(|s| s.to_string()));
    match c.format.as_str() {
        "wav" => a.extend(["-c:a", "pcm_s16le"].iter().map(|s| s.to_string())),
        "mp3" => a.extend(["-c:a", "libmp3lame", "-b:a", "320k"].iter().map(|s| s.to_string())),
        _ => a.extend(["-c:a", "flac", "-sample_fmt", "s32", "-bits_per_raw_sample", "24"].iter().map(|s| s.to_string())),
    }
    a.push("-metadata".into()); a.push(format!("title={title}"));
    a.push(dst.to_string_lossy().to_string());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn args() {
        let c = VideoConfig::default();
        let a = ffmpeg_args(&c, Path::new("/v/a.mp4"), Path::new("/o/a.flac"), "a", 60.0);
        assert!(a.contains(&"-vn".to_string()));
        let ch = filter_chain(&c, 60.0);
        assert!(ch.contains("loudnorm=I=-16.0") && ch.contains("areverse"));
        assert!(!filter_chain(&c, 3600.0).contains("areverse"));
        assert!(is_video(Path::new("x.MOV")) && !is_video(Path::new("x.mp3")));
    }
}
