//! Knihovna: sledované složky, automatický import, čtení tagů a fronta analýzy.
//!
//! Princip: soubory se nemusí nikam importovat. Stačí je nakopírovat do sledované složky
//! (např. `Songs/Pop/…`) a ORCA je sama přidá, aktualizuje nebo označí jako chybějící.

use crate::analysis::analyze_file;
use crate::config::{Config, WatchFolder};
use crate::db::{Db, ScannedFile, UpsertResult};
use crate::decode::is_supported;
use crate::events::{Event, EventBus};
use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use lofty::prelude::*;
use lofty::picture::PictureType;
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, FileIdMap};
use parking_lot::{Mutex, RwLock};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RESCAN_EVERY: Duration = Duration::from_secs(600);

pub struct Library {
    pub data_dir: PathBuf,
    db: Arc<Db>,
    cfg: Arc<RwLock<Config>>,
    events: EventBus,
    analysis_tx: Sender<i64>,
    fs_tx: Sender<LibMsg>,
    watcher: Mutex<Option<Debouncer<RecommendedWatcher, FileIdMap>>>,
}

enum LibMsg { Paths(Vec<PathBuf>), Rescan }

#[derive(Default, Debug, Clone, Copy)]
pub struct ScanStats { pub added: usize, pub updated: usize, pub missing: usize }

fn file_stamp(p: &Path) -> Option<(i64, i64)> {
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() { return None; }
    let mtime = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((mtime, md.len() as i64))
}

pub fn path_str(p: &Path) -> String { p.to_string_lossy().to_string() }

/// Načte tagy a metadata souboru.
pub fn read_file(path: &Path, wf: &WatchFolder) -> Result<ScannedFile> {
    let (mtime, size) = file_stamp(path).ok_or_else(|| anyhow::anyhow!("soubor nedostupný"))?;
    let filename = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let category = category_for(path, wf);
    let mut f = ScannedFile { kind: wf.kind, path: path_str(path), filename, category, file_mtime: mtime, file_size: size, ..Default::default() };
    match lofty::read_from_path(path) {
        Ok(tf) => {
            f.duration_ms = tf.properties().duration().as_millis() as i64;
            if let Some(tag) = tf.primary_tag().or_else(|| tf.first_tag()) {
                f.artist = tag.artist().map(|s| s.trim().to_string()).unwrap_or_default();
                f.title = tag.title().map(|s| s.trim().to_string()).unwrap_or_default();
                f.album = tag.album().map(|s| s.trim().to_string()).unwrap_or_default();
                f.genre = tag.genre().map(|s| s.trim().to_string()).unwrap_or_default();
                f.year = tag.year().map(|y| y as i32);
                f.isrc = tag.get_string(&ItemKey::Isrc).or_else(|| tag.get_string(&ItemKey::Unknown("ISRC".into()))).unwrap_or_default().to_string();
                f.composer = tag.get_string(&ItemKey::Composer).unwrap_or_default().to_string();
                f.publisher = tag.get_string(&ItemKey::Publisher).or_else(|| tag.get_string(&ItemKey::Label)).unwrap_or_default().to_string();
                f.lyricist = tag.get_string(&ItemKey::Lyricist).unwrap_or_default().to_string();
                f.label = tag.get_string(&ItemKey::Label).or_else(|| tag.get_string(&ItemKey::Publisher)).unwrap_or_default().to_string();
            }
        }
        Err(e) => log::warn!("tagy nelze přečíst {}: {e}", path.display()),
    }
    // Záloha: "Interpret - Název.mp3"
    if f.title.is_empty() {
        if let Some((a, t)) = stem.split_once(" - ") {
            if f.artist.is_empty() { f.artist = a.trim().to_string(); }
            f.title = t.trim().to_string();
        } else {
            f.title = stem.clone();
        }
    }
    Ok(f)
}

