//! API: editor nahrávek a podcast.

use crate::{bad, ok, ApiError, ApiResult, St};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use orca_core::podcast::{self, Episode, Project};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

pub fn routes() -> Router<St> {
    Router::new()
        .route("/podcast", get(overview))
        .route("/podcast/project", post(create))
        .route("/podcast/project/{id}", get(get_p).put(save_p).delete(del_p))
        .route("/podcast/project/{id}/peaks", get(peaks))
        .route("/podcast/project/{id}/source", get(source))
        .route("/podcast/project/{id}/monitor", post(monitor))
        .route("/podcast/project/{id}/preview", post(preview))
        .route("/podcast/project/{id}/render", post(render))
        .route("/podcast/episode/{id}", axum::routing::put(ep_save).delete(ep_del))
        .route("/podcast/episodes/{file}", get(ep_file))
        .route("/podcast/publish", post(publish))
        .route("/podcast/feed.xml", get(feed))
        .route("/podcast/upload", post(upload).layer(DefaultBodyLimit::max(4 * 1024 * 1024 * 1024)))
        .route("/podcast/image", post(image_upload).layer(DefaultBodyLimit::max(20 * 1024 * 1024)))
        .route("/podcast/image/{name}", get(image_get))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))
}

const AUDIO: [&str; 9] = ["mp3", "wav", "flac", "m4a", "aac", "ogg", "opus", "aiff", "aif"];
fn is_audio(p: &std::path::Path) -> bool { p.extension().map(|e| AUDIO.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false) }

/// Nahrávky k úpravě: záznamy pořadů, výřezy z archivu, nahrané soubory.
fn recordings(data_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut v = vec![];
    let mut walk = |dir: PathBuf, group: &str| {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() { stack.push(p); continue; }
                if !is_audio(&p) { continue; }
                let m = e.metadata().ok();
                let modified = m.as_ref().and_then(|m| m.modified().ok()).map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339()).unwrap_or_default();
                v.push(json!({"path": p.to_string_lossy(), "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "group": group,
                    "folder": p.parent().and_then(|x| x.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "bytes": m.map(|m| m.len()).unwrap_or(0), "modified": modified}));
            }
        }
    };
    walk(data_dir.join("Logger").join("Pořady"), "shows");
    walk(data_dir.join("Exports").join("Logger"), "cuts");
    walk(podcast::root(data_dir).join("uploads"), "uploads");
    v.sort_by(|a, b| b["modified"].as_str().cmp(&a["modified"].as_str()));
    v
}

fn allowed_source(data_dir: &std::path::Path, p: &std::path::Path) -> bool {
    let Ok(c) = p.canonicalize() else { return false };
    [data_dir.join("Logger"), data_dir.join("Exports"), podcast::root(data_dir)].iter().filter_map(|d| d.canonicalize().ok()).any(|d| c.starts_with(d))
}

async fn overview(State(o): State<St>) -> ApiResult {
    let dd = o.data_dir.clone();
    let (projects, episodes, recs) = blocking(move || Ok((podcast::list_projects(&dd), podcast::list_episodes(&dd), recordings(&dd)))).await?;
    let projects: Vec<_> = projects.into_iter().map(|p| { let body = podcast::body_ms(&p); let mut v = json!(p); v["body_ms"] = json!(body); v }).collect();
    Ok(Json(json!({ "projects": projects, "episodes": episodes, "recordings": recs, "feed_url": "/api/podcast/feed.xml" })).into_response())
}

