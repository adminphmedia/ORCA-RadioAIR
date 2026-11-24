//! API pro kompletní knihovnu: přidávání souborů, kategorie, memos, palety, spoty/kampaně,
//! reklamní bloky, Stitcher a Studio nahrávání.

use crate::{bad, not_found, ok, ApiError, ApiResult, St};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::NaiveDate;
use orca_core::catalog::{self, CategoryEntry, Memo, Pad, Palette};
use orca_core::db::TrackPatch;
use orca_core::library::{sanitize, unique_path, ImportReport};
use orca_core::model::TrackKind;
use orca_core::spots::{self, Campaign, Preset};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

pub fn routes() -> Router<St> {
    Router::new()
        // knihovna
        .route("/library/upload", post(upload).layer(DefaultBodyLimit::max(8 * 1024 * 1024 * 1024)))
        .route("/library/import", post(import_paths))
        .route("/library/info", get(library_info))
        .route("/library/export.txt", get(export_txt))
        .route("/library/broadcasts", get(broadcasts))
        .route("/tracks/bulk", post(bulk))
        .route("/tracks/{id}/delete", post(track_delete))
        .route("/tracks/{id}/plays", get(track_plays))
        .route("/tracks/{id}/waveform", get(track_waveform))
        .route("/tracks/{id}/audio", get(track_audio))
        .route("/tracks/{id}/cover/custom", post(cover_set).delete(cover_clear).layer(DefaultBodyLimit::max(20 * 1024 * 1024)))
        // číselníky
        .route("/catalog/categories", get(cat_list).post(cat_create))
        .route("/catalog/categories/{id}", put(cat_update).delete(cat_delete))
        // memos
        .route("/memos", get(memo_list).post(memo_save))
        .route("/memos/{id}", put(memo_update).delete(memo_delete))
        .route("/memos/import", post(memo_import))
        // palety
        .route("/palettes", get(pal_list).post(pal_create))
        .route("/palettes/{id}", put(pal_update).delete(pal_delete))
        .route("/palettes/{id}/pads", put(pad_set))
        // spoty
        .route("/spots/campaigns", get(camp_list).post(camp_create))
        .route("/spots/campaigns/{id}", get(camp_get).put(camp_update).delete(camp_delete))
        .route("/spots/breaks", get(breaks_day))
        .route("/spots/breaks/insert", post(break_insert))
        .route("/spots/breaks/reset", post(break_reset))
        .route("/spots/playlister/import", post(pl_import).layer(DefaultBodyLimit::max(10 * 1024 * 1024)))
        .route("/spots/playlister/export.txt", get(pl_export))
        .route("/spots/playlister/durations.txt", get(pl_durations))
        .route("/spots/playlister/cleanup", post(pl_cleanup))
        .route("/spots/presets", get(preset_list).post(preset_save))
        .route("/spots/presets/{id}", delete(preset_delete))
        .route("/spots/reports/{name}", get(spot_report))
        .route("/spots/clients", get(client_list).post(client_save))
        .route("/spots/clients/{id}", delete(client_delete))
        .route("/spots/spot-meta", get(spot_meta_list))
        .route("/spots/spot-meta/{id}", get(spot_meta_get).put(spot_meta_put))
        .route("/spots/spot-meta/{id}/image", post(spot_image_upload).get(spot_image_get).delete(spot_image_delete).layer(DefaultBodyLimit::max(20 * 1024 * 1024)))
        .route("/spots/availability", get(spot_availability))
        .route("/spots/proof", get(spot_proof))
        // stitcher
        .route("/queue/stitch", post(queue_stitch))
        .route("/queue/bed", post(queue_bed))
        .route("/bed/stop", post(bed_stop))
        .route("/stitcher/preview", get(stitch_preview))
        // studio – nahrávání
        .route("/record/devices", get(rec_devices))
        .route("/mic/devices", get(rec_devices))
        .route("/mic/status", get(mic_status))
        .route("/record/status", get(rec_status))
        .route("/record/start", post(rec_start))
        .route("/record/stop", post(rec_stop))
}

fn kind_of(s: Option<&str>) -> Result<TrackKind, ApiError> {
    match s { None | Some("") => Ok(TrackKind::Song), Some(k) => TrackKind::parse(k).ok_or_else(|| bad("neznámý typ knihovny")) }
}

fn text(body: String, ctype: &'static str, filename: &str) -> Response {
    ([(header::CONTENT_TYPE, ctype.to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))], body).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::from)?.map_err(ApiError::from)
}

// ---------------------------------------------------------------------------------------------
// Knihovna