/// Obal alba (front cover) z tagu.
pub fn read_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let tf = lofty::read_from_path(path).ok()?;
    for tag in tf.tags() {
        let pics = tag.pictures();
        let pic = pics.iter().find(|p| p.pic_type() == PictureType::CoverFront).or_else(|| pics.first());
        if let Some(p) = pic {
            let mime = p.mime_type().map(|m| m.as_str().to_string()).unwrap_or_else(|| "image/jpeg".into());
            return Some((p.data().to_vec(), mime));
        }
    }
    None
}

pub const TRASH_DIR: &str = "_Trash";

pub fn in_trash(p: &Path) -> bool { p.components().any(|c| c.as_os_str() == TRASH_DIR) }

/// Kořenová složka knihovny.
pub fn library_root(cfg: &Config, data_dir: &Path) -> PathBuf {
    if cfg.library.root.as_os_str().is_empty() { data_dir.join("Library") } else { cfg.library.root.clone() }
}

/// Vytvoří podsložky Songs, Spots, Jingles … a zaregistruje je jako sledované. Vrací true, pokud se změnila konfigurace.
pub fn ensure_kind_folders(cfg: &mut Config, data_dir: &Path) -> bool {
    if !cfg.library.auto_folders { return false; }
    let root = library_root(cfg, data_dir);
    let mut changed = false;
    for kind in TrackKind::ALL {
        let dir = root.join(kind.folder_name());
        if let Err(e) = std::fs::create_dir_all(&dir) { log::warn!("{}: {e}", dir.display()); continue; }
        if !cfg.watch_folders.iter().any(|w| w.path == dir) {
            cfg.watch_folders.push(WatchFolder {
                id: format!("lib-{}", kind.as_str()), path: dir, kind, category: String::new(), category_from_subfolder: true, enabled: true,
            });
            changed = true;
        }
    }
    let _ = std::fs::create_dir_all(root.join(TRASH_DIR));
    changed
}

/// Bezpečný název souboru/složky (bez lomítek a zakázaných znaků Windows).
pub fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "soubor".into() } else { s }
}

/// Volná cesta v cílové složce podle politiky duplicit (rename | replace | skip).
pub fn unique_path(dir: &Path, filename: &str, policy: &str) -> Option<PathBuf> {
    let p = dir.join(filename);
    if !p.exists() || policy == "replace" { return Some(p); }
    if policy == "skip" { return None; }
    let stem = Path::new(filename).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = Path::new(filename).extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
    (2..10_000).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|c| !c.exists())
}

use crate::model::{Track, TrackKind};

#[derive(Debug, Default, serde::Serialize)]
pub struct ImportReport {
    pub added: Vec<Track>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

fn category_for(path: &Path, wf: &WatchFolder) -> String {
    if wf.category_from_subfolder {
        if let Ok(rel) = path.strip_prefix(&wf.path) {
            let comps: Vec<_> = rel.components().collect();
            if comps.len() > 1 {
                return comps[0].as_os_str().to_string_lossy().to_string();
            }
        }
    }
    wf.category.clone()
}

impl Library {
    pub fn new(db: Arc<Db>, cfg: Arc<RwLock<Config>>, events: EventBus, data_dir: PathBuf) -> Arc<Self> {
        let (atx, arx) = unbounded();
        let (ftx, frx) = unbounded();
        let lib = Arc::new(Self { data_dir, db, cfg, events, analysis_tx: atx, fs_tx: ftx, watcher: Mutex::new(None) });
        {
            let l = lib.clone();
            std::thread::Builder::new().name("orca-analysis".into()).spawn(move || l.analysis_worker(arx)).unwrap();
        }
        {
            let l = lib.clone();
            std::thread::Builder::new().name("orca-library".into()).spawn(move || l.fs_worker(frx)).unwrap();
        }
        lib
    }

    fn folder_for(&self, path: &Path) -> Option<WatchFolder> {
        let cfg = self.cfg.read();
        cfg.watch_folders.iter().filter(|w| w.enabled && path.starts_with(&w.path))
            .max_by_key(|w| w.path.as_os_str().len()).cloned()
    }

