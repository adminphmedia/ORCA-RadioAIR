//! Lokální HTTP/WebSocket API ORCA RadioAIR. Používá ho UI (Lovable/React) i vzdálené panely.

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use orca_core::automation::Mode;
use orca_core::config::Config;
use orca_core::db::{TrackFilter, TrackPatch};
use orca_core::engine::Cmd;
use orca_core::model::QueueItem;
use orca_core::Orca;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

pub(crate) type St = Arc<Orca>;
pub(crate) type ApiResult = Result<Response, ApiError>;

mod airlog_api;
mod podcast_api;
mod vt_api;
mod ext;
mod general_api;
mod import_api;
mod selector_api;
mod plan;
mod proc_api;

pub struct ApiError(pub(crate) StatusCode, pub(crate) String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response { (self.0, Json(json!({"error": self.1}))).into_response() }
}
impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self { ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) }
}
pub(crate) fn not_found(what: &str) -> ApiError { ApiError(StatusCode::NOT_FOUND, format!("{what} nenalezeno")) }
pub(crate) fn bad(msg: &str) -> ApiError { ApiError(StatusCode::BAD_REQUEST, msg.into()) }
pub(crate) fn ok() -> ApiResult { Ok(Json(json!({"ok": true})).into_response()) }

const FALLBACK_UI: &str = include_str!("fallback_ui.html");

/// Složka s webovým UI.
pub(crate) fn ui_dir(orca: &St) -> Option<std::path::PathBuf> {
    let c = orca.cfg.read();
    if !c.api.ui_dir.as_os_str().is_empty() { Some(c.api.ui_dir.clone()) }
    else if let Some(d) = std::env::var_os("ORCA_UI_DIR") { Some(std::path::PathBuf::from(d)) } else {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ui"))).filter(|d| d.join("index.html").is_file())
    }
}

/// Voice tracky z domova – samostatný port (spouští main serveru i desktopová aplikace).
pub async fn run_remote_vt(orca: St) { vt_api::run_remote(orca).await }

pub fn router(orca: St) -> Router {
    let api = Router::new()
        .route("/info", get(info))
        .route("/status", get(status))
        .route("/ws", get(ws_handler))
        // transport
        .route("/transport/{action}", post(transport))
        .route("/mode", post(set_mode))
        // fronta / decky
        .route("/meters/history", get(meter_history))
        .route("/queue", get(queue_get).post(queue_add).delete(queue_clear))
        .route("/queue/{id}", axum::routing::delete(queue_remove))
        .route("/queue/move", post(queue_move))
        .route("/queue/note", post(queue_note))
        .route("/ai/presets", get(ai_presets))
        .route("/ai/models", post(ai_models))
        .route("/ai/track-notes/{id}", post(ai_track_notes))
        .route("/ai/batch", get(ai_batch_status).post(ai_batch_start))
        .route("/ai/batch/cancel", post(ai_batch_cancel))
        .route("/library/expired", get(expired_list).post(expired_delete))
        .route("/queue/preview-next-hour", get(queue_preview))
        .route("/queue/{id}/play-now", post(queue_play_now))
        // knihovna
        .route("/tracks", get(tracks_list))
        .route("/tracks/{id}", get(track_get).patch(track_patch))
        .route("/tracks/{id}/cover", get(track_cover))
        .route("/tracks/{id}/reanalyze", post(track_reanalyze))
        .route("/tracks/{id}/play-now", post(track_play_now))
        .route("/instant/{id}", post(instant_play))
        .route("/instant/stop", post(instant_stop))
        .route("/categories", get(categories))
        .route("/library/rescan", post(rescan))
        .route("/library/stats", get(lib_stats))
        .route("/auto/reshuffle", post(reshuffle))
        // log a reporty
        .route("/log", get(log_get))
        .route("/reports/broadcast.csv", get(report_csv))
        // nastavení
        .route("/config", get(config_get).put(config_put))
        .route("/settings/ui", axum::routing::put(ui_settings))
        .route("/audio/devices", get(audio_devices))
        .route("/encoders", get(encoders_get))
        .route("/encoders/{id}/{action}", post(encoder_action))
        .merge(airlog_api::routes())
        .merge(podcast_api::routes())
        .merge(vt_api::routes())
        .merge(ext::routes())
        .merge(plan::routes())
        .merge(proc_api::routes())
        .merge(general_api::routes())
        .merge(import_api::routes())
        .merge(selector_api::routes())
        .layer(middleware::from_fn_with_state(orca.clone(), general_api::perms))
        .layer(middleware::from_fn_with_state(orca.clone(), auth));

    let ui_dir = ui_dir(&orca);
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE, Method::OPTIONS])
        .allow_headers(Any)
        .allow_private_network(true); // Chrome: přístup z https stránky (Lovable náhled) na localhost

    let mut app = Router::new().nest("/api", api).merge(plan::public_routes());
    app = match ui_dir {
        Some(d) if d.join("index.html").is_file() => {
            log::info!("UI ze složky {}", d.display());
            // /assets: soubory s hashem v názvu – chybějící = 404 (ne index.html), jinak by stará stránka po aktualizaci
            // dostala HTML místo skriptu a spadla; ostatní cesty = SPA (index.html)
            app.nest_service("/assets", ServeDir::new(d.join("assets")))
                .fallback_service(ServeDir::new(&d).fallback(ServeFile::new(d.join("index.html"))))
                .layer(middleware::from_fn(ui_cache_headers))
        }
        _ => app.fallback(|| async { Html(FALLBACK_UI) }),
    };
    app.layer(cors).with_state(orca)
}