#[derive(Deserialize)]
struct UploadQ { kind: Option<String>, category: Option<String> }

/// multipart/form-data: soubory (pole "file"/"files"), volitelně pole "kind" a "category".
async fn upload(State(o): State<St>, Query(q): Query<UploadQ>, mut mp: Multipart) -> ApiResult {
    let mut kind = kind_of(q.kind.as_deref())?;
    let mut category = q.category.clone().unwrap_or_default();
    let policy = o.cfg.read().library.on_duplicate.clone();
    let mut saved: Vec<(PathBuf, String)> = vec![];
    let mut rep = ImportReport::default();
    while let Some(mut field) = mp.next_field().await.map_err(|e| bad(&e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        if name == "kind" { kind = kind_of(Some(&field.text().await.unwrap_or_default()))?; continue; }
        if name == "category" { category = field.text().await.unwrap_or_default(); continue; }
        let Some(fname) = field.file_name().map(sanitize) else { continue };
        if !orca_core::decode::is_supported(std::path::Path::new(&fname)) { rep.skipped.push(format!("{fname}: nepodporovaný formát")); continue; }
        let dir = o.library.target_dir(kind, &category);
        let Some(dest) = unique_path(&dir, &fname, &policy) else { rep.skipped.push(format!("{fname}: už existuje")); continue };
        // soubor se zapisuje pod dočasným jménem, aby ho sledování složek nezachytilo rozepsaný
        let tmp = dir.join(format!(".orca-upload-{}", uuid_like()));
        let mut f = tokio::fs::File::create(&tmp).await?;
        while let Some(chunk) = field.chunk().await.map_err(|e| bad(&e.to_string()))? { f.write_all(&chunk).await?; }
        f.flush().await?;
        drop(f);
        tokio::fs::rename(&tmp, &dest).await?;
        saved.push((dest, category.clone()));
    }
    let lib = o.library.clone();
    let rep = blocking(move || {
        for (p, cat) in saved {
            match lib.register_file(&p, &cat) {
                Ok(Some(t)) => rep.added.push(t),
                Ok(None) => rep.skipped.push(p.display().to_string()),
                Err(e) => rep.errors.push(format!("{}: {e}", p.display())),
            }
        }
        Ok(rep)
    }).await?;
    if !rep.added.is_empty() { o.events.emit(orca_core::events::Event::LibraryChanged { added: rep.added.len(), updated: 0, missing: 0 }); }
    Ok(Json(rep).into_response())
}

fn uuid_like() -> String { format!("{:x}{:x}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)) }

#[derive(Deserialize)]
struct ImportBody { paths: Vec<String>, kind: Option<String>, category: Option<String> }

/// Import souborů/složek z disku počítače (cesty) – soubory mimo knihovnu se zkopírují.
async fn import_paths(State(o): State<St>, Json(b): Json<ImportBody>) -> ApiResult {
    let kind = kind_of(b.kind.as_deref())?;
    let cat = b.category.unwrap_or_default();
    let paths: Vec<PathBuf> = b.paths.iter().map(|p| PathBuf::from(p.trim())).filter(|p| !p.as_os_str().is_empty()).collect();
    if paths.is_empty() { return Err(bad("žádné cesty")); }
    let lib = o.library.clone();
    Ok(Json(blocking(move || Ok(lib.add_paths(&paths, kind, &cat))).await?).into_response())
}

async fn library_info(State(o): State<St>) -> ApiResult {
    let root = o.library.root();
    let folders: Vec<_> = TrackKind::ALL.iter().map(|k| json!({"kind": k.as_str(), "path": o.library.target_dir(*k, "")})).collect();
    Ok(Json(json!({"root": root, "trash": root.join(orca_core::library::TRASH_DIR), "folders": folders, "stats": o.db.stats()?})).into_response())
}

#[derive(Deserialize)]
struct ExportQ { kind: Option<String>, template: Option<String>, since: Option<String> }
async fn export_txt(State(o): State<St>, Query(q): Query<ExportQ>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let body = orca_core::library::export_playlister(&o.db, &cfg, q.kind.as_deref(), q.template.as_deref(), q.since.as_deref())?;
    Ok(text(body, "text/plain; charset=utf-8", &format!("orca-{}.txt", q.kind.unwrap_or_else(|| "knihovna".into()))))
}

#[derive(Deserialize)]
struct RangeQ { from: Option<String>, to: Option<String>, kind: Option<String>, limit: Option<i64> }
async fn broadcasts(State(o): State<St>, Query(q): Query<RangeQ>) -> ApiResult {
    let from = q.from.unwrap_or_else(|| orca_core::db::utc_minus_minutes(30 * 24 * 60));
    let to = q.to.unwrap_or_else(|| "9999".into());
    Ok(Json(o.db.plays_by_track(&from, &to, q.kind.as_deref(), q.limit.unwrap_or(500))?).into_response())
}

#[derive(Deserialize)]
struct BulkBody { ids: Vec<i64>, patch: Option<TrackPatch>, action: Option<String> }
/// Hromadné úpravy (Jazler: mass change) a akce: reanalyze | reset_cues | delete | enable | disable | bookmark | unbookmark
async fn bulk(State(o): State<St>, Json(b): Json<BulkBody>) -> ApiResult {
    let o2 = o.clone();
    let n = blocking(move || {
        let mut n = 0;
        for id in &b.ids {
            if o2.db.get_track(*id)?.is_none() { continue; }
            if let Some(p) = &b.patch { o2.db.patch_track(*id, p)?; }
            match b.action.as_deref() {
                Some("reanalyze") => { o2.db.reset_analysis(*id, false)?; o2.library.queue_analysis(*id); }
                Some("reset_cues") => { o2.db.reset_analysis(*id, true)?; o2.library.queue_analysis(*id); }
                Some("delete") => o2.library.delete_track(*id)?,
                Some("enable") => o2.db.patch_track(*id, &TrackPatch { enabled: Some(true), ..Default::default() })?,
                Some("disable") => o2.db.patch_track(*id, &TrackPatch { enabled: Some(false), ..Default::default() })?,
                Some("bookmark") => o2.db.patch_track(*id, &TrackPatch { bookmarked: Some(true), ..Default::default() })?,
                Some("unbookmark") => o2.db.patch_track(*id, &TrackPatch { bookmarked: Some(false), ..Default::default() })?,
                Some(other) if !other.is_empty() => return Err(anyhow::anyhow!("neznámá akce {other}")),
                _ => {}
            }
            n += 1;
        }
        Ok(n)
    }).await?;
    o.events.emit(orca_core::events::Event::LibraryChanged { added: 0, updated: n, missing: 0 });
    Ok(Json(json!({"ok": true, "count": n})).into_response())
}

async fn track_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    let lib = o.library.clone();
    blocking(move || lib.delete_track(id)).await?;
    ok()
}

async fn track_plays(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(o.db.plays_of_track(id, 1000)?).into_response())
}

/// Vlastní obal: tělo požadavku = obrázek (Content-Type image/jpeg|png|webp) nebo multipart pole "file".
#[derive(Deserialize)]
struct WaveQ { points: Option<usize> }
/// Průběh signálu (špičky + RMS) pro editor cue bodů.
async fn track_waveform(State(o): State<St>, Path(id): Path<i64>, Query(q): Query<WaveQ>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    let path = PathBuf::from(orca_core::general::map_path(&o.cfg.read(), &t.path));
    let key = format!("{id}-{}", std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0));
    let cache = o.data_dir.join("Cache").join("waveforms");
    let w = blocking(move || orca_core::analysis::waveform(&path, &cache, &key, q.points.unwrap_or(1600))).await?;
    Ok(Json(w).into_response())
}