    /// Zpracuje jeden soubor (nový/změněný). Vrací (added, updated).
    pub fn index_file(&self, path: &Path) -> Result<(usize, usize)> {
        let Some(wf) = self.folder_for(path) else { return Ok((0, 0)) };
        if !is_supported(path) || in_trash(path) { return Ok((0, 0)); }
        // rychlá kontrola beze čtení tagů
        if let (Some((mtime, size)), Some((_, m0, s0, missing))) = (file_stamp(path), self.db.track_by_path(&path_str(path))?) {
            if m0 == mtime && s0 == size && !missing { return Ok((0, 0)); }
        }
        let f = read_file(path, &wf)?;
        Ok(match self.db.upsert_scanned(&f)? {
            UpsertResult::Inserted(id) => { let _ = self.analysis_tx.send(id); (1, 0) }
            UpsertResult::Updated(id) => { let _ = self.analysis_tx.send(id); (0, 1) }
            UpsertResult::Unchanged(_) => (0, 0),
        })
    }

    /// Projde všechny sledované složky.
    pub fn full_scan(&self) -> ScanStats {
        let folders: Vec<WatchFolder> = self.cfg.read().watch_folders.iter().filter(|w| w.enabled).cloned().collect();
        let mut st = ScanStats::default();
        let started = Instant::now();
        for wf in folders {
            if !wf.path.is_dir() {
                self.events.alert("warning", "watch_folder_unavailable", format!("Složka není dostupná: {}", wf.path.display()));
                continue; // nedostupný disk/NAS – nic neoznačujeme jako chybějící
            }
            let mut seen = HashSet::new();
            for entry in walkdir::WalkDir::new(&wf.path).follow_links(true).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if !entry.file_type().is_file() || !is_supported(p) || in_trash(p) { continue; }
                seen.insert(path_str(p));
                match self.index_file(p) {
                    Ok((a, u)) => { st.added += a; st.updated += u; }
                    Err(e) => log::warn!("{}: {e}", p.display()),
                }
            }
            if let Ok(known) = self.db.paths_under(&path_str(&wf.path)) {
                for (id, p) in known {
                    if !seen.contains(&p) && !Path::new(&p).exists() {
                        let _ = self.db.set_missing(id, true);
                        st.missing += 1;
                    }
                }
            }
        }
        log::info!("sken knihovny: +{} ~{} -{} za {:?}", st.added, st.updated, st.missing, started.elapsed());
        if st.added + st.updated + st.missing > 0 {
            self.events.emit(Event::LibraryChanged { added: st.added, updated: st.updated, missing: st.missing });
        }
        st
    }

    pub fn request_rescan(&self) { let _ = self.fs_tx.send(LibMsg::Rescan); }

    pub fn queue_analysis(&self, id: i64) { let _ = self.analysis_tx.send(id); }

    /// (Re)start sledování složek podle konfigurace.
    pub fn start_watching(self: &Arc<Self>) -> Result<()> {
        let tx = self.fs_tx.clone();
        let mut deb = new_debouncer(Duration::from_millis(1500), None, move |res: DebounceEventResult| {
            if let Ok(events) = res {
                let paths: Vec<PathBuf> = events.into_iter().flat_map(|e| e.event.paths.clone()).collect();
                if !paths.is_empty() { let _ = tx.send(LibMsg::Paths(paths)); }
            }
        })?;
        for wf in self.cfg.read().watch_folders.iter().filter(|w| w.enabled) {
            if wf.path.is_dir() {
                if let Err(e) = deb.watcher().watch(&wf.path, RecursiveMode::Recursive) {
                    self.events.alert("warning", "watch_failed", format!("{}: {e}", wf.path.display()));
                } else {
                    deb.cache().add_root(&wf.path, RecursiveMode::Recursive);
                    log::info!("sleduji složku {}", wf.path.display());
                }
            }
        }
        *self.watcher.lock() = Some(deb);
        self.request_rescan();
        Ok(())
    }

