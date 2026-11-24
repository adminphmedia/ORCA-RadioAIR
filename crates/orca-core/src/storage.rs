//! Přehled místa na disku: kolik zabírají jednotlivé části ORCA (knihovna po typech a kategoriích, videa,
//! koš, zálohy, databáze, reporty…) a kolik je na disku celkem / volno. Počítá se na pozadí a ukládá do mezipaměti.

use crate::config::Config;
use crate::model::TrackKind;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Usage { pub bytes: u64, pub files: u64 }

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    /// knihovna | video | trash | backups | database | data
    pub group: String,
    pub kind: Option<TrackKind>,
    pub path: String,
    pub bytes: u64,
    pub files: u64,
    pub children: Vec<Child>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Child { pub label: String, pub bytes: u64, pub files: u64 }

#[derive(Debug, Clone, Serialize)]
pub struct Disk { pub path: String, pub total: u64, pub free: u64, pub orca: u64 }

#[derive(Debug, Clone, Serialize, Default)]
pub struct Report { pub computed_at: Option<String>, pub running: bool, pub nodes: Vec<Node>, pub disks: Vec<Disk>, pub total: u64, pub elapsed_ms: u64 }

fn walk(p: &Path) -> Usage {
    let mut u = Usage::default();
    for e in walkdir::WalkDir::new(p).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() { u.files += 1; u.bytes += e.metadata().map(|m| m.len()).unwrap_or(0); }
    }
    u
}

fn children(p: &Path, skip: &[&str]) -> (Vec<Child>, Usage) {
    let mut kids = vec![];
    let mut total = Usage::default();
    let mut loose = Usage::default();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if skip.contains(&name.as_str()) { continue; }
            let path = e.path();
            if path.is_dir() {
                let u = walk(&path);
                total.bytes += u.bytes; total.files += u.files;
                kids.push(Child { label: name, bytes: u.bytes, files: u.files });
            } else if let Ok(m) = e.metadata() {
                loose.bytes += m.len(); loose.files += 1;
            }
        }
    }
    if loose.files > 0 { kids.push(Child { label: "(soubory přímo ve složce)".into(), bytes: loose.bytes, files: loose.files }); total.bytes += loose.bytes; total.files += loose.files; }
    kids.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    (kids, total)
}