/// Zvuk skladby pro poslech v editoru (podpora Range); formáty, které prohlížeč neumí, se převedou na MP3.
async fn track_audio(State(o): State<St>, Path(id): Path<i64>, req: axum::extract::Request) -> ApiResult {
    use tower::ServiceExt;
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    let path = PathBuf::from(orca_core::general::map_path(&o.cfg.read(), &t.path));
    if !path.is_file() { return Err(not_found("soubor")); }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let serve = if ["mp3", "m4a", "aac", "mp4", "wav", "flac"].contains(&ext.as_str()) { path } else {
        let cache = o.data_dir.join("Cache").join("audio");
        std::fs::create_dir_all(&cache)?;
        let out = cache.join(format!("{id}.mp3"));
        if !out.is_file() {
            let ff = orca_core::encoder::resolve_ffmpeg(&o.cfg.read());
            let (src, dst) = (path.clone(), out.clone());
            let ok = tokio::task::spawn_blocking(move || std::process::Command::new(ff).args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(&src).args(["-c:a", "libmp3lame", "-b:a", "192k"]).arg(&dst).status().map(|s| s.success()).unwrap_or(false)).await.unwrap_or(false);
            if !ok { return Err(bad("soubor nelze převést pro poslech")); }
        }
        out
    };
    let res = tower_http::services::ServeFile::new(serve).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(res.into_response())
}

