//! Složka stanice: jedno místo na disku pro všechny soubory stanice.
//!
//! ```text
//! <Složka stanice>/
//!   Songs/<kategorie>/…        hudba (sledovaná složka, kategorie = podsložka)
//!   Jingles/ Spots/ Sweepers/ Voice Tracks/ Assets/ Instant Jingles/ Beds/
//!   Import/                    automatická složka – co sem nakopírujete, samo se zařadí
//!     Songs/<kategorie>/ Jingles/ …   (podle podsložky se určí typ a kategorie)
//!     _Chyby/                  soubory, které nešlo zařadit (+ .txt s důvodem)
//!   Data/
//!     Images/Covers, Presenters, Shows, Spots, Station, Slides
//!     Recordings/ Documents/ Exports/
//!   _Trash/                    smazané soubory
//! ```
//!
//! Databáze (SQLite `orca.db`) zůstává v datové složce aplikace; ukládá absolutní cesty k souborům.
//! Při přesunu složky stanice se cesty v databázi přepíšou (všechny tabulky a sloupce).

use crate::config::Config;
use crate::db::Db;
use crate::decode::is_supported;
use crate::library::{self, library_root, Library};
use crate::model::TrackKind;
use anyhow::{anyhow, Result};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub const DATA_DIR: &str = "Data";
pub const INBOX_DIR: &str = "Import";
pub const INBOX_ERRORS: &str = "_Chyby";

pub const DATA_SUBDIRS: &[&str] = &[
    "Images/Covers", "Images/Presenters", "Images/Shows", "Images/Spots", "Images/Station", "Images/Slides",
    "Recordings", "Documents", "Exports",
];