    fn fs_worker(self: Arc<Self>, rx: Receiver<LibMsg>) {
        let mut last_scan = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(LibMsg::Rescan) => { self.full_scan(); last_scan = Instant::now(); }
                Ok(LibMsg::Paths(paths)) => {
                    let mut st = ScanStats::default();
                    let uniq: HashSet<PathBuf> = paths.into_iter().collect();
                    for p in uniq {
                        if p.is_file() {
                            match self.index_file(&p) { Ok((a, u)) => { st.added += a; st.updated += u; } Err(e) => log::warn!("{}: {e}", p.display()) }
                        } else if p.is_dir() {
                            for e in walkdir::WalkDir::new(&p).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
                                if let Ok((a, u)) = self.index_file(e.path()) { st.added += a; st.updated += u; }
                            }
                        } else if !p.exists() && self.folder_for(&p).is_some() {
                            // smazáno nebo přesunuto (přesun se objeví jako nový soubor jinde)
                            if let Ok(n) = self.db.mark_missing_prefix(&path_str(&p)) { st.missing += n; }
                        }
                    }
                    if st.added + st.updated + st.missing > 0 {
                        self.events.emit(Event::LibraryChanged { added: st.added, updated: st.updated, missing: st.missing });
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // pojistka pro síťové disky, kde neprojdou systémové notifikace
            if last_scan.elapsed() > RESCAN_EVERY { self.full_scan(); last_scan = Instant::now(); }
        }
    }

    fn analysis_worker(self: Arc<Self>, rx: Receiver<i64>) {
        if let Ok(ids) = self.db.unanalyzed_ids() { for id in ids { let _ = self.analysis_tx.send(id); } }
        let mut last_emit = Instant::now();
        while let Ok(id) = rx.recv() {
            let Ok(Some(t)) = self.db.get_track(id) else { continue };
            if t.analyzed || t.missing { continue; }
            let th = self.cfg.read().autocue.for_kind(t.kind());
            match analyze_file(Path::new(&t.path), &th) {
                Ok(a) => { let _ = self.db.set_analysis(id, a.duration_ms, a.cue_in_ms, a.mix_ms, a.cue_out_ms, a.lufs); }
                Err(e) => {
                    self.events.alert("warning", "analysis_failed", format!("{}: {e}", t.path));
                    let _ = self.db.set_analysis(id, 0, 0, 0, 0, None); // nehratelné – vyřadí se z rotace
                }
            }
            if last_emit.elapsed() > Duration::from_millis(500) || rx.is_empty() {
                self.events.emit(Event::AnalysisProgress { pending: rx.len() });
                last_emit = Instant::now();
            }
        }
    }
}


impl Library {
    /// Zastaví sledování složek (např. během přesunu složky stanice).
    pub fn stop_watching(&self) { *self.watcher.lock() = None; }

    pub fn folder_for_public(&self, path: &Path) -> Option<WatchFolder> { self.folder_for(path) }

    pub fn notify_changed(&self, added: usize) { self.events.emit(Event::LibraryChanged { added, updated: 0, missing: 0 }); }

    pub fn root(&self) -> PathBuf { library_root(&self.cfg.read(), &self.data_dir) }

