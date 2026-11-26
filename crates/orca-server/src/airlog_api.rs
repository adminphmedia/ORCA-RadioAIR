//! API: Monitoring – nahrávání vysílání (záznamníky, prohlížení dne, poslech, výřezy, aircheck, zamčení dnů).

use crate::{bad, not_found, ok, ApiError, ApiResult, St};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<St> {
    Router::new()
        .route("/airlog", get(overview))
        .route("/airlog/{id}/day", get(day))
        .route("/airlog/{id}/file/{name}", get(file))
        .route("/airlog/{id}/verify", post(verify))
        .route("/airlog/{id}/keep", post(keep))
        .route("/airlog/{id}/monitor", post(monitor))
        .route("/airlog/{id}/export", post(export))
        .route("/airlog/exports/{name}", get(export_file))
        .route("/airlog/archive", get(archive_status))
        .route("/airlog/archive/run", post(archive_run))
        .route("/airlog/archive/delete", post(archive_delete))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
}

fn parse_dt(s: &str) -> Result<DateTime<Local>, ApiError> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) { return Ok(t.with_timezone(&Local)); }
    for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(s, f) { if let Some(t) = Local.from_local_datetime(&n).earliest() { return Ok(t); } }
    }
    Err(bad("čas ve tvaru RRRR-MM-DDTHH:MM:SS"))
}

async fn overview(State(o): State<St>) -> ApiResult {
    let a = o.airlog.clone();
    let status = blocking(move || Ok(a.status())).await?;
    let cfg = o.cfg.read().airlog.clone();
    let est: Vec<_> = cfg.loggers.iter().map(|l| json!({"id": l.id, "bytes_per_day": orca_core::airlog::bytes_per_day(l)})).collect();
    Ok(Json(json!({ "status": status, "estimates": est })).into_response())
}

#[derive(Deserialize)]
struct DayQ { date: String }
async fn day(State(o): State<St>, Path(id): Path<String>, Query(q): Query<DayQ>) -> ApiResult {
    let d = NaiveDate::parse_from_str(&q.date, "%Y-%m-%d").map_err(|_| bad("datum RRRR-MM-DD"))?;
    let (a, db) = (o.airlog.clone(), o.db.clone());
    let (segs, marks, plays) = blocking(move || {
        let (s, m) = a.day(&id, d)?;
        let d0 = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().unwrap();
        let fmt = |t: DateTime<Local>| t.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let plays = db.log_range(&fmt(d0), &fmt(d0 + chrono::Duration::days(1)), None, 5000)?;
        Ok((s, m, plays))
    }).await?;
    Ok(Json(json!({ "segments": segs, "marks": marks, "plays": plays })).into_response())
}

async fn file(State(o): State<St>, Path((id, name)): Path<(String, String)>, req: axum::extract::Request) -> ApiResult {
    use tower::ServiceExt;
    let p = o.airlog.file_path(&id, &name).map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    let res = tower_http::services::ServeFile::new(p).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(res.into_response())
}

#[derive(Deserialize)]
struct NameB { name: String }
async fn verify(State(o): State<St>, Path(id): Path<String>, Json(b): Json<NameB>) -> ApiResult {
    let a = o.airlog.clone();
    let good = blocking(move || a.verify(&id, &b.name)).await?;
    Ok(Json(json!({ "ok": good })).into_response())
}

#[derive(Deserialize)]
struct KeepB { date: String, keep: bool }
async fn keep(State(o): State<St>, Path(id): Path<String>, Json(b): Json<KeepB>) -> ApiResult {
    o.airlog.set_kept(&id, &b.date, b.keep).map_err(|e| bad(&e.to_string()))?;
    ok()
}

#[derive(Deserialize)]
struct AtB { at: String }
/// Poslech záznamu od daného času přes předposlech (nikdy do vysílání).
async fn monitor(State(o): State<St>, Path(id): Path<String>, Json(b): Json<AtB>) -> ApiResult {
    let at = parse_dt(&b.at)?;
    let o2 = o.clone();
    blocking(move || {
        let (p, off) = o2.airlog.locate(&id, at)?;
        let c = o2.cfg.read().clone();
        o2.monitor.play(&c, None, p, off, 0)
    }).await?;
    ok()
}

#[derive(Deserialize)]
struct ExportB { from: String, to: String, #[serde(default)] format: String, #[serde(default)] aircheck: bool, #[serde(default)] name: String }
async fn export(State(o): State<St>, Path(id): Path<String>, Json(b): Json<ExportB>) -> ApiResult {
    let (from, to) = (parse_dt(&b.from)?, parse_dt(&b.to)?);
    let a = o.airlog.clone();
    let p = blocking(move || a.export(&id, from, to, &b.format, b.aircheck, &b.name, None)).await?;
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    Ok(Json(json!({ "name": name, "path": p.to_string_lossy(), "bytes": bytes, "url": format!("/api/airlog/exports/{}", urlencoding::encode(&name)) })).into_response())
}

async fn export_file(State(o): State<St>, Path(name): Path<String>, req: axum::extract::Request) -> ApiResult {
    use tower::ServiceExt;
    if name.contains('/') || name.contains('\\') || name.contains("..") { return Err(bad("neplatný název")); }
    let p = o.data_dir.join("Exports").join("Logger").join(&name);
    if !p.is_file() { return Err(not_found("výřez")); }
    let mut res = tower_http::services::ServeFile::new(p).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{}", urlencoding::encode(&name))) { res.headers_mut().insert(axum::http::header::CONTENT_DISPOSITION, v); }
    Ok(res)
}

async fn archive_status(State(o): State<St>) -> ApiResult {
    let a = o.airlog.clone();
    let st = blocking(move || Ok(a.archive_status())).await?;
    Ok(Json(json!(st)).into_response())
}

#[derive(Deserialize)]
struct RunB { #[serde(default)] date: Option<String> }
/// „Kopírovat teď“ – běží na pozadí, výsledek přijde jako hláška ve studiu.
async fn archive_run(State(o): State<St>, Json(b): Json<RunB>) -> ApiResult {
    o.airlog.archive_now(b.date).map_err(|e| bad(&e.to_string()))?;
    ok()
}

#[derive(Deserialize)]
struct DelB { logger_id: String, date: String }
/// Smazání dne ze systému – jen pokud je ověřeně v archivu.
async fn archive_delete(State(o): State<St>, Json(b): Json<DelB>) -> ApiResult {
    let a = o.airlog.clone();
    let n = blocking(move || a.archive_delete_local(&b.logger_id, &b.date)).await?;
    Ok(Json(json!({ "ok": true, "deleted": n })).into_response())
}