async fn cover_set(State(o): State<St>, Path(id): Path<i64>, headers: HeaderMap, body: Bytes) -> ApiResult {
    o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("image/jpeg").to_string();
    let data: Vec<u8> = if ct.starts_with("multipart/") {
        // jednoduché vytažení prvního souboru z multipart
        let boundary = ct.split("boundary=").nth(1).ok_or_else(|| bad("chybí boundary"))?.trim_matches('"').to_string();
        extract_first_part(&body, &boundary).ok_or_else(|| bad("v požadavku není obrázek"))?
    } else { body.to_vec() };
    if data.is_empty() { return Err(bad("prázdný obrázek")); }
    let ff = orca_core::encoder::resolve_ffmpeg(&o.cfg.read());
    let (data, ext) = orca_core::slides::normalize_upload(&data, &ff).map_err(|e| bad(&e.to_string()))?;
    let mime = if ext == "png" { "image/png" } else { "image/jpeg" };
    o.library.set_custom_cover(id, &data, mime)?;
    ok()
}

fn extract_first_part(body: &[u8], boundary: &str) -> Option<Vec<u8>> {
    let delim = format!("--{boundary}");
    let s = body;
    let find = |hay: &[u8], needle: &[u8], from: usize| hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from);
    let start = find(s, delim.as_bytes(), 0)?;
    let hdr_end = find(s, b"\r\n\r\n", start)? + 4;
    let end = find(s, format!("\r\n{delim}").as_bytes(), hdr_end)?;
    Some(s[hdr_end..end].to_vec())
}

async fn cover_clear(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { o.library.clear_custom_cover(id)?; ok() }

// ---------------------------------------------------------------------------------------------
// Číselníky kategorií

#[derive(Deserialize)]
struct CatQ { kind: Option<String>, level: Option<String>, clear: Option<bool> }
async fn cat_list(State(o): State<St>, Query(q): Query<CatQ>) -> ApiResult {
    Ok(Json(catalog::list_categories(&o.db, q.kind.as_deref().filter(|s| !s.is_empty()), q.level.as_deref().filter(|s| !s.is_empty()))?).into_response())
}
async fn cat_create(State(o): State<St>, Json(e): Json<CategoryEntry>) -> ApiResult {
    let id = catalog::create_category(&o.db, &e)?;
    o.events.emit(orca_core::events::Event::LibraryChanged { added: 0, updated: 0, missing: 0 });
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
async fn cat_update(State(o): State<St>, Path(id): Path<i64>, Json(e): Json<CategoryEntry>) -> ApiResult {
    catalog::update_category(&o.db, id, &e)?;
    o.events.emit(orca_core::events::Event::LibraryChanged { added: 0, updated: 1, missing: 0 });
    ok()
}
async fn cat_delete(State(o): State<St>, Path(id): Path<i64>, Query(q): Query<CatQ>) -> ApiResult {
    catalog::delete_category(&o.db, id, q.clear.unwrap_or(false))?;
    o.events.emit(orca_core::events::Event::LibraryChanged { added: 0, updated: 1, missing: 0 });
    ok()
}

// ---------------------------------------------------------------------------------------------
// Memos

#[derive(Deserialize)]
struct MemoQ { category: Option<String>, q: Option<String> }
async fn memo_list(State(o): State<St>, Query(q): Query<MemoQ>) -> ApiResult {
    Ok(Json(catalog::list_memos(&o.db, q.category.as_deref(), q.q.as_deref())?).into_response())
}
async fn memo_save(State(o): State<St>, Json(m): Json<Memo>) -> ApiResult { Ok(Json(catalog::save_memo(&o.db, &m)?).into_response()) }
async fn memo_update(State(o): State<St>, Path(id): Path<i64>, Json(mut m): Json<Memo>) -> ApiResult {
    m.id = id;
    Ok(Json(catalog::save_memo(&o.db, &m)?).into_response())
}
async fn memo_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { catalog::delete_memo(&o.db, id)?; ok() }
/// Import textových souborů jako memos (multipart, volitelné pole "category").
async fn memo_import(State(o): State<St>, mut mp: Multipart) -> ApiResult {
    let mut category = String::new();
    let mut out = vec![];
    while let Some(field) = mp.next_field().await.map_err(|e| bad(&e.to_string()))? {
        if field.name() == Some("category") { category = field.text().await.unwrap_or_default(); continue; }
        let title = field.file_name().map(|f| std::path::Path::new(f).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()).unwrap_or_default();
        let data = field.bytes().await.map_err(|e| bad(&e.to_string()))?;
        let body = String::from_utf8_lossy(&data).trim_start_matches('\u{feff}').to_string();
        out.push(catalog::save_memo(&o.db, &Memo { title, body, category: category.clone(), ..Default::default() })?);
    }
    Ok(Json(out).into_response())
}

// ---------------------------------------------------------------------------------------------
// Palety instant jinglů

async fn pal_list(State(o): State<St>) -> ApiResult { Ok(Json(catalog::list_palettes(&o.db)?).into_response()) }
async fn pal_create(State(o): State<St>, Json(mut p): Json<Palette>) -> ApiResult {
    p.id = 0;
    let id = catalog::save_palette(&o.db, &p)?;
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
async fn pal_update(State(o): State<St>, Path(id): Path<i64>, Json(mut p): Json<Palette>) -> ApiResult {
    p.id = id;
    catalog::save_palette(&o.db, &p)?;
    ok()
}
async fn pal_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { catalog::delete_palette(&o.db, id)?; ok() }
async fn pad_set(State(o): State<St>, Path(id): Path<i64>, Json(p): Json<Pad>) -> ApiResult { catalog::set_pad(&o.db, id, &p)?; ok() }

// ---------------------------------------------------------------------------------------------
// Spoty / kampaně

fn now_local_str(o: &St) -> String { orca_core::automation::now_local(&o.cfg.read()).format("%Y-%m-%d %H:%M").to_string() }

#[derive(Deserialize)]
struct CampQ { status: Option<String> }
async fn camp_list(State(o): State<St>, Query(q): Query<CampQ>) -> ApiResult {
    let now = now_local_str(&o);
    Ok(Json(spots::list_campaigns(&o.db, q.status.as_deref().filter(|s| !s.is_empty() && *s != "all"), &now)?).into_response())
}
async fn camp_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(spots::get_campaign(&o.db, id)?.ok_or_else(|| not_found("kampaň"))?).into_response())
}
async fn camp_create(State(o): State<St>, Json(mut c): Json<Campaign>) -> ApiResult {
    c.id = 0;
    Ok(Json(spots::save_campaign(&o.db, &c).map_err(|e| bad(&e.to_string()))?).into_response())
}
async fn camp_update(State(o): State<St>, Path(id): Path<i64>, Json(mut c): Json<Campaign>) -> ApiResult {
    c.id = id;
    Ok(Json(spots::save_campaign(&o.db, &c).map_err(|e| bad(&e.to_string()))?).into_response())
}
async fn camp_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { spots::delete_campaign(&o.db, id)?; ok() }

fn parse_date(o: &St, s: Option<&str>) -> Result<NaiveDate, ApiError> {
    match s.filter(|x| !x.is_empty()) {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| bad("datum ve formátu YYYY-MM-DD")),
        None => Ok(orca_core::automation::now_local(&o.cfg.read()).date_naive()),
    }
}

