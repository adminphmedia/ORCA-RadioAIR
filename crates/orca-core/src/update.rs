//! Aktualizace aplikace: zachování dat mezi verzemi a jednotlačítková aktualizace.
//!
//! Data (nastavení orca.toml, databáze orca.db, logy, zálohy) leží mimo aplikaci ve složce dat
//! (macOS ~/Library/Application Support/ORCA RadioAIR, Windows %APPDATA%\ORCA RadioAIR) – instalace
//! nové verze je nepřepíše. Před prvním spuštěním nové verze se navíc vytvoří záloha (před migrací databáze).
//! Samotné stažení a instalaci zajišťuje desktopová aplikace (Tauri updater) přes `UpdateHook`.

use parking_lot::Mutex;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateInfo { pub version: String, pub notes: String, pub date: String }

/// Napojení na updater desktopové aplikace (v samostatném serveru není).
#[derive(Clone)]
pub struct UpdateHook {
    pub check: Arc<dyn Fn() -> Result<Option<UpdateInfo>, String> + Send + Sync>,
    /// Stáhne, ověří podpis, nainstaluje a restartuje aplikaci; průběh hlásí do `UpdateState.progress`.
    pub install: Arc<dyn Fn(Arc<Mutex<UpdateState>>) -> Result<(), String> + Send + Sync>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateState {
    pub supported: bool,
    pub current: String,
    pub available: Option<UpdateInfo>,
    pub checked_at: Option<String>,
    pub checking: bool,
    pub installing: bool,
    /// 0–100 při stahování
    pub progress: Option<u8>,
    pub error: Option<String>,
    /// verze, ze které se aktualizovalo při tomto spuštění (pro hlášku „Aktualizováno z …“)
    pub updated_from: Option<String>,
    pub pre_update_backup: Option<String>,
}

/// Před otevřením databáze: když se změnila verze, zkopíruje databázi a nastavení do Backups/před aktualizací.
/// Vrací (předchozí verze, cesta zálohy).
pub fn backup_on_version_change(data_dir: &Path) -> (Option<String>, Option<String>) {
    let marker = data_dir.join("version.txt");
    let prev = std::fs::read_to_string(&marker).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let db = data_dir.join("orca.db");
    let mut path = None;
    let changed = prev.as_deref() != Some(crate::VERSION);
    if changed && db.is_file() {
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let dir = data_dir.join("Backups").join(format!("pred-aktualizaci_{}_na_{}_{stamp}", prev.clone().unwrap_or_else(|| "starsi".into()), crate::VERSION));
        if std::fs::create_dir_all(&dir).is_ok() {
            let mut ok = true;
            for f in ["orca.db", "orca.db-wal", "orca.db-shm", "orca.toml"] {
                let src = data_dir.join(f);
                if src.is_file() && std::fs::copy(&src, dir.join(f)).is_err() { ok = false; }
            }
            if ok { path = Some(dir.to_string_lossy().to_string()); log::info!("záloha před aktualizací: {}", dir.display()); }
            // ponechat posledních 5 záloh před aktualizací
            if let Ok(rd) = std::fs::read_dir(data_dir.join("Backups")) {
                let mut v: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("pred-aktualizaci_")).unwrap_or(false)).collect();
                v.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
                while v.len() > 5 { let _ = std::fs::remove_dir_all(v.remove(0)); }
            }
            else { log::warn!("záloha před aktualizací se nepodařila celá: {}", dir.display()); }
        }
    }
    (if changed { prev } else { None }, path)
}

pub fn mark_version(data_dir: &Path) { let _ = std::fs::write(data_dir.join("version.txt"), crate::VERSION); }