/// Cache pro UI: index.html a ostatní soubory vždy znovu ověřit (po aktualizaci aplikace se hned načte nová verze),
/// soubory v /assets mají v názvu hash → mohou se cachovat natrvalo.
async fn ui_cache_headers(req: axum::extract::Request, next: middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    if path.starts_with("/api") || res.headers().contains_key(header::CACHE_CONTROL) { return res; }
    let v = if path.starts_with("/assets/") && res.status().is_success() { "public, max-age=31536000, immutable" } else { "no-cache, must-revalidate" };
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
    res
}

async fn auth(State(o): State<St>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let token = o.cfg.read().api.token.clone();
    if token.is_empty() || req.method() == Method::OPTIONS { return next.run(req).await; }
    let hdr_ok = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(|v| v == format!("Bearer {token}")).unwrap_or(false);
    let q_ok = req.uri().query().map(|q| q.split('&').any(|kv| kv == format!("token={token}"))).unwrap_or(false);
    if hdr_ok || q_ok { next.run(req).await } else { (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response() }
}

fn info_json(o: &Orca) -> Value {
    let c = o.cfg.read();
    json!({
        "app": "ORCA RadioAIR", "version": orca_core::VERSION, "station": c.station.name,
        "language": c.station.language, "theme": c.station.theme,
        "platform": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "data_dir": o.data_dir, "sample_rate": o.engine.sample_rate(),
    })
}

fn leveler_json(o: &Orca) -> Value {
    let (g, lu) = o.engine.leveler_state();
    let c = o.cfg.read();
    json!({"gain_db": (g * 10.0).round() / 10.0, "input_lufs": (lu * 10.0).round() / 10.0, "enabled": c.audio.leveler.enabled, "target_lufs": c.audio.target_lufs})
}

fn status_json(o: &Orca) -> Value {
    let (l, r) = o.engine.meters();
    json!({
        "type": "status",
        "time": orca_core::automation::now_local(&o.cfg.read()).to_rfc3339(),
        "mode": o.automation.mode(),
        "engine": o.engine.snapshot(),
        "meters": {"l": l, "r": r},
        "leveler": leveler_json(o),
        "encoders": o.encoders.statuses(),
        "queue_length": o.engine.queue.lock().len(),
    })
}

async fn info(State(o): State<St>) -> Json<Value> { Json(info_json(&o)) }
async fn status(State(o): State<St>) -> Json<Value> { Json(status_json(&o)) }

async fn transport(State(o): State<St>, Path(action): Path<String>) -> ApiResult {
    let cmd = match action.as_str() {
        "play" => Cmd::Play,
        "stop" => Cmd::Stop,
        "next" => Cmd::Next,
        "pause" => Cmd::Pause,
        "stop-after-current" => Cmd::StopAfterCurrent(true),
        "cancel-stop-after-current" => Cmd::StopAfterCurrent(false),
        "loop" => Cmd::Loop(true),
        "cancel-loop" => Cmd::Loop(false),
        "restart" => Cmd::Restart,
        "talkover" => Cmd::TalkOver(true),
        "cancel-talkover" => Cmd::TalkOver(false),
        "continue" => { o.engine.live_continue(); return ok(); }
        _ => return Err(bad("neznámá akce")),
    };
    o.engine.send(cmd);
    ok()
}

#[derive(Deserialize)]
struct ModeBody { mode: Mode }
async fn set_mode(State(o): State<St>, Json(b): Json<ModeBody>) -> ApiResult { o.automation.set_mode(b.mode); ok() }

async fn queue_get(State(o): State<St>) -> Json<Vec<QueueItem>> { Json(o.automation.queue_with_eta()) }

#[derive(Deserialize)]
struct QueueAdd { track_id: i64, position: Option<usize>, overlay_position: Option<String> }
async fn queue_add(State(o): State<St>, Json(b): Json<QueueAdd>) -> ApiResult {
    let t = o.db.get_track(b.track_id)?.ok_or_else(|| not_found("skladba"))?;
    let mut item = QueueItem::new(t, "manual", "přidáno ručně");
    if item.track.kind().is_overlay_capable() { item.position = b.overlay_position.clone(); }
    let id = item.id.clone();
    o.engine.queue_push(item, b.position);
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
#[derive(Deserialize)]
struct QueueNote { #[serde(default)] memo_id: Option<i64>, #[serde(default)] title: String, #[serde(default)] body: String, #[serde(default)] color: String, #[serde(default)] position: Option<usize> }
/// Poznámka (memo) do fronty – nehraje, moderátor si ji otevře a přečte.
async fn queue_note(State(o): State<St>, Json(b): Json<QueueNote>) -> ApiResult {
    let mut n = orca_core::model::Note { memo_id: b.memo_id, title: b.title, body: b.body, color: b.color };
    if let Some(id) = b.memo_id {
        let m = orca_core::catalog::list_memos(&o.db, None, None)?.into_iter().find(|m| m.id == id).ok_or_else(|| not_found("poznámka"))?;
        if n.title.is_empty() { n.title = m.title; }
        if n.body.is_empty() { n.body = m.body; }
        if n.color.is_empty() { n.color = m.color; }
    }
    if n.title.trim().is_empty() && n.body.trim().is_empty() { return Err(bad("prázdná poznámka")); }
    let item = QueueItem::note(n, "manual");
    let id = item.id.clone();
    o.engine.queue_push(item, b.position);
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
async fn queue_remove(State(o): State<St>, Path(id): Path<String>) -> ApiResult {
    if o.engine.queue_remove(&id) { ok() } else { Err(not_found("položka")) }
}
async fn queue_clear(State(o): State<St>) -> ApiResult { o.engine.queue_clear(); ok() }
#[derive(Deserialize)]
struct QueueMove { id: String, to: usize }
async fn queue_move(State(o): State<St>, Json(b): Json<QueueMove>) -> ApiResult {
    if o.engine.queue_move(&b.id, b.to) { ok() } else { Err(not_found("položka")) }
}

async fn tracks_list(State(o): State<St>, Query(f): Query<TrackFilter>) -> ApiResult {
    let (items, total) = o.db.list_tracks(&f)?;
    Ok(Json(json!({"items": items, "total": total})).into_response())
}
async fn track_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?).into_response())
}
async fn track_patch(State(o): State<St>, Path(id): Path<i64>, Json(p): Json<TrackPatch>) -> ApiResult {
    o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    o.db.patch_track(id, &p)?;
    Ok(Json(o.db.get_track(id)?).into_response())
}
async fn track_cover(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    let res = tokio::task::spawn_blocking(move || orca_core::library::cover_for(&t)).await?;
    match res {
        Some((data, mime)) => Ok(([(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/jpeg"))), (header::CACHE_CONTROL, HeaderValue::from_static("max-age=3600"))], Body::from(data)).into_response()),
        None => Err(not_found("obal")),
    }
}
#[derive(Deserialize)]
struct Reanalyze { #[serde(default)] reset_manual: bool }
async fn track_reanalyze(State(o): State<St>, Path(id): Path<i64>, body: Option<Json<Reanalyze>>) -> ApiResult {
    o.db.reset_analysis(id, body.map(|b| b.reset_manual).unwrap_or(false))?;
    o.library.queue_analysis(id);
    ok()
}
async fn track_play_now(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    o.engine.queue_push(QueueItem::new(t, "manual", "přehráno okamžitě"), Some(0));
    o.engine.send(Cmd::PlayNow);
    ok()
}
// ---------------------------------------------------------------------------------------------
// AI – informace o skladbách pro moderátora

async fn ai_presets(State(o): State<St>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let p: Vec<_> = orca_core::ai::PRESETS.iter().map(|(id, name, text)| json!({ "id": id, "name": name, "text": text })).collect();
    Ok(Json(json!({ "presets": p, "system_prompt": orca_core::ai::system_prompt(&cfg) })).into_response())
}
/// Seznam modelů – s nastavením z formuláře (ještě neuloženým); maska klíče = uložený klíč.
async fn ai_models(State(o): State<St>, Json(mut a): Json<orca_core::ai::AiConfig>) -> ApiResult {
    if a.api_key.starts_with(KEY_MASK) { a.api_key = o.cfg.read().ai.api_key.clone(); }
    let v = tokio::task::spawn_blocking(move || orca_core::ai::models(&a)).await?.map_err(|e| bad(&e.to_string()))?;
    Ok(Json(v).into_response())
}
/// Vygeneruje návrh textu pro moderátora (neukládá – uloží se v editoru skladby).
async fn ai_track_notes(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    let cfg = o.cfg.read().clone();
    let (text, source) = tokio::task::spawn_blocking(move || orca_core::ai::track_notes(&cfg, &t)).await?.map_err(|e| bad(&e.to_string()))?;
    Ok(Json(json!({ "text": text, "source": source })).into_response())
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct AiBatch { ids: Vec<i64>, overwrite: bool }
async fn ai_batch_start(State(o): State<St>, Json(r): Json<AiBatch>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let s = orca_core::ai::batch_start(o.db.clone(), cfg, r.ids, r.overwrite).map_err(|e| bad(&e.to_string()))?;
    Ok(Json(s).into_response())
}
async fn ai_batch_status() -> ApiResult { Ok(Json(orca_core::ai::batch_status()).into_response()) }
async fn ai_batch_cancel() -> ApiResult { orca_core::ai::batch_cancel(); ok() }

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExpiredReq { ids: Option<Vec<i64>> }
/// Soubory po platnosti (ne hudba).
async fn expired_list(State(o): State<St>) -> ApiResult {
    let lib = o.library.clone();
    Ok(Json(tokio::task::spawn_blocking(move || lib.expired(0)).await??).into_response())
}
/// Přesune soubory po platnosti do koše knihovny (všechny, nebo vybrané).
async fn expired_delete(State(o): State<St>, Json(r): Json<ExpiredReq>) -> ApiResult {
    let lib = o.library.clone();
    let n = tokio::task::spawn_blocking(move || lib.delete_expired(r.ids.as_deref(), 0)).await??;
    Ok(Json(json!({ "deleted": n })).into_response())
}

/// Náhled další hodiny (ruční režim – co by se hrálo, bez zařazení do fronty).
async fn queue_preview(State(o): State<St>) -> ApiResult {
    let a = o.automation.clone();
    let cfg = o.cfg.read().clone();
    let r = tokio::task::spawn_blocking(move || {
        use chrono::Timelike;
        let n = orca_core::automation::now_local(&cfg);
        let next = n.with_minute(0).and_then(|d| d.with_second(0)).unwrap_or(n) + chrono::Duration::hours(1);
        serde_json::json!({ "hour": next.format("%H:00").to_string(), "start": next.to_rfc3339(), "items": a.preview_hour(next) })
    }).await?;
    Ok(Json(r).into_response())
}
/// Přehrát hned položku, která už je v deckách (přesune se na začátek – nezdvojí se).
async fn queue_play_now(State(o): State<St>, Path(id): Path<String>) -> ApiResult {
    if !o.engine.queue_move(&id, 0) { return Err(not_found("položka decků")); }
    o.engine.send(Cmd::PlayNow);
    ok()
}
async fn instant_play(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    o.engine.send(Cmd::Instant(t));
    ok()
}
async fn instant_stop(State(o): State<St>) -> ApiResult { o.engine.send(Cmd::StopInstants); ok() }
async fn categories(State(o): State<St>) -> ApiResult { Ok(Json(o.db.categories()?).into_response()) }
async fn rescan(State(o): State<St>) -> ApiResult { o.library.request_rescan(); ok() }
async fn lib_stats(State(o): State<St>) -> ApiResult { Ok(Json(o.db.stats()?).into_response()) }
async fn reshuffle(State(o): State<St>) -> ApiResult { o.db.reshuffle()?; ok() }

#[derive(Deserialize)]
struct LogQuery { from: Option<String>, to: Option<String>, kind: Option<String>, limit: Option<i64>, lang: Option<String> }

fn range(q: &LogQuery) -> (String, String) {
    let from = q.from.clone().unwrap_or_else(|| orca_core::db::utc_minus_minutes(24 * 60));
    let to = q.to.clone().unwrap_or_else(|| "9999".into());
    (from, to)
}
async fn log_get(State(o): State<St>, Query(q): Query<LogQuery>) -> ApiResult {
    let (from, to) = range(&q);
    Ok(Json(o.db.log_range(&from, &to, q.kind.as_deref(), q.limit.unwrap_or(500))?).into_response())
}
async fn report_csv(State(o): State<St>, Query(q): Query<LogQuery>) -> ApiResult {
    let (from, to) = range(&q);
    let lang = q.lang.clone().unwrap_or_else(|| o.cfg.read().station.language.clone());
    let csv = orca_core::reports::broadcast_csv(&o.db, &from, &to, q.kind.as_deref().or(Some("song")), &lang)?;
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"orca-vysilani.csv\"")], csv).into_response())
}

/// Klíč AI se do prohlížeče neposílá celý – jen maska s posledními znaky.
const KEY_MASK: &str = "••••••";
fn masked(mut c: Config) -> Config {
    let k = c.ai.api_key.trim().to_string();
    if !k.is_empty() { c.ai.api_key = format!("{KEY_MASK}{}", k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>()); }
    c
}
async fn config_get(State(o): State<St>) -> Json<Config> { Json(masked(o.cfg.read().clone())) }
async fn config_put(State(o): State<St>, Json(mut c): Json<Config>) -> ApiResult {
    if c.ai.api_key.starts_with(KEY_MASK) { c.ai.api_key = o.cfg.read().ai.api_key.clone(); }
    let o2 = o.clone();
    tokio::task::spawn_blocking(move || o2.apply_config(c)).await??;
    Ok(Json(masked(o.cfg.read().clone())).into_response())
}
#[derive(Deserialize)]
struct UiSettings { language: Option<String>, theme: Option<String> }
async fn ui_settings(State(o): State<St>, Json(b): Json<UiSettings>) -> ApiResult {
    let mut c = o.cfg.read().clone();
    if let Some(l) = b.language { if l != "cs" && l != "en" { return Err(bad("language: cs|en")); } c.station.language = l; }
    if let Some(t) = b.theme { if !["dark", "light", "system"].contains(&t.as_str()) { return Err(bad("theme: dark|light|system")); } c.station.theme = t; }
    let o2 = o.clone();
    tokio::task::spawn_blocking(move || o2.apply_config(c)).await??;
    Ok(Json(info_json(&o)).into_response())
}
async fn audio_devices() -> ApiResult {
    Ok(Json(tokio::task::spawn_blocking(orca_core::audio_out::list_output_devices).await?).into_response())
}
async fn encoders_get(State(o): State<St>) -> ApiResult { Ok(Json(o.encoders.statuses()).into_response()) }
async fn encoder_action(State(o): State<St>, Path((id, action)): Path<(String, String)>) -> ApiResult {
    match action.as_str() {
        "start" => { if !o.encoders.start(&id) { return Err(not_found("encoder")); } }
        "stop" => o.encoders.stop(&id),
        _ => return Err(bad("akce: start|stop")),
    }
    ok()
}

// ---------------------------------------------------------------------------------------------
// WebSocket: události v reálném čase + stav 5× za sekundu

#[derive(Deserialize)]
struct MeterQ { #[serde(default)] seconds: Option<f32> }
/// Historie úrovní výstupu (pro graf hlasitosti v čase po otevření okna).
async fn meter_history(State(o): State<St>, Query(q): Query<MeterQ>) -> ApiResult {
    let n = ((q.seconds.unwrap_or(60.0).clamp(1.0, 120.0)) * 100.0) as usize;
    let (seq, bms, blocks) = o.engine.meter_blocks(0, n);
    Ok(Json(json!({"seq": seq, "block_ms": bms, "blocks": blocks})).into_response())
}

async fn ws_handler(State(o): State<St>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_session(o, socket))
}

async fn ws_session(o: St, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let (etx, mut erx) = tokio::sync::mpsc::channel::<String>(512);
    let sub = o.events.subscribe();
    let fwd = std::thread::spawn(move || {
        while let Ok(ev) = sub.recv() {
            if let Ok(s) = serde_json::to_string(&ev) { if etx.blocking_send(s).is_err() { break; } }
        }
    });
    let hello = json!({"type": "hello", "info": info_json(&o)}).to_string();
    if tx.send(Message::Text(hello.into())).await.is_err() { return; }
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    // plynulé měřiče: ~30× za sekundu nové bloky po 10 ms (špička + RMS)
    let mut mtick = tokio::time::interval(Duration::from_millis(33));
    let mut mseq = o.engine.meter_blocks(u64::MAX, 0).0;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let s = status_json(&o).to_string();
                if tx.send(Message::Text(s.into())).await.is_err() { break; }
            }
            _ = mtick.tick() => {
                let (seq, bms, blocks) = o.engine.meter_blocks(mseq, 50);
                if !blocks.is_empty() {
                    mseq = seq;
                    let s = json!({"type": "meters", "seq": seq, "block_ms": bms, "blocks": blocks}).to_string();
                    if tx.send(Message::Text(s.into())).await.is_err() { break; }
                }
            }
            Some(ev) = erx.recv() => {
                if tx.send(Message::Text(ev.into())).await.is_err() { break; }
            }
            msg = rx.next() => {
                match msg { Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break, _ => {} }
            }
        }
    }
    // forward vlákno skončí při příští události (odeslání selže)
    drop(erx);
    drop(fwd);
}