#[derive(Deserialize)]
struct DayQ { date: Option<String>, only_nonempty: Option<bool>, campaign_id: Option<i64>, from: Option<String>, to: Option<String> }
async fn breaks_day(State(o): State<St>, Query(q): Query<DayQ>) -> ApiResult {
    let date = parse_date(&o, q.date.as_deref())?;
    let cfg = o.cfg.read().clone();
    Ok(Json(spots::day_blocks(&o.db, &cfg, date, q.only_nonempty.unwrap_or(false))?).into_response())
}

#[derive(Deserialize)]
struct BreakBody { date: Option<String>, time: String }
async fn break_insert(State(o): State<St>, Json(b): Json<BreakBody>) -> ApiResult {
    let date = parse_date(&o, b.date.as_deref())?;
    let n = o.automation.insert_break_now(date, &b.time)?;
    Ok(Json(json!({"ok": true, "items": n})).into_response())
}
async fn break_reset(State(o): State<St>, Json(b): Json<BreakBody>) -> ApiResult {
    let date = parse_date(&o, b.date.as_deref())?;
    spots::unmark_inserted(&o.db, &date.format("%Y-%m-%d").to_string(), &b.time)?;
    ok()
}

#[derive(Deserialize)]
struct PlJson { date: Option<String>, text: String, verify: Option<bool> }
/// Import plánu z traffic softwaru: JSON {date, text, verify} nebo multipart se soubory YYYYMMDD.txt.
async fn pl_import(State(o): State<St>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let cfg = o.cfg.read().clone();
    if ct.starts_with("application/json") {
        let j: PlJson = serde_json::from_slice(&body).map_err(|e| bad(&e.to_string()))?;
        let date = parse_date(&o, j.date.as_deref())?;
        return Ok(Json(vec![spots::import_playlister(&o.db, &cfg, date, &j.text, j.verify.unwrap_or(true))?]).into_response());
    }
    if ct.starts_with("multipart/") {
        let boundary = ct.split("boundary=").nth(1).ok_or_else(|| bad("chybí boundary"))?.trim_matches('"').to_string();
        let mut out = vec![];
        for (fname, data) in split_multipart(&body, &boundary) {
            let Some(date) = spots::date_from_filename(&fname) else { return Err(bad(&format!("{fname}: název souboru musí obsahovat datum YYYYMMDD"))); };
            out.push(spots::import_playlister(&o.db, &cfg, date, &String::from_utf8_lossy(&data), true)?);
        }
        return Ok(Json(out).into_response());
    }
    Err(ApiError(StatusCode::UNSUPPORTED_MEDIA_TYPE, "očekávám JSON nebo multipart".into()))
}