pub fn compute(cfg: &Config, data_dir: &Path) -> Report {
    let t0 = std::time::Instant::now();
    let root = crate::library::library_root(cfg, data_dir);
    let mut nodes = vec![];
    let mut seen: Vec<PathBuf> = vec![];
    for k in TrackKind::ALL {
        let p = root.join(k.folder_name());
        if !p.is_dir() { continue; }
        let (kids, u) = children(&p, &[]);
        seen.push(p.clone());
        nodes.push(Node { id: format!("lib-{}", k.as_str()), label: k.folder_name().into(), group: "library".into(), kind: Some(k), path: p.to_string_lossy().to_string(), bytes: u.bytes, files: u.files, children: kids });
    }
    // sledované složky mimo knihovnu (např. NAS)
    for w in cfg.watch_folders.iter().filter(|w| w.enabled) {
        if w.path.starts_with(&root) || seen.iter().any(|s| w.path.starts_with(s)) || !w.path.is_dir() { continue; }
        let (kids, u) = children(&w.path, &[]);
        seen.push(w.path.clone());
        nodes.push(Node { id: format!("watch-{}", w.id), label: w.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), group: "library".into(), kind: Some(w.kind), path: w.path.to_string_lossy().to_string(), bytes: u.bytes, files: u.files, children: kids });
    }
    let trash = root.join(crate::library::TRASH_DIR);
    if trash.is_dir() { let u = walk(&trash); nodes.push(Node { id: "trash".into(), label: "Koš knihovny (_Trash)".into(), group: "trash".into(), kind: None, path: trash.to_string_lossy().to_string(), bytes: u.bytes, files: u.files, children: vec![] }); }
    let vdir = crate::video::video_dir(cfg, data_dir);
    if vdir.is_dir() { let u = walk(&vdir); nodes.push(Node { id: "video".into(), label: "Videa (převod na zvuk)".into(), group: "video".into(), kind: None, path: vdir.to_string_lossy().to_string(), bytes: u.bytes, files: u.files, children: vec![] }); }
    // datová složka: databáze + ostatní podsložky (zálohy, reporty, cache, DAB…)
    let mut db = Usage::default();
    for f in ["orca.db", "orca.db-wal", "orca.db-shm"] { if let Ok(m) = std::fs::metadata(data_dir.join(f)) { db.bytes += m.len(); db.files += 1; } }
    nodes.push(Node { id: "database".into(), label: "Databáze".into(), group: "database".into(), kind: None, path: data_dir.join("orca.db").to_string_lossy().to_string(), bytes: db.bytes, files: db.files, children: vec![] });
    if let Ok(rd) = std::fs::read_dir(data_dir) {
        let mut other: BTreeMap<String, PathBuf> = BTreeMap::new();
        for e in rd.flatten() {
            let p = e.path();
            // knihovna a videa mají vlastní řádky
            if !p.is_dir() || root.starts_with(&p) || vdir.starts_with(&p) { continue; }
            other.insert(e.file_name().to_string_lossy().to_string(), p);
        }
        for (name, p) in other {
            let (kids, u) = children(&p, &[]);
            let group = if name == "Backups" { "backups" } else { "data" };
            nodes.push(Node { id: format!("data-{name}"), label: name, group: group.into(), kind: None, path: p.to_string_lossy().to_string(), bytes: u.bytes, files: u.files, children: kids });
        }
    }
    let total: u64 = nodes.iter().map(|n| n.bytes).sum();
    // disky: datová složka, knihovna a složky mimo ni (každý disk jednou); ORCA = součet částí na daném disku
    let id_of = |p: &Path| -> Option<(u64, u64)> { let q = if p.exists() { p.to_path_buf() } else { p.parent()?.to_path_buf() }; Some((fs4::total_space(&q).ok()?, fs4::available_space(&q).ok()?)) };
    let same = |a: (u64, u64), d: &Disk| a.0 == d.total && a.1.abs_diff(d.free) < 256 * 1024 * 1024;
    let mut disks: Vec<Disk> = vec![];
    let mut probe: Vec<PathBuf> = vec![data_dir.to_path_buf(), root.clone()];
    probe.extend(seen.iter().cloned());
    for p in probe {
        let Some(k) = id_of(&p) else { continue };
        if disks.iter().any(|d| same(k, d)) { continue; }
        disks.push(Disk { path: p.to_string_lossy().to_string(), total: k.0, free: k.1, orca: 0 });
    }
    for n in &nodes {
        if let Some(k) = id_of(Path::new(&n.path)) { if let Some(d) = disks.iter_mut().find(|d| same(k, d)) { d.orca += n.bytes; } }
    }
    Report { computed_at: Some(chrono::Local::now().to_rfc3339()), running: false, nodes, disks, total, elapsed_ms: t0.elapsed().as_millis() as u64 }
}

#[derive(Default)]
pub struct Storage { pub report: Arc<Mutex<Report>> }

impl Storage {
    /// Spustí přepočet na pozadí (velká knihovna může trvat desítky sekund).
    pub fn refresh(&self, cfg: Config, data_dir: PathBuf) {
        { let mut r = self.report.lock(); if r.running { return; } r.running = true; }
        let rep = self.report.clone();
        std::thread::Builder::new().name("orca-storage".into()).spawn(move || { let r = compute(&cfg, &data_dir); *rep.lock() = r; }).ok();
    }
}

/// Vysype koš knihovny (trvale smaže soubory v _Trash). Vrací (počet, bajty).
pub fn empty_trash(cfg: &Config, data_dir: &Path) -> (u64, u64) {
    let trash = crate::library::library_root(cfg, data_dir).join(crate::library::TRASH_DIR);
    let u = walk(&trash);
    if let Ok(rd) = std::fs::read_dir(&trash) {
        for e in rd.flatten() { let p = e.path(); let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) }; }
    }
    let left = walk(&trash);
    (u.files - left.files.min(u.files), u.bytes - left.bytes.min(u.bytes))
}
