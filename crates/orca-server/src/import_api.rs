//! API: složka stanice, automatická složka, import z jiných programů a procházení disku.

use crate::{bad, ApiError, ApiResult, St};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use orca_core::import::{self, ImportOptions};
use orca_core::station;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

pub fn routes() -> Router<St> {
    Router::new()
        .route("/station", get(station_info))
        .route("/station/structure", post(structure))
        .route("/station/relocate", post(relocate))
        .route("/library/inbox", get(inbox_status))
        .route("/library/inbox/scan", post(inbox_scan))
        .route("/import/presets", get(presets))
        .route("/import/upload", post(upload).layer(DefaultBodyLimit::max(1024 * 1024 * 1024)))
        .route("/import/preview", post(preview))
        .route("/import/run", post(run))
        .route("/import/status", get(status))
        .route("/fs/list", get(fs_list))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::from)?.map_err(ApiError::from)
}

async fn station_info(State(o): State<St>) -> ApiResult {
    let v = blocking(move || {
        let cfg = o.cfg.read().clone();
        let root = orca_core::library::library_root(&cfg, &o.data_dir);
        Ok(json!({
            "root": root.display().to_string(),
            "custom_root": !cfg.library.root.as_os_str().is_empty(),
            "data_dir": o.data_dir.display().to_string(),
            "database": o.data_dir.join("orca.db").display().to_string(),
            "inbox": station::inbox_dir(&cfg, &o.data_dir).display().to_string(),
            "images": station::images_dir(&cfg, &o.data_dir, "covers").parent().map(|p| p.display().to_string()),
            "folders": station::overview(&cfg, &o.data_dir, &o.db),
            "data_subdirs": station::DATA_SUBDIRS,
        }))
    }).await?;
    Ok(Json(v).into_response())
}

async fn structure(State(o): State<St>) -> ApiResult {
    let rep = blocking(move || {
        let cfg = o.cfg.read().clone();
        let r = station::create_structure(&cfg, &o.data_dir, &o.db)?;
        let _ = o.library.start_watching();
        Ok(r)
    }).await?;
    Ok(Json(rep).into_response())
}

#[derive(Deserialize)]
struct RelocateReq { path: String, #[serde(default = "yes")] move_files: bool, #[serde(default = "yes")] rewrite_db: bool }
fn yes() -> bool { true }

async fn relocate(State(o): State<St>, Json(r): Json<RelocateReq>) -> ApiResult {
    if r.path.trim().is_empty() { return Err(bad("zadejte složku")); }
    let rep = blocking(move || {
        let _ = orca_core::general::backup_now(&o.db, &o.cfg.read(), &o.data_dir);
        o.relocate_station(&PathBuf::from(r.path.trim()), r.move_files, r.rewrite_db)
    }).await?;
    Ok(Json(rep).into_response())
}

async fn inbox_status(State(o): State<St>) -> ApiResult { Ok(Json(o.inbox.status()).into_response()) }

async fn inbox_scan(State(o): State<St>) -> ApiResult {
    let n = blocking(move || {
        // dvě projití: první zaznamená velikost, druhé zpracuje soubory, které se nemění
        let mut n = o.inbox.tick();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        n += o.inbox.tick();
        Ok(n)
    }).await?;
    Ok(Json(json!({ "processed": n })).into_response())
}

async fn presets() -> ApiResult {
    Ok(Json(json!({ "presets": import::PRESETS, "fields": import::FIELDS, "radiodj_kinds": import::radiodj_kind_map() })).into_response())
}

#[derive(Deserialize)]
struct UploadQ { name: Option<String> }

/// Nahraje exportní soubor (CSV, SQL, M3U) na server; vrátí cestu, kterou pak použijte jako `source`.
async fn upload(State(o): State<St>, Query(q): Query<UploadQ>, body: Bytes) -> ApiResult {
    let dir = o.data_dir.join("Import");
    let name = orca_core::library::sanitize(q.name.as_deref().unwrap_or("import.csv"));
    let p = blocking(move || {
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(format!("{}_{name}", chrono::Local::now().format("%Y%m%d_%H%M%S")));
        std::fs::write(&p, &body)?;
        Ok(p)
    }).await?;
    Ok(Json(json!({ "path": p.display().to_string() })).into_response())
}

async fn preview(Json(opts): Json<ImportOptions>) -> ApiResult {
    let pv = blocking(move || import::preview(&opts)).await?;
    Ok(Json(pv).into_response())
}

async fn run(State(o): State<St>, Json(opts): Json<ImportOptions>) -> ApiResult {
    let st = blocking(move || {
        let _ = orca_core::general::backup_now(&o.db, &o.cfg.read(), &o.data_dir);
        import::start(o.library.clone(), o.db.clone(), opts)
    }).await?;
    Ok(Json(st).into_response())
}

async fn status() -> ApiResult { Ok(Json(import::status()).into_response()) }

#[derive(Deserialize)]
struct FsQ { path: Option<String>, files: Option<bool> }

/// Procházení disku (výběr složky / souboru v nastavení). Jen složky a podporované soubory.
async fn fs_list(Query(q): Query<FsQ>) -> ApiResult {
    let v = blocking(move || {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let path = q.path.filter(|p| !p.trim().is_empty()).map(PathBuf::from).unwrap_or_else(|| home.clone());
        // soubor nebo neexistující cesta → nejbližší existující nadřazená složka
        let mut path = path;
        while !path.is_dir() { match path.parent() { Some(p) if !p.as_os_str().is_empty() => path = p.to_path_buf(), _ => { path = home.clone(); break; } } }
        let mut roots = vec![json!({"name": "Domů", "path": home.display().to_string()})];
        if cfg!(target_os = "macos") {
            if let Ok(rd) = std::fs::read_dir("/Volumes") { for e in rd.flatten() { roots.push(json!({"name": e.file_name().to_string_lossy(), "path": e.path().display().to_string()})); } }
        } else if cfg!(windows) {
            for l in b'C'..=b'Z' { let d = format!("{}:\\", l as char); if std::path::Path::new(&d).exists() { roots.push(json!({"name": d, "path": d})); } }
        } else { roots.push(json!({"name": "/", "path": "/"})); }
        let mut dirs_v = vec![];
        let mut files_v = vec![];
        if let Ok(rd) = std::fs::read_dir(&path) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') { continue; }
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() { dirs_v.push(name); }
                else if q.files.unwrap_or(false) {
                    let low = name.to_lowercase();
                    if [".csv", ".tsv", ".txt", ".sql", ".m3u", ".m3u8", ".pls", ".xml"].iter().any(|x| low.ends_with(x)) { files_v.push(name); }
                }
            }
        }
        dirs_v.sort_by_key(|s| s.to_lowercase());
        files_v.sort_by_key(|s| s.to_lowercase());
        Ok(json!({
            "path": path.display().to_string(),
            "parent": path.parent().map(|p| p.display().to_string()),
            "exists": path.is_dir(),
            "dirs": dirs_v, "files": files_v, "roots": roots, "sep": std::path::MAIN_SEPARATOR.to_string(),
        }))
    }).await?;
    Ok(Json(v).into_response())
}