/// Složka pro obrázky daného druhu (covers, presenters, shows, spots, station, slides).
/// Se zapnutým „data ve složce stanice“ `<stanice>/Data/Images/<Druh>`, jinak původní umístění v datové složce aplikace.
pub fn images_dir(cfg: &Config, data_dir: &Path, sub: &str) -> PathBuf {
    let name = match sub.to_ascii_lowercase().as_str() {
        "covers" => "Covers", "presenters" => "Presenters", "shows" => "Shows", "spots" => "Spots",
        "station" => "Station", "slides" => "Slides", _ => "Other",
    };
    let dir = if cfg.library.data_in_station {
        library_root(cfg, data_dir).join(DATA_DIR).join("Images").join(name)
    } else if name == "Covers" {
        data_dir.join("Covers")
    } else {
        data_dir.join("Media").join(sub)
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn inbox_dir(cfg: &Config, data_dir: &Path) -> PathBuf {
    if cfg.library.inbox.dir.as_os_str().is_empty() { library_root(cfg, data_dir).join(INBOX_DIR) } else { cfg.library.inbox.dir.clone() }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StructureReport { pub root: String, pub created: Vec<String>, pub existing: usize }

fn mk(dir: &Path, rep: &mut StructureReport, root: &Path) {
    if dir.is_dir() { rep.existing += 1; return; }
    match std::fs::create_dir_all(dir) {
        Ok(_) => rep.created.push(dir.strip_prefix(root).map(|p| p.display().to_string()).unwrap_or_else(|_| dir.display().to_string())),
        Err(e) => log::warn!("{}: {e}", dir.display()),
    }
}

/// Kategorie podle typu (z knihovny i ze seznamu kategorií).
pub fn categories_by_kind(db: &Db) -> HashMap<TrackKind, Vec<String>> {
    let mut m: HashMap<TrackKind, Vec<String>> = HashMap::new();
    if let Ok(cs) = db.categories() {
        for c in cs {
            if let Some(k) = TrackKind::parse(&c.kind) { if !c.category.trim().is_empty() { m.entry(k).or_default().push(c.category); } }
        }
    }
    let _ = db.with_conn(|c| -> rusqlite::Result<()> {
        let mut st = c.prepare("SELECT kind, name FROM categories WHERE level = 'category'")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (k, n) in rows.flatten() {
            if let Some(k) = TrackKind::parse(&k) { if !n.trim().is_empty() { m.entry(k).or_default().push(n); } }
        }
        Ok(())
    });
    for v in m.values_mut() { v.sort(); v.dedup(); }
    m
}

const README: &str = "ORCA RadioAIR – složka stanice\r\n\r\n\
Songs, Jingles, Spots, Sweepers, Voice Tracks, Assets, Instant Jingles, Beds\r\n\
  Knihovna podle typu. Podsložka = kategorie (např. Songs/Pop 90s). Složky se sledují: co sem\r\n\
  nakopírujete, objeví se v knihovně. Smazání souboru tady = skladba v knihovně se označí jako chybějící.\r\n\r\n\
Import\r\n\
  Automatická složka. Soubor sem položený (nejlépe do Import/<Typ>/<Kategorie>/) se po dokopírování sám\r\n\
  přesune do knihovny, převezme značky (cart chunk, RadioDJ, mAirList .mmd) a zmizí odsud.\r\n\
  Co nejde zařadit, skončí v Import/_Chyby s textovým souborem s důvodem.\r\n\r\n\
Data\r\n\
  Obrázky moderátorů, pořadů, obaly skladeb, obrázky ke spotům (DAB slideshow), nahrávky a dokumenty.\r\n\r\n\
Databáze (orca.db) je v datové složce aplikace; zálohy se dělají automaticky (Nastavení → Údržba).\r\n";

/// Vytvoří celou strukturu složky stanice. Vrací seznam nově vytvořených složek.
pub fn create_structure(cfg: &Config, data_dir: &Path, db: &Db) -> Result<StructureReport> {
    let root = library_root(cfg, data_dir);
    std::fs::create_dir_all(&root).map_err(|e| anyhow!("{}: {e}", root.display()))?;
    let mut rep = StructureReport { root: root.display().to_string(), ..Default::default() };
    let cats = if cfg.library.category_folders { categories_by_kind(db) } else { HashMap::new() };
    let inbox = inbox_dir(cfg, data_dir);
    for k in TrackKind::ALL {
        let base = root.join(k.folder_name());
        mk(&base, &mut rep, &root);
        mk(&inbox.join(k.folder_name()), &mut rep, &root);
        for c in cats.get(&k).into_iter().flatten() {
            let s = library::sanitize(c);
            mk(&base.join(&s), &mut rep, &root);
            mk(&inbox.join(k.folder_name()).join(&s), &mut rep, &root);
        }
    }
    mk(&inbox.join(INBOX_ERRORS), &mut rep, &root);
    for d in DATA_SUBDIRS { mk(&root.join(DATA_DIR).join(d), &mut rep, &root); }
    mk(&root.join(library::TRASH_DIR), &mut rep, &root);
    let readme = root.join("ČTI MĚ.txt");
    if !readme.exists() { let _ = std::fs::write(readme, README); }
    Ok(rep)
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderInfo { pub role: String, pub kind: Option<String>, pub path: String, pub exists: bool, pub tracks: i64 }

/// Přehled složek stanice (pro nastavení).
pub fn overview(cfg: &Config, data_dir: &Path, db: &Db) -> Vec<FolderInfo> {
    let root = library_root(cfg, data_dir);
    let count = |p: &Path| -> i64 {
        let mut s = p.display().to_string();
        if !s.ends_with(std::path::MAIN_SEPARATOR) { s.push(std::path::MAIN_SEPARATOR); }
        db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM tracks WHERE substr(path, 1, length(?1)) = ?1", [&s], |r| r.get(0))).unwrap_or(0)
    };
    let mut v = vec![FolderInfo { role: "root".into(), kind: None, path: root.display().to_string(), exists: root.is_dir(), tracks: count(&root) }];
    for k in TrackKind::ALL {
        let p = root.join(k.folder_name());
        v.push(FolderInfo { role: "kind".into(), kind: Some(k.as_str().into()), path: p.display().to_string(), exists: p.is_dir(), tracks: count(&p) });
    }
    let ib = inbox_dir(cfg, data_dir);
    v.push(FolderInfo { role: "inbox".into(), kind: None, path: ib.display().to_string(), exists: ib.is_dir(), tracks: 0 });
    let d = root.join(DATA_DIR);
    v.push(FolderInfo { role: "data".into(), kind: None, path: d.display().to_string(), exists: d.is_dir(), tracks: 0 });
    for wf in cfg.watch_folders.iter().filter(|w| !w.path.starts_with(&root)) {
        v.push(FolderInfo { role: "watch".into(), kind: Some(wf.kind.as_str().into()), path: wf.path.display().to_string(), exists: wf.path.is_dir(), tracks: count(&wf.path) });
    }
    v
}

/// Přepíše cestu ve všech textových sloupcích všech tabulek (i v JSON dokumentech plánu).
pub fn rewrite_all_paths(db: &Db, from: &str, to: &str) -> Result<usize> {
    if from.is_empty() || from == to { return Ok(0); }
    let esc = |s: &str| s.replace('\\', "\\\\");
    db.with_conn(|c| -> Result<usize> {
        let tables: Vec<String> = {
            let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let tx = c.unchecked_transaction()?;
        let mut n = 0;
        for t in tables {
            let cols: Vec<(String, String)> = {
                let mut st = tx.prepare(&format!("PRAGMA table_info(\"{t}\")"))?;
                st.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<_>>()?
            };
            for (col, ty) in cols {
                let ty = ty.to_ascii_uppercase();
                if !(ty.contains("TEXT") || ty.contains("CHAR") || ty.is_empty()) { continue; }
                let sql = format!("UPDATE \"{t}\" SET \"{col}\" = REPLACE(\"{col}\", ?1, ?2) WHERE instr(\"{col}\", ?1) > 0");
                n += tx.execute(&sql, [from, to])?;
                if esc(from) != from { n += tx.execute(&sql, [esc(from), esc(to)])?; }
            }
        }
        tx.execute("UPDATE tracks SET missing=0 WHERE instr(path, ?1) = 1", [to])?;
        tx.commit()?;
        Ok(n)
    })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RelocateReport { pub from: String, pub to: String, pub moved_files: usize, pub rewritten: usize, pub errors: Vec<String> }

/// Přesune soubory staré složky stanice do nové (rename, přes disky kopie + smazání).
pub fn move_tree(from: &Path, to: &Path, rep: &mut RelocateReport) {
    let files: Vec<PathBuf> = walkdir::WalkDir::new(from).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.path().to_path_buf()).collect();
    for f in files {
        let Ok(rel) = f.strip_prefix(from) else { continue };
        let dest = to.join(rel);
        if let Some(p) = dest.parent() { if let Err(e) = std::fs::create_dir_all(p) { rep.errors.push(format!("{}: {e}", p.display())); continue; } }
        if dest.exists() { rep.errors.push(format!("{}: v cíli už existuje, ponechán", rel.display())); continue; }
        let r = std::fs::rename(&f, &dest).or_else(|_| std::fs::copy(&f, &dest).and_then(|_| std::fs::remove_file(&f)));
        match r { Ok(_) => rep.moved_files += 1, Err(e) => rep.errors.push(format!("{}: {e}", rel.display())) }
    }
    // prázdné složky po přesunu
    let mut dirs: Vec<PathBuf> = walkdir::WalkDir::new(from).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_dir()).map(|e| e.path().to_path_buf()).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs { let _ = std::fs::remove_dir(d); }
}

/// Převede cestu v konfiguraci ze staré složky stanice do nové.
pub fn remap(p: &Path, from: &Path, to: &Path) -> PathBuf {
    p.strip_prefix(from).map(|rel| to.join(rel)).unwrap_or_else(|_| p.to_path_buf())
}

pub fn with_sep(p: &Path) -> String {
    let mut s = p.display().to_string();
    if !s.ends_with(std::path::MAIN_SEPARATOR) { s.push(std::path::MAIN_SEPARATOR); }
    s
}

// ---------------------------------------------------------------------------------------------
// Automatická složka (inbox)

#[derive(Debug, Clone, Serialize)]
pub struct InboxEntry { pub at: String, pub file: String, pub ok: bool, pub message: String, pub track_id: Option<i64>, pub kind: String, pub category: String }

#[derive(Debug, Clone, Serialize)]
pub struct InboxStatus { pub enabled: bool, pub dir: String, pub waiting: usize, pub processed: usize, pub failed: usize, pub log: Vec<InboxEntry> }

pub struct Inbox {
    cfg: Arc<RwLock<Config>>,
    library: Arc<Library>,
    db: Arc<Db>,
    data_dir: PathBuf,
    log: Mutex<VecDeque<InboxEntry>>,
    seen: Mutex<HashMap<PathBuf, (u64, SystemTime)>>,
    counters: Mutex<(usize, usize)>,
}

const SKIP_EXT: &[&str] = &["part", "crdownload", "tmp", "download", "partial", "mmd", "txt", "ini", "db", "ds_store"];

fn kind_from_folder(name: &str) -> Option<TrackKind> {
    let n = crate::import::norm(name);
    TrackKind::ALL.into_iter().find(|k| crate::import::norm(k.folder_name()) == n || k.as_str() == n).or(match n.as_str() {
        "song" | "hudba" | "pisne" | "skladby" | "music" => Some(TrackKind::Song),
        "jingle" | "znelky" | "znelka" => Some(TrackKind::Jingle),
        "spot" | "reklama" | "reklamy" | "spoty" | "commercials" => Some(TrackKind::Spot),
        "sweeper" | "predely" => Some(TrackKind::Sweeper),
        "voicetrack" | "vt" | "voicetracky" => Some(TrackKind::VoiceTrack),
        "podkresy" | "podkres" | "bed" => Some(TrackKind::Bed),
        "instant" | "instanty" | "instantjingly" => Some(TrackKind::Instant),
        "ostatni" | "asset" | "other" => Some(TrackKind::Asset),
        _ => None,
    })
}

impl Inbox {
    pub fn start(cfg: Arc<RwLock<Config>>, library: Arc<Library>, db: Arc<Db>, data_dir: PathBuf) -> Arc<Self> {
        let me = Arc::new(Self { cfg, library, db, data_dir, log: Mutex::new(VecDeque::new()), seen: Mutex::new(HashMap::new()), counters: Mutex::new((0, 0)) });
        let m = me.clone();
        std::thread::Builder::new().name("orca-inbox".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_secs(3));
            if m.cfg.read().library.inbox.enabled { m.tick(); }
        }).unwrap();
        me
    }

    pub fn dir(&self) -> PathBuf { inbox_dir(&self.cfg.read(), &self.data_dir) }

    pub fn status(&self) -> InboxStatus {
        let c = *self.counters.lock();
        InboxStatus {
            enabled: self.cfg.read().library.inbox.enabled, dir: self.dir().display().to_string(),
            waiting: self.seen.lock().len(), processed: c.0, failed: c.1, log: self.log.lock().iter().rev().cloned().collect(),
        }
    }

    fn push(&self, e: InboxEntry) {
        let mut c = self.counters.lock();
        if e.ok { c.0 += 1 } else { c.1 += 1 }
        let mut l = self.log.lock();
        l.push_back(e);
        while l.len() > 300 { l.pop_front(); }
    }

    /// Jedno projití automatické složky. Vrací počet zpracovaných souborů.
    pub fn tick(&self) -> usize {
        let dir = self.dir();
        if !dir.is_dir() {
            if std::fs::create_dir_all(dir.join(INBOX_ERRORS)).is_ok() {
                for k in TrackKind::ALL { let _ = std::fs::create_dir_all(dir.join(k.folder_name())); }
            }
            return 0;
        }
        let settle = self.cfg.read().library.inbox.settle_s as u64;
        let mut ready = vec![];
        let mut present = std::collections::HashSet::new();
        for e in walkdir::WalkDir::new(&dir).min_depth(1).into_iter().filter_entry(|e| e.file_name() != INBOX_ERRORS).filter_map(|e| e.ok()) {
            if !e.file_type().is_file() { continue; }
            let p = e.path().to_path_buf();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.starts_with("~$") { continue; }
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if SKIP_EXT.contains(&ext.as_str()) || name.eq_ignore_ascii_case("ČTI MĚ.txt") { continue; }
            let Ok(md) = e.metadata() else { continue };
            let stamp = (md.len(), md.modified().unwrap_or(SystemTime::UNIX_EPOCH));
            present.insert(p.clone());
            let mut seen = self.seen.lock();
            let prev = seen.insert(p.clone(), stamp);
            let old_enough = stamp.1.elapsed().map(|d| d.as_secs() >= settle).unwrap_or(true);
            if prev == Some(stamp) && old_enough && stamp.0 > 0 { ready.push(p); }
        }
        self.seen.lock().retain(|p, _| present.contains(p));
        let n = ready.len();
        for p in ready {
            self.seen.lock().remove(&p);
            self.process(&dir, &p);
        }
        n
    }

    fn process(&self, inbox: &Path, p: &Path) {
        let rel: Vec<String> = p.strip_prefix(inbox).map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect()).unwrap_or_default();
        let (default_kind, read_markers, policy) = { let c = self.cfg.read(); (c.library.inbox.default_kind, c.library.inbox.read_markers, c.library.on_duplicate.clone()) };
        let dirs = &rel[..rel.len().saturating_sub(1)];
        let (kind, category) = match dirs.first().and_then(|d| kind_from_folder(d)) {
            Some(k) => (k, dirs.get(1).cloned().unwrap_or_default()),
            None => (default_kind, dirs.first().cloned().unwrap_or_default()),
        };
        let fname = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut entry = InboxEntry { at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), file: rel.join("/"), ok: false, message: String::new(), track_id: None, kind: kind.as_str().into(), category: category.clone() };
        let res: Result<i64> = (|| {
            if !is_supported(p) { return Err(anyhow!("nepodporovaný formát")); }
            let markers = if read_markers { crate::import::file_markers(p) } else { Default::default() };
            let dest_dir = self.library.target_dir(kind, &category);
            let Some(dest) = library::unique_path(&dest_dir, &library::sanitize(&fname), &policy) else { return Err(anyhow!("soubor se stejným názvem už v knihovně je (nastavení duplicit: přeskočit)")) };
            if std::fs::rename(p, &dest).is_err() { std::fs::copy(p, &dest)?; std::fs::remove_file(p)?; }
            for sc in [PathBuf::from(format!("{}.mmd", p.display())), p.with_extension("mmd")] {
                if sc.is_file() { let _ = std::fs::rename(&sc, PathBuf::from(format!("{}.mmd", dest.display()))); }
            }
            let t = self.library.register_file(&dest, &category)?.ok_or_else(|| anyhow!("soubor nejde zařadit – typ {} nemá sledovanou složku", kind.as_str()))?;
            if dest.extension().map(|x| x.eq_ignore_ascii_case("wav")).unwrap_or(false) {
                if let Some(w) = crate::import::read_wav_info(&dest) {
                    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    if t.artist.is_empty() && (t.title.is_empty() || t.title == stem) && (!w.title.is_empty() || !w.artist.is_empty()) {
                        let s = |v: String| if v.is_empty() { None } else { Some(v) };
                        self.db.patch_track(t.id, &crate::db::TrackPatch { artist: s(w.artist), title: s(w.title), playlister_code: s(w.cut_id), ..Default::default() })?;
                    }
                }
            }
            if !markers.is_empty() {
                let patch = crate::db::TrackPatch { cue_in_ms: markers.cue_in_ms, intro_ms: markers.intro_ms, outro_ms: markers.outro_ms, mix_ms: markers.mix_ms, cue_out_ms: markers.cue_out_ms, hook_in_ms: markers.hook_in_ms, hook_out_ms: markers.hook_out_ms, ..Default::default() };
                self.db.patch_track(t.id, &patch)?;
            }
            Ok(t.id)
        })();
        match res {
            Ok(id) => {
                entry.ok = true; entry.track_id = Some(id);
                entry.message = format!("zařazeno do {}{}", kind.folder_name(), if category.is_empty() { String::new() } else { format!(" / {category}") });
                self.library.notify_changed(1);
            }
            Err(e) => {
                entry.message = e.to_string();
                let errdir = inbox.join(INBOX_ERRORS);
                let _ = std::fs::create_dir_all(&errdir);
                if p.exists() {
                    if let Some(d) = library::unique_path(&errdir, &fname, "rename") {
                        if std::fs::rename(p, &d).is_ok() { let _ = std::fs::write(PathBuf::from(format!("{}.txt", d.display())), format!("{}\r\n{}\r\n", entry.at, e)); }
                    }
                }
                log::warn!("automatická složka: {}: {e}", entry.file);
            }
        }
        self.push(entry);
    }
}

/// Pomocné: čeká, dokud automatická složka nezpracuje soubory (pro testy).
pub fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout { if f() { return true; } std::thread::sleep(Duration::from_millis(50)); }
    false
}