#[derive(Deserialize)]
struct CreateB { source: String, #[serde(default)] title: String }
async fn create(State(o): State<St>, Json(b): Json<CreateB>) -> ApiResult {
    let src = PathBuf::from(&b.source);
    if !allowed_source(&o.data_dir, &src) { return Err(bad("nahrávka musí být ze záznamů vysílání, výřezů nebo nahraná do podcastu")); }
    let (cfg, dd) = (o.cfg.read().clone(), o.data_dir.clone());
    let p = blocking(move || {
        let ms = podcast::probe_ms(&cfg, &src)?;
        let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mut p = Project { title: if b.title.is_empty() { stem } else { b.title }, source: src.to_string_lossy().to_string(), source_ms: ms,
            intro_track_id: cfg.podcast.default_intro, outro_track_id: cfg.podcast.default_outro, sweeper_track_id: cfg.podcast.default_sweeper, ..Default::default() };
        podcast::save_project(&dd, &mut p)?;
        Ok(p)
    }).await?;
    Ok(Json(json!(p)).into_response())
}

async fn get_p(State(o): State<St>, Path(id): Path<String>) -> ApiResult {
    let p = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let body = podcast::body_ms(&p);
    let mut v = json!(p); v["body_ms"] = json!(body);
    Ok(Json(v).into_response())
}

async fn save_p(State(o): State<St>, Path(id): Path<String>, Json(mut p): Json<Project>) -> ApiResult {
    let old = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    p.id = old.id; p.source = old.source; p.source_ms = old.source_ms; p.created = old.created;
    podcast::save_project(&o.data_dir, &mut p).map_err(|e| bad(&e.to_string()))?;
    let body = podcast::body_ms(&p);
    let mut v = json!(p); v["body_ms"] = json!(body);
    Ok(Json(v).into_response())
}

async fn del_p(State(o): State<St>, Path(id): Path<String>) -> ApiResult { podcast::delete_project(&o.data_dir, &id).map_err(|e| bad(&e.to_string()))?; ok() }

#[derive(Deserialize)]
struct PeaksQ { #[serde(default)] from: i64, #[serde(default)] to: i64, #[serde(default)] n: usize }
async fn peaks(State(o): State<St>, Path(id): Path<String>, Query(q): Query<PeaksQ>) -> ApiResult {
    let p = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let cfg = o.cfg.read().clone();
    let to = if q.to > q.from { q.to.min(p.source_ms) } else { p.source_ms };
    let from = q.from.clamp(0, to.max(1) - 1);
    let n = if q.n == 0 { 2000 } else { q.n };
    let v = blocking(move || podcast::peaks(&cfg, std::path::Path::new(&p.source), from, to, n)).await?;
    Ok(Json(json!({ "from": from, "to": to, "peaks": v })).into_response())
}

async fn source(State(o): State<St>, Path(id): Path<String>, req: axum::extract::Request) -> ApiResult {
    use tower::ServiceExt;
    let p = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let res = tower_http::services::ServeFile::new(&p.source).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(res.into_response())
}

#[derive(Deserialize)]
struct AtB { at_ms: i64 }
/// Poslech původní nahrávky od pozice přes předposlech (nikdy do vysílání).
async fn monitor(State(o): State<St>, Path(id): Path<String>, Json(b): Json<AtB>) -> ApiResult {
    let p = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let o2 = o.clone();
    blocking(move || { let c = o2.cfg.read().clone(); o2.monitor.play(&c, None, PathBuf::from(&p.source), b.at_ms.max(0), 0) }).await?;
    ok()
}

#[derive(Deserialize, Default)]
struct PreviewB { #[serde(default)] seconds: u32, #[serde(default)] monitor: bool }
/// Náhled výsledku (začátek epizody se znělkou a sweeperem) – vrací MP3, nebo hraje do předposlechu.
async fn preview(State(o): State<St>, Path(id): Path<String>, Json(b): Json<PreviewB>) -> ApiResult {
    let p = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let (cfg, db, dd) = (o.cfg.read().clone(), o.db.clone(), o.data_dir.clone());
    let secs = if b.seconds == 0 { 45 } else { b.seconds.min(600) };
    let path = blocking(move || podcast::render(&cfg, &db, &dd, &p, Some(secs))).await?;
    if b.monitor {
        let o2 = o.clone();
        blocking(move || { let c = o2.cfg.read().clone(); o2.monitor.play(&c, None, path, 0, 0) }).await?;
        return ok();
    }
    let data = tokio::fs::read(&path).await?;
    Ok(([(header::CONTENT_TYPE, "audio/mpeg")], data).into_response())
}

/// Vyrobí epizodu z úprav a přidá ji do seznamu epizod (zatím nezveřejněnou).
async fn render(State(o): State<St>, Path(id): Path<String>) -> ApiResult {
    let mut p = podcast::get_project(&o.data_dir, &id).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let (cfg, db, dd) = (o.cfg.read().clone(), o.db.clone(), o.data_dir.clone());
    let ep = blocking(move || {
        let out = podcast::render(&cfg, &db, &dd, &p, None)?;
        let file = out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let ms = podcast::probe_ms(&cfg, &out).unwrap_or(0);
        let mut eps = podcast::list_episodes(&dd);
        // nová verze téže úpravy nahradí starou (soubor smaže)
        if let Some(old) = eps.iter().position(|e| e.project_id == p.id) { let e = eps.remove(old); if e.file != file { let _ = std::fs::remove_file(podcast::episodes_dir(&dd).join(&e.file)); } }
        let ep = Episode { id: podcast::new_id(), project_id: p.id.clone(), title: p.title.clone(), description: p.description.clone(), file: file.clone(),
            bytes: std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0), duration_ms: ms, pub_date: chrono::Local::now().to_rfc3339(), published: false, uploaded: false };
        eps.insert(0, ep.clone());
        podcast::save_episodes(&dd, &eps)?;
        p.episode = file; podcast::save_project(&dd, &mut p)?;
        Ok(ep)
    }).await?;
    Ok(Json(json!(ep)).into_response())
}

#[derive(Deserialize)]
struct EpB { title: Option<String>, description: Option<String>, published: Option<bool> }
async fn ep_save(State(o): State<St>, Path(id): Path<String>, Json(b): Json<EpB>) -> ApiResult {
    let mut eps = podcast::list_episodes(&o.data_dir);
    let e = eps.iter_mut().find(|e| e.id == id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "epizoda neexistuje".into()))?;
    if let Some(t) = b.title { e.title = t; }
    if let Some(d) = b.description { e.description = d; }
    if let Some(p) = b.published { if p && !e.published { e.pub_date = chrono::Local::now().to_rfc3339(); } e.published = p; }
    podcast::save_episodes(&o.data_dir, &eps).map_err(|e| bad(&e.to_string()))?;
    ok()
}
async fn ep_del(State(o): State<St>, Path(id): Path<String>) -> ApiResult {
    let mut eps = podcast::list_episodes(&o.data_dir);
    let Some(i) = eps.iter().position(|e| e.id == id) else { return Err(ApiError(StatusCode::NOT_FOUND, "epizoda neexistuje".into())) };
    let e = eps.remove(i);
    let _ = std::fs::remove_file(podcast::episodes_dir(&o.data_dir).join(&e.file));
    podcast::save_episodes(&o.data_dir, &eps).map_err(|e| bad(&e.to_string()))?;
    ok()
}

async fn ep_file(State(o): State<St>, Path(file): Path<String>, req: axum::extract::Request) -> ApiResult {
    use tower::ServiceExt;
    if file.contains('/') || file.contains('\\') || file.contains("..") { return Err(bad("neplatný název")); }
    let p = podcast::episodes_dir(&o.data_dir).join(&file);
    if !p.is_file() { return Err(ApiError(StatusCode::NOT_FOUND, "epizoda neexistuje".into())); }
    Ok(tower_http::services::ServeFile::new(p).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.into_response())
}

async fn publish(State(o): State<St>) -> ApiResult {
    let (cfg, dd) = (o.cfg.read().clone(), o.data_dir.clone());
    let msg = blocking(move || podcast::publish(&cfg, &dd)).await?;
    Ok(Json(json!({ "ok": true, "message": msg })).into_response())
}

async fn feed(State(o): State<St>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let x = podcast::feed_xml(&cfg, &podcast::list_episodes(&o.data_dir));
    Ok(([(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")], x).into_response())
}

async fn save_upload(dir: PathBuf, mut mp: Multipart, check: fn(&std::path::Path) -> bool) -> Result<Vec<String>, ApiError> {
    tokio::fs::create_dir_all(&dir).await?;
    let mut saved = vec![];
    while let Some(mut field) = mp.next_field().await.map_err(|e| bad(&e.to_string()))? {
        let Some(fname) = field.file_name().map(orca_core::library::sanitize) else { continue };
        if !check(std::path::Path::new(&fname)) { return Err(bad(&format!("{fname}: nepodporovaný typ souboru"))); }
        let Some(dest) = orca_core::library::unique_path(&dir, &fname, "rename") else { continue };
        let mut f = tokio::fs::File::create(&dest).await?;
        while let Some(chunk) = field.chunk().await.map_err(|e| bad(&e.to_string()))? { f.write_all(&chunk).await?; }
        f.flush().await?;
        saved.push(dest.to_string_lossy().to_string());
    }
    Ok(saved)
}

async fn upload(State(o): State<St>, mp: Multipart) -> ApiResult {
    let saved = save_upload(podcast::root(&o.data_dir).join("uploads"), mp, is_audio).await?;
    Ok(Json(json!({ "saved": saved })).into_response())
}

fn is_image(p: &std::path::Path) -> bool { p.extension().map(|e| ["jpg", "jpeg", "png"].contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false) }
async fn image_upload(State(o): State<St>, mp: Multipart) -> ApiResult {
    let saved = save_upload(podcast::root(&o.data_dir), mp, is_image).await?;
    let name = saved.first().and_then(|p| std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().to_string())).ok_or_else(|| bad("žádný obrázek"))?;
    Ok(Json(json!({ "name": name })).into_response())
}
async fn image_get(State(o): State<St>, Path(name): Path<String>, req: axum::extract::Request) -> ApiResult {
    use tower::ServiceExt;
    if name.contains('/') || name.contains('\\') || name.contains("..") { return Err(bad("neplatný název")); }
    Ok(tower_http::services::ServeFile::new(podcast::root(&o.data_dir).join(name)).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.into_response())
}
