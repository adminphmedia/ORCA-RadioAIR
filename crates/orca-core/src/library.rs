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
    pub fn new(db: Arc<Db>, cfg: Arc<RwLock<Config>>, events: EventBus) -> Arc<Self> {
        let (atx, arx) = unbounded();
        let (ftx, frx) = unbounded();
        let lib = Arc::new(Self { db, cfg, events, analysis_tx: atx, fs_tx: ftx, watcher: Mutex::new(None) });
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
        if !is_supported(path) { return Ok((0, 0)); }
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
                if !entry.file_type().is_file() || !is_supported(p) { continue; }
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
