//! API: vysílací zvukový processing (presety, revize, nasazení, rollback, náhled A/B, měření, audit).

use crate::{bad, not_found, ok, ApiError, ApiResult, St};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use orca_core::dsp::params::{template, Preset};
use orca_core::processing::{audit_list, PreviewReq, Revision};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<St> {
    Router::new()
        .route("/processing/status", get(status))
        .route("/processing/presets", get(presets).post(preset_save))
        .route("/processing/presets/{id}", delete(preset_delete))
        .route("/processing/presets/{id}/export", get(preset_export))
        .route("/processing/presets/import", post(preset_import))
        .route("/processing/revisions", get(revisions))
        .route("/processing/draft", get(draft))
        .route("/processing/deploy", post(deploy))
        .route("/processing/rollback/{id}", post(rollback))
        .route("/processing/block", post(block))
        .route("/processing/preview", post(preview))
        .route("/processing/preview/{file}", get(preview_file))
        .route("/processing/meters/reset", post(meters_reset))
        .route("/processing/templates", get(templates))
        .route("/processing/restart-streams", post(restart_streams))
        .route("/processing/audit", get(audit))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::from)?.map_err(ApiError::from)
}
/// Uživatel pro audit (z hlavičky X-Orca-User; do zavedení uživatelských účtů „operátor“).
fn user(h: &HeaderMap) -> String { h.get("x-orca-user").and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty()).unwrap_or("operátor").to_string() }

async fn status(State(o): State<St>) -> ApiResult { Ok(Json(o.processing.status()).into_response()) }
async fn presets(State(o): State<St>) -> ApiResult { Ok(Json(o.processing.presets()?).into_response()) }
async fn preset_save(State(o): State<St>, h: HeaderMap, Json(p): Json<Preset>) -> ApiResult { let u = user(&h); let pr = o.processing.clone(); Ok(Json(blocking(move || pr.save_preset(p, &u)).await.map_err(|e| bad(&e.1))?).into_response()) }
async fn preset_delete(State(o): State<St>, h: HeaderMap, Path(id): Path<String>) -> ApiResult { o.processing.delete_preset(&id, &user(&h)).map_err(|e| bad(&e.to_string()))?; ok() }
async fn preset_export(State(o): State<St>, Path(id): Path<String>) -> ApiResult {
    let s = o.processing.export_preset(&id).map_err(|e| not_found(&e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "application/json".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"orca-preset-{id}.json\""))], s).into_response())
}
async fn preset_import(State(o): State<St>, h: HeaderMap, body: Bytes) -> ApiResult {
    let (p, warn) = o.processing.import_preset(&String::from_utf8_lossy(&body), &user(&h)).map_err(|e| bad(&e.to_string()))?;
    Ok(Json(json!({"preset": p, "warnings": warn})).into_response())
}
async fn revisions(State(o): State<St>) -> ApiResult { Ok(Json(o.processing.revisions(100)?).into_response()) }
async fn draft(State(o): State<St>) -> ApiResult { Ok(Json(o.processing.default_revision()).into_response()) }

#[derive(Deserialize)]
struct DeployReq { revision: Revision, #[serde(default)] note: String, base_id: Option<i64> }
async fn deploy(State(o): State<St>, h: HeaderMap, Json(r): Json<DeployReq>) -> ApiResult {
    let u = user(&h); let p = o.processing.clone();
    let id = blocking(move || p.deploy(r.revision, &u, &r.note, r.base_id)).await.map_err(|e| ApiError(axum::http::StatusCode::CONFLICT, e.1))?;
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
async fn rollback(State(o): State<St>, h: HeaderMap, Path(id): Path<i64>) -> ApiResult {
    let u = user(&h); let p = o.processing.clone();
    let nid = blocking(move || p.rollback(id, &u)).await.map_err(|e| bad(&e.1))?;
    Ok(Json(json!({"ok": true, "id": nid})).into_response())
}
#[derive(Deserialize)]
struct BlockReq { hold: bool }
async fn block(State(o): State<St>, h: HeaderMap, Json(b): Json<BlockReq>) -> ApiResult { let u = user(&h); let p = o.processing.clone(); blocking(move || p.set_manual_hold(b.hold, &u)).await?; ok() }

async fn preview(State(o): State<St>, Json(r): Json<PreviewReq>) -> ApiResult {
    let p = o.processing.clone();
    Ok(Json(blocking(move || p.preview(&r)).await.map_err(|e| bad(&e.1))?).into_response())
}
async fn preview_file(State(o): State<St>, Path(file): Path<String>) -> ApiResult {
    let p = o.processing.preview_path(&file).ok_or_else(|| not_found("náhled"))?;
    let b = tokio::fs::read(p).await?;
    Ok(([(header::CONTENT_TYPE, "audio/wav"), (header::CACHE_CONTROL, "no-cache")], b).into_response())
}
async fn meters_reset() -> ApiResult { orca_core::dsp::meter_reset_gen().fetch_add(1, std::sync::atomic::Ordering::SeqCst); orca_core::dsp::meters().lock().clear(); ok() }
#[derive(Deserialize)]
struct TplQ { target: Option<String> }
async fn templates(Query(q): Query<TplQ>) -> ApiResult {
    let t = q.target.unwrap_or_default();
    Ok(Json(json!({ "dab": template("dab", &t), "icecast": template("icecast", &t), "pcm": template("pcm", &t), "pcm23": template("pcm23", &t) })).into_response())
}
async fn restart_streams(State(o): State<St>, h: HeaderMap) -> ApiResult {
    o.encoders.stop_all(); o.encoders.autostart();
    orca_core::processing::audit(&o.db, &user(&h), "restart_streams", "aktivace čekajících změn výstupních větví");
    ok()
}
#[derive(Deserialize)]
struct LimitQ { limit: Option<i64> }
async fn audit(State(o): State<St>, Query(q): Query<LimitQ>) -> ApiResult { Ok(Json(audit_list(&o.db, q.limit.unwrap_or(200))?).into_response()) }