fn split_multipart(body: &[u8], boundary: &str) -> Vec<(String, Vec<u8>)> {
    let delim = format!("--{boundary}");
    let mut out = vec![];
    let find = |hay: &[u8], needle: &[u8], from: usize| if from >= hay.len() { None } else { hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from) };
    let mut pos = 0;
    while let Some(start) = find(body, delim.as_bytes(), pos) {
        let Some(hdr_end) = find(body, b"\r\n\r\n", start) else { break };
        let headers = String::from_utf8_lossy(&body[start..hdr_end]).to_string();
        let data_start = hdr_end + 4;
        let Some(end) = find(body, format!("\r\n{delim}").as_bytes(), data_start) else { break };
        if let Some(fname) = headers.split("filename=\"").nth(1).and_then(|s| s.split('"').next()) {
            out.push((fname.to_string(), body[data_start..end].to_vec()));
        }
        pos = end + 2;
    }
    out
}

async fn pl_export(State(o): State<St>, Query(q): Query<DayQ>) -> ApiResult {
    let date = parse_date(&o, q.date.as_deref())?;
    let cfg = o.cfg.read().clone();
    Ok(text(spots::export_breaks_playlist(&o.db, &cfg, date)?, "text/plain; charset=utf-8", &format!("{}.txt", date.format("%Y%m%d"))))
}
async fn pl_durations(State(o): State<St>, Query(q): Query<DayQ>) -> ApiResult {
    let date = parse_date(&o, q.date.as_deref())?;
    let cfg = o.cfg.read().clone();
    Ok(text(spots::export_breaks_duration(&o.db, &cfg, date)?, "text/plain; charset=utf-8", &format!("{}-delky.txt", date.format("%Y%m%d"))))
}
#[derive(Deserialize)]
struct CleanBody { days: Option<i64> }
async fn pl_cleanup(State(o): State<St>, Json(b): Json<CleanBody>) -> ApiResult {
    let n = spots::cleanup_old_playlister_spots(&o.db, b.days.unwrap_or(60))?;
    Ok(Json(json!({"ok": true, "disabled": n})).into_response())
}