    /// Cílová složka pro nový soubor daného typu a kategorie.
    pub fn target_dir(&self, kind: TrackKind, category: &str) -> PathBuf {
        let cfg = self.cfg.read();
        let root = library_root(&cfg, &self.data_dir);
        let wf = cfg.watch_folders.iter().filter(|w| w.enabled && w.kind == kind)
            .min_by_key(|w| if w.path.starts_with(&root) { 0 } else { 1 }).cloned();
        let (base, sub) = match wf {
            Some(w) => (w.path.clone(), w.category_from_subfolder),
            None => (root.join(kind.folder_name()), true),
        };
        let dir = if sub && !category.trim().is_empty() { base.join(sanitize(category)) } else { base };
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Okamžitě zařadí soubor do knihovny (bez čekání na sledování složek) a vrátí záznam.
    pub fn register_file(&self, path: &Path, category: &str) -> Result<Option<Track>> {
        self.index_file(path)?;
        let Some((id, _, _, _)) = self.db.track_by_path(&path_str(path))? else { return Ok(None) };
        if !category.is_empty() {
            let cur = self.db.get_track(id)?.map(|t| t.category).unwrap_or_default();
            if cur != category {
                self.db.patch_track(id, &crate::db::TrackPatch { category: Some(category.to_string()), ..Default::default() })?;
            }
        }
        self.queue_analysis(id);
        Ok(self.db.get_track(id)?)
    }

    /// Přidá soubory nebo celé složky (rekurzivně). Soubory mimo sledované složky se zkopírují do knihovny.
    pub fn add_paths(&self, sources: &[PathBuf], kind: TrackKind, category: &str) -> ImportReport {
        let mut rep = ImportReport::default();
        let policy = self.cfg.read().library.on_duplicate.clone();
        let mut files = vec![];
        for s in sources {
            if s.is_dir() {
                for e in walkdir::WalkDir::new(s).follow_links(true).into_iter().filter_map(|e| e.ok()) {
                    if e.file_type().is_file() && is_supported(e.path()) { files.push(e.path().to_path_buf()); }
                }
            } else if s.is_file() {
                if is_supported(s) { files.push(s.clone()); } else { rep.skipped.push(format!("{}: nepodporovaný formát", s.display())); }
            } else {
                rep.errors.push(format!("{}: neexistuje", s.display()));
            }
        }
        for f in files {
            // už je ve sledované složce stejného typu → jen zaregistrovat
            if let Some(wf) = self.folder_for(&f) {
                if wf.kind == kind {
                    match self.register_file(&f, category) {
                        Ok(Some(t)) => rep.added.push(t),
                        Ok(None) => rep.skipped.push(f.display().to_string()),
                        Err(e) => rep.errors.push(format!("{}: {e}", f.display())),
                    }
                    continue;
                }
            }
            let dir = self.target_dir(kind, category);
            let name = sanitize(&f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            let Some(dest) = unique_path(&dir, &name, &policy) else { rep.skipped.push(format!("{name}: už existuje")); continue };
            if let Err(e) = std::fs::copy(&f, &dest) { rep.errors.push(format!("{}: {e}", f.display())); continue; }
            match self.register_file(&dest, category) {
                Ok(Some(t)) => rep.added.push(t),
                Ok(None) => rep.skipped.push(dest.display().to_string()),
                Err(e) => rep.errors.push(format!("{}: {e}", dest.display())),
            }
        }
        if !rep.added.is_empty() {
            self.events.emit(Event::LibraryChanged { added: rep.added.len(), updated: 0, missing: 0 });
        }
        rep
    }

    /// Smaže záznam; soubor se přesune do koše knihovny (_Trash), aby ho sledování znovu nepřidalo.
    pub fn delete_track(&self, id: i64) -> Result<()> {
        let Some(t) = self.db.get_track(id)? else { return Ok(()) };
        let src = PathBuf::from(&t.path);
        if src.exists() && self.folder_for(&src).is_some() {
            let trash = self.root().join(TRASH_DIR);
            std::fs::create_dir_all(&trash)?;
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let dest = trash.join(format!("{stamp}_{}", t.filename));
            if std::fs::rename(&src, &dest).is_err() {
                std::fs::copy(&src, &dest)?;
                std::fs::remove_file(&src)?;
            }
        }
        self.db.delete_track(id)?;
        self.events.emit(Event::LibraryChanged { added: 0, updated: 0, missing: 1 });
        Ok(())
    }

    /// Soubory (ne hudba), kterým skončila platnost – nejdřív nejstarší.
    pub fn expired(&self, older_than_days: u32) -> Result<Vec<Track>> {
        let now = chrono::Local::now().naive_local();
        let limit = now - chrono::Duration::days(older_than_days as i64);
        let all = self.db.with_conn(|c| -> Result<Vec<Track>> {
            let mut st = c.prepare(&format!("SELECT {} FROM tracks WHERE kind<>'song' AND valid_to<>''", crate::db::TRACK_COLS))?;
            let v = st.query_map([], crate::db::row_track)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        })?;
        let mut v: Vec<Track> = all.into_iter().filter(|t| t.expired(limit)).collect();
        v.sort_by(|a, b| a.valid_to.cmp(&b.valid_to));
        Ok(v)
    }

    /// Přesune soubory po platnosti do koše knihovny a odebere je z knihovny. Vrací počet.
    pub fn delete_expired(&self, ids: Option<&[i64]>, older_than_days: u32) -> Result<usize> {
        let list = self.expired(older_than_days)?;
        let mut n = 0;
        for t in list.iter().filter(|t| ids.map(|i| i.contains(&t.id)).unwrap_or(true)) { self.delete_track(t.id)?; n += 1; }
        Ok(n)
    }

    /// Vlastní obal skladby.
    pub fn set_custom_cover(&self, id: i64, data: &[u8], mime: &str) -> Result<()> {
        let dir = crate::station::images_dir(&self.cfg.read(), &self.data_dir, "covers");
        std::fs::create_dir_all(&dir)?;
        let ext = if mime.contains("png") { "png" } else if mime.contains("webp") { "webp" } else { "jpg" };
        for e in ["png", "jpg", "webp"] { let _ = std::fs::remove_file(dir.join(format!("{id}.{e}"))); }
        let p = dir.join(format!("{id}.{ext}"));
        std::fs::write(&p, data)?;
        self.db.set_cover(id, Some(&path_str(&p)))?;
        Ok(())
    }

    pub fn clear_custom_cover(&self, id: i64) -> Result<()> {
        if let Some(t) = self.db.get_track(id)? { if let Some(p) = t.cover_path { let _ = std::fs::remove_file(p); } }
        self.db.set_cover(id, None)
    }
}

/// Obal: vlastní obrázek, jinak z tagu.
pub fn cover_for(t: &Track) -> Option<(Vec<u8>, String)> {
    if let Some(p) = &t.cover_path {
        if let Ok(d) = std::fs::read(p) {
            let mime = if p.ends_with(".png") { "image/png" } else if p.ends_with(".webp") { "image/webp" } else { "image/jpeg" };
            return Some((d, mime.into()));
        }
    }
    read_cover(Path::new(&t.path))
}

/// Efektivní playlister kód (ruční, jinak prefix + id).
pub fn playlister_code(cfg: &Config, t: &Track) -> String {
    if let Some(c) = &t.playlister_code { if !c.is_empty() { return c.clone(); } }
    let pre = cfg.library.prefixes.get(&t.kind()).cloned().unwrap_or_else(|| t.kind().default_prefix().into());
    format!("{pre}{:05}", t.id)
}

/// Export knihovny do textu pro playlister / traffic software.
pub fn export_playlister(db: &Db, cfg: &Config, kind: Option<&str>, template: Option<&str>, since: Option<&str>) -> Result<String> {
    let f = crate::db::TrackFilter { kind: kind.map(String::from), include_missing: Some(false), limit: Some(1_000_000), ..Default::default() };
    let (tracks, _) = db.list_tracks(&f)?;
    let tpl = template.filter(|t| !t.is_empty()).unwrap_or(&cfg.library.export_template).to_string();
    let mut out = String::new();
    for t in tracks {
        if let Some(s) = since { if t.added_at.as_str() < s { continue; } }
        let (ci, _, co) = t.effective_cues();
        let dur_s = ((co - ci).max(0) as f64 / 1000.0).round() as i64;
        let line = tpl
            .replace("{CODE}", &playlister_code(cfg, &t))
            .replace("{ARTIST}", &t.artist).replace("{TITLE}", &t.title).replace("{ALBUM}", &t.album)
            .replace("{CATEGORY}", &t.category).replace("{CATEGORY2}", &t.category2).replace("{CATEGORY3}", &t.category3)
            .replace("{DURATION}", &format!("{}:{:02}", dur_s / 60, dur_s % 60)).replace("{DURATION_S}", &dur_s.to_string())
            .replace("{FILENAME}", &t.filename).replace("{PATH}", &t.path).replace("{ISRC}", &t.isrc)
            .replace("{YEAR}", &t.year.map(|y| y.to_string()).unwrap_or_default())
            .replace("{KIND}", t.kind().as_str());
        out += &line;
        out += "\r\n";
    }
    Ok(out)
}