async fn preset_list(State(o): State<St>) -> ApiResult { Ok(Json(spots::list_presets(&o.db)?).into_response()) }
async fn preset_save(State(o): State<St>, Json(p): Json<Preset>) -> ApiResult {
    let id = spots::save_preset(&o.db, &p)?;
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
async fn preset_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { spots::delete_preset(&o.db, id)?; ok() }

/// plays.csv?campaign_id · schedule.csv?campaign_id · running.csv?from&to · daily.csv?date
async fn spot_report(State(o): State<St>, Path(name): Path<String>, Query(q): Query<DayQ>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let body = match name.as_str() {
        "plays.csv" => spots::report_campaign_plays(&o.db, q.campaign_id.ok_or_else(|| bad("campaign_id"))?)?,
        "schedule.csv" => spots::report_campaign_schedule(&o.db, q.campaign_id.ok_or_else(|| bad("campaign_id"))?)?,
        "running.csv" => spots::report_running(&o.db, q.from.as_deref().unwrap_or("0000"), q.to.as_deref().unwrap_or("9999"))?,
        "daily.csv" => spots::report_daily(&o.db, &cfg, parse_date(&o, q.date.as_deref())?)?,
        _ => return Err(not_found("report")),
    };
    Ok(text(body, "text/csv; charset=utf-8", &name))
}

// ---------------------------------------------------------------------------------------------
// Reklama – klienti, obrázek a text spotu pro DAB+, obsazenost bloků, protokol o odvysílání


async fn client_list(State(o): State<St>) -> ApiResult {
    let now = now_local_str(&o);
    let v = blocking(move || orca_core::ads::list_clients(&o.db, &now)).await?;
    Ok(Json(v).into_response())
}
async fn client_save(State(o): State<St>, Json(c): Json<orca_core::ads::Client>) -> ApiResult {
    let id = blocking(move || orca_core::ads::save_client(&o.db, &c)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.1))?;
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
async fn client_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    blocking(move || orca_core::ads::delete_client(&o.db, id)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.1))?;
    ok()
}
async fn spot_meta_list(State(o): State<St>) -> ApiResult { Ok(Json(orca_core::ads::spot_metas(&o.db)?).into_response()) }
async fn spot_meta_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(orca_core::ads::spot_meta(&o.db, id)?.unwrap_or(orca_core::ads::SpotMeta { track_id: id, ..Default::default() })).into_response())
}
async fn spot_meta_put(State(o): State<St>, Path(id): Path<i64>, Json(mut m): Json<orca_core::ads::SpotMeta>) -> ApiResult {
    m.track_id = id;
    if let Some(cur) = orca_core::ads::spot_meta(&o.db, id)? { if m.image_path.is_empty() { m.image_path = cur.image_path; } }
    orca_core::ads::save_spot_meta(&o.db, &m)?;
    o.meta.refresh();
    ok()
}
async fn spot_image_upload(State(o): State<St>, Path(id): Path<i64>, body: Bytes) -> ApiResult {
    let dir = orca_core::station::images_dir(&o.cfg.read(), &o.data_dir, "spots");
    let o2 = o.clone();
    blocking(move || {
        let ff = orca_core::encoder::resolve_ffmpeg(&o2.cfg.read());
        let p = orca_core::ads::store_spot_image(&dir, id, &body, &ff)?;
        let mut m = orca_core::ads::spot_meta(&o2.db, id)?.unwrap_or(orca_core::ads::SpotMeta { track_id: id, ..Default::default() });
        m.image_path = p;
        orca_core::ads::save_spot_meta(&o2.db, &m)
    }).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.1))?;
    o.meta.refresh();
    ok()
}
async fn spot_image_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let m = orca_core::ads::spot_meta(&o.db, id)?.filter(|m| !m.image_path.is_empty()).ok_or_else(|| not_found("obrázek"))?;
    let b = std::fs::read(&m.image_path).map_err(|_| not_found("obrázek"))?;
    let ct = if m.image_path.ends_with(".png") { "image/png" } else { "image/jpeg" };
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "no-cache")], b).into_response())
}
async fn spot_image_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    if let Some(mut m) = orca_core::ads::spot_meta(&o.db, id)? {
        let _ = std::fs::remove_file(&m.image_path);
        m.image_path.clear();
        orca_core::ads::save_spot_meta(&o.db, &m)?;
        o.meta.refresh();
    }
    ok()
}
#[derive(Deserialize)]
struct AvailQ { from: Option<String>, days: Option<u32> }
async fn spot_availability(State(o): State<St>, Query(q): Query<AvailQ>) -> ApiResult {
    let from = parse_date(&o, q.from.as_deref())?;
    let cfg = o.cfg.read().clone();
    let v = blocking(move || orca_core::ads::availability(&o.db, &cfg, from, q.days.unwrap_or(7))).await?;
    Ok(Json(v).into_response())
}
#[derive(Deserialize)]
struct ProofQ { client_id: Option<i64>, campaign_id: Option<i64>, from: String, to: String, #[serde(default)] format: Option<String> }
async fn spot_proof(State(o): State<St>, Query(q): Query<ProofQ>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    if q.format.as_deref() == Some("json") {
        let v = blocking(move || orca_core::ads::plays_for(&o.db, q.client_id, q.campaign_id, &q.from, &q.to)).await?;
        return Ok(Json(v).into_response());
    }
    let h = blocking(move || orca_core::ads::proof_html(&o.db, &cfg, q.client_id, q.campaign_id, &q.from, &q.to)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.1))?;
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], h).into_response())
}

// ---------------------------------------------------------------------------------------------
// Stitcher

#[derive(Deserialize)]
struct StitchBody { r#type: String, position: Option<usize> }
#[derive(Deserialize)]
struct BedBody { action: String, #[serde(default)] track_id: Option<i64>, #[serde(default)] position: Option<usize> }
/// Podkres do decků: ON (soubor z knihovny Podkresy) nebo OFF.
async fn queue_bed(State(o): State<St>, Json(b): Json<BedBody>) -> ApiResult {
    let item = if b.action == "off" {
        orca_core::model::QueueItem::bed_off("manual", "podkres OFF – přidáno ručně")
    } else {
        let t = o.db.get_track(b.track_id.ok_or_else(|| bad("vyberte podkres"))?)?.ok_or_else(|| crate::not_found("podkres"))?;
        orca_core::model::QueueItem::bed_on(t, "manual", "podkres ON – přidáno ručně")
    };
    let id = item.id.clone();
    o.engine.queue_push(item, b.position);
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
/// Okamžitě ukončit podkres (OFF hned na začátek decků a přechod na něj).
async fn bed_stop(State(o): State<St>) -> ApiResult {
    o.engine.bed_stop_now();
    crate::ok()
}

async fn queue_stitch(State(o): State<St>, Json(b): Json<StitchBody>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let upcoming = o.engine.queue_snapshot();
    let plan = orca_core::stitcher::resolve(&o.db, &cfg, &b.r#type, &upcoming, orca_core::automation::now_local(&cfg)).map_err(|e| bad(&e.to_string()))?;
    let item = orca_core::stitcher::queue_item(&b.r#type, plan.total_ms(), "Stitcher – přidáno ručně");
    let id = item.id.clone();
    o.engine.queue_push(item, b.position);
    Ok(Json(json!({"ok": true, "id": id, "plan": plan})).into_response())
}

#[derive(Deserialize)]
struct StitchQ { r#type: String }
async fn stitch_preview(State(o): State<St>, Query(q): Query<StitchQ>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let upcoming = o.engine.queue_snapshot();
    let plan = orca_core::stitcher::resolve(&o.db, &cfg, &q.r#type, &upcoming, orca_core::automation::now_local(&cfg)).map_err(|e| bad(&e.to_string()))?;
    Ok(Json(plan).into_response())
}

// ---------------------------------------------------------------------------------------------
// Studio – nahrávání moderátorských vstupů

async fn mic_status() -> ApiResult { Ok(Json(orca_core::mic::status()).into_response()) }
async fn rec_devices() -> ApiResult {
    Ok(Json(tokio::task::spawn_blocking(orca_core::recorder::list_input_devices).await?).into_response())
}
async fn rec_status(State(o): State<St>) -> ApiResult { Ok(Json(o.recorder.status()).into_response()) }

#[derive(Deserialize)]
struct RecStart { device: Option<String>, kind: Option<String>, category: Option<String>, title: Option<String> }
async fn rec_start(State(o): State<St>, Json(b): Json<RecStart>) -> ApiResult {
    let rc = o.cfg.read().recorder.clone();
    let kind = match b.kind.as_deref() { Some(k) if !k.is_empty() => kind_of(Some(k))?, _ => rc.default_kind };
    let category = b.category.unwrap_or(rc.default_category);
    let title = b.title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Studio".into());
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    let dir = o.library.target_dir(kind, &category);
    // během nahrávání soubor nese tečku na začátku, aby ho sledování složek nebralo
    let path = dir.join(format!(".rec {} - {stamp}.wav", sanitize(&title)));
    let device = b.device.unwrap_or(rc.input_device);
    let rec = o.recorder.clone();
    let (k, c, t) = (kind.as_str().to_string(), category.clone(), title.clone());
    blocking(move || rec.start(&device, &path, rc.mono, &k, &c, &t)).await.map_err(|e| bad(&e.1))?;
    Ok(Json(o.recorder.status()).into_response())
}

#[derive(Deserialize)]
struct RecStop { discard: Option<bool> }
async fn rec_stop(State(o): State<St>, body: Option<Json<RecStop>>) -> ApiResult {
    let discard = body.map(|b| b.discard.unwrap_or(false)).unwrap_or(false);
    let st = o.recorder.status();
    let rec = o.recorder.clone();
    let lib = o.library.clone();
    let db = o.db.clone();
    let track = blocking(move || {
        let Some((tmp, category, _ms)) = rec.stop(discard)? else { return Ok(None) };
        let name = tmp.file_name().map(|n| n.to_string_lossy().trim_start_matches(".rec ").to_string()).unwrap_or_else(|| "nahravka.wav".into());
        let dest = tmp.with_file_name(name);
        std::fs::rename(&tmp, &dest)?;
        let t = lib.register_file(&dest, &category)?;
        if let Some(t) = &t {
            // název z formuláře, ne z názvu souboru
            let when = chrono::Local::now().format("%d.%m.%Y %H:%M");
            db.patch_track(t.id, &TrackPatch { artist: Some(String::new()), title: Some(format!("{} ({when})", st.title)), ..Default::default() })?;
        }
        Ok(t)
    }).await?;
    match track {
        Some(t) => {
            o.events.emit(orca_core::events::Event::LibraryChanged { added: 1, updated: 0, missing: 0 });
            Ok(Json(o.db.get_track(t.id)?).into_response())
        }
        None => ok(),
    }
}
