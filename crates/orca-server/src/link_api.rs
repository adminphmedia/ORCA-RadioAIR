//! Propojení počítačů (hlavní / záložní / přípravný): samostatný port chráněný společným klíčem
//! + ovládání z UI (stav, stáhnout, odeslat, převzít, vrátit, Telegram).

use crate::{bad, ok, ApiError, ApiResult, St};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use orca_core::link::{self, ManifestItem};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))
}

// ------------------------------------------------------------------ UI (hlavní port, /api)

pub fn routes() -> Router<St> {
    Router::new()
        .route("/link/status", get(status))
        .route("/link/pull", post(pull))
        .route("/link/push", post(push))
        .route("/link/takeover", post(takeover))
        .route("/link/handback", post(handback))
        .route("/link/genkey", post(genkey))
        .route("/link/test", post(test))
        .route("/telegram/test", post(tg_test))
        .route("/telegram/chats", post(tg_chats))
        .route("/netsync", get(ns_status))
        .route("/netsync/test", post(ns_test))
        .route("/netsync/local-break", post(ns_local))
        .route("/netsync/signal", get(ns_signal))
}

async fn ns_status(State(o): State<St>) -> Json<orca_core::netsync::NetSyncStatus> { Json(o.netsync.status.read().clone()) }

#[derive(Deserialize)]
struct NsTest { kind: String, #[serde(default)] len_s: i64 }
/// Centrála: ruční signál (zkouška / ruční začátek a konec bloku).
async fn ns_test(State(o): State<St>, Json(b): Json<NsTest>) -> ApiResult {
    let n = blocking(move || orca_core::netsync::send(&o, &b.kind, &format!("rucne-{}", chrono::Local::now().format("%H%M%S")), b.len_s * 1000)).await?;
    Ok(Json(json!({"ok": true, "message": format!("signál odeslán {n} stanicím")})).into_response())
}
#[derive(Deserialize)]
struct NsLocal { #[serde(default)] len_s: i64 }
/// Region: vyzkoušet místní blok (jako by přišel signál).
async fn ns_local(State(o): State<St>, Json(b): Json<NsLocal>) -> ApiResult {
    let m = blocking(move || orca_core::netsync::local_break(&o, b.len_s * 1000)).await?;
    Ok(Json(json!({"ok": true, "message": m})).into_response())
}
#[derive(Deserialize)]
struct NsSig { key: String, #[serde(rename = "type")] kind: String, #[serde(default)] len: f64 }
/// Region: signál přes HTTP (pro centrály, které nejsou ORCA): /api/netsync/signal?key=…&type=break_start&len=120
async fn ns_signal(State(o): State<St>, Query(q): Query<NsSig>, headers: HeaderMap) -> ApiResult {
    let _ = headers;
    let c = o.cfg.read().netsync.clone();
    if c.role != "regional" { return Err(bad("tato stanice není nastavená jako regionální")); }
    if c.key.is_empty() || q.key != c.key { return Err(ApiError(StatusCode::UNAUTHORIZED, "neplatný klíč".into())); }
    let sig = orca_core::netsync::Signal { kind: q.kind, len_ms: (q.len * 1000.0) as i64, id: format!("http-{}", chrono::Local::now().format("%H%M%S")), ..Default::default() };
    let o2 = o.clone();
    blocking(move || { orca_core::netsync::handle(&o2, &sig, "HTTP"); Ok(()) }).await?;
    ok()
}

async fn status(State(o): State<St>) -> Json<link::LinkStatus> { Json(o.link.status.read().clone()) }

async fn pull(State(o): State<St>) -> ApiResult {
    let r = blocking(move || link::pull(&o, true)).await?;
    Ok(Json(json!({"ok": true, "message": r})).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PushReq { force: bool }
async fn push(State(o): State<St>, Json(b): Json<PushReq>) -> ApiResult {
    let r = blocking(move || link::push(&o, b.force)).await?;
    Ok(Json(json!({"ok": true, "message": r})).into_response())
}

async fn takeover(State(o): State<St>) -> ApiResult {
    if o.cfg.read().link.role != "backup" { return Err(bad("převzít vysílání může jen záložní počítač")); }
    link::takeover(&o, "ruční převzetí obsluhou");
    ok()
}
async fn handback(State(o): State<St>) -> ApiResult {
    if o.cfg.read().link.role != "backup" { return Err(bad("vrátit vysílání může jen záložní počítač")); }
    link::handback(&o, "ruční vrácení obsluhou");
    ok()
}
async fn genkey() -> Json<serde_json::Value> { Json(json!({"key": link::gen_key()})) }

#[derive(Deserialize)]
struct TestReq { peer: String, key: String }
async fn test(Json(b): Json<TestReq>) -> ApiResult {
    let h = blocking(move || {
        let c = link::LinkConfig { peer: b.peer, key: b.key, ..Default::default() };
        link::hello_of(&c)
    }).await?;
    Ok(Json(h).into_response())
}

#[derive(Deserialize)]
struct TgReq { bot_token: String, chat_id: String }
async fn tg_test(State(o): State<St>, Json(b): Json<TgReq>) -> ApiResult {
    let st = o.cfg.read().meta.station_long.clone();
    blocking(move || link::telegram_send(&link::TelegramConfig { enabled: true, bot_token: b.bot_token, chat_id: b.chat_id, min_level: "error".into() }, &format!("🟢 {st}: zkušební zpráva z ORCA RadioAIR – upozornění do Telegramu fungují."))).await?;
    ok()
}
#[derive(Deserialize)]
struct TgChats { bot_token: String }
async fn tg_chats(Json(b): Json<TgChats>) -> ApiResult {
    let v = blocking(move || link::telegram_chats(&b.bot_token)).await?;
    Ok(Json(v.into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>()).into_response())
}

// ------------------------------------------------------------------ port propojení (klíč)

fn peer_router(o: St) -> Router {
    Router::new()
        .route("/link/hello", get(p_hello))
        .route("/link/snapshot", get(p_snapshot))
        .route("/link/config", get(p_config))
        .route("/link/manifest", get(p_manifest))
        .route("/link/file", get(p_file))
        .route("/link/fingerprint", get(p_fingerprint))
        .route("/link/missing", post(p_missing))
        .route("/link/upload", post(p_upload))
        .route("/link/push", post(p_push))
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(o.clone(), key_guard))
        .with_state(o)
}

async fn key_guard(State(o): State<St>, req: Request, next: Next) -> Response {
    let c = o.cfg.read().link.clone();
    if c.role == "single" { return (StatusCode::FORBIDDEN, "propojení je vypnuté").into_response(); }
    if c.key.trim().len() < 8 { return (StatusCode::FORBIDDEN, "na tomto počítači není nastavený klíč propojení").into_response(); }
    let given = req.headers().get("x-orca-link").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct_eq(given.trim().as_bytes(), c.key.trim().as_bytes()) {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        return (StatusCode::UNAUTHORIZED, "neplatný klíč propojení").into_response();
    }
    next.run(req).await
}
fn ct_eq(a: &[u8], b: &[u8]) -> bool { a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 }

async fn p_hello(State(o): State<St>) -> Json<link::Hello> { Json(link::hello(&o)) }

async fn p_snapshot(State(o): State<St>) -> ApiResult {
    let bytes = blocking(move || {
        let dir = o.data_dir.join("Link");
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(format!("snap-{}.db", orca_core::podcast::new_id()));
        o.db.with_conn(|c| c.execute("VACUUM INTO ?1", [p.to_string_lossy().to_string()]))?;
        let b = std::fs::read(&p)?;
        let _ = std::fs::remove_file(&p);
        Ok(b)
    }).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn p_config(State(o): State<St>) -> Json<orca_core::config::Config> { Json(o.cfg.read().clone()) }

async fn p_manifest(State(o): State<St>) -> ApiResult {
    let v = blocking(move || {
        let c = o.cfg.read().clone();
        let rows: Vec<(i64, String)> = o.db.with_conn(|cn| -> anyhow::Result<_> {
            let mut st = cn.prepare("SELECT id, path FROM tracks WHERE missing = 0")?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })?;
        Ok(rows.into_iter().filter_map(|(id, path)| link::local_file(&c, &path).map(|p| ManifestItem { id, size: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0), path })).collect::<Vec<_>>())
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct FileQ { id: i64 }
async fn p_file(State(o): State<St>, Query(q): Query<FileQ>, req: Request) -> ApiResult {
    use tower::ServiceExt;
    let t = o.db.get_track(q.id)?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "skladba neexistuje".into()))?;
    let p = link::local_file(&o.cfg.read(), &t.path).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "soubor tu není".into()))?;
    Ok(tower_http::services::ServeFile::new(p).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.into_response())
}

async fn p_fingerprint(State(o): State<St>) -> ApiResult {
    let fp = blocking(move || o.db.with_conn(|c| link::plan_fingerprint(c, "main"))).await?;
    Ok(fp.into_response())
}

/// Které z poslaných souborů tu chybí (nebo mají jinou velikost).
async fn p_missing(State(o): State<St>, Json(items): Json<Vec<ManifestItem>>) -> Json<Vec<i64>> {
    let c = o.cfg.read().clone();
    Json(items.into_iter().filter(|m| link::local_file(&c, &m.path).map(|p| std::fs::metadata(p).map(|x| x.len() != m.size).unwrap_or(true)).unwrap_or(true)).map(|m| m.id).collect())
}

#[derive(Deserialize)]
struct UpQ { path: String }
async fn p_upload(State(o): State<St>, Query(q): Query<UpQ>, req: Request) -> ApiResult {
    let c = o.cfg.read().clone();
    let dst = match link::local_file(&c, &q.path) { Some(p) => p, None => link::mirror_path(&link::mirror_dir(&c), &q.path) };
    save_body(req, &dst).await?;
    log::info!("propojení: přijat soubor {}", dst.display());
    ok()
}

async fn save_body(req: Request, dst: &PathBuf) -> Result<u64, ApiError> {
    if let Some(p) = dst.parent() { tokio::fs::create_dir_all(p).await?; }
    let tmp = dst.with_extension("orca-part");
    let mut f = tokio::fs::File::create(&tmp).await?;
    let mut s = req.into_body().into_data_stream();
    let mut n = 0u64;
    while let Some(ch) = s.next().await {
        let ch = ch.map_err(|e| bad(&e.to_string()))?;
        n += ch.len() as u64;
        f.write_all(&ch).await?;
    }
    f.flush().await?;
    drop(f);
    tokio::fs::rename(&tmp, dst).await?;
    Ok(n)
}

#[derive(Deserialize)]
struct PushQ { base: String, #[serde(default)] force: bool }
async fn p_push(State(o): State<St>, Query(q): Query<PushQ>, headers: HeaderMap, req: Request) -> ApiResult {
    let _ = headers;
    if o.cfg.read().link.role != "main" { return Err(bad("přípravu lze odeslat jen na hlavní vysílací počítač")); }
    let p = o.data_dir.join("Link").join(format!("prijato-{}.db", orca_core::podcast::new_id()));
    save_body(req, &p).await?;
    let p2 = p.clone();
    let r = blocking(move || {
        let m = link::apply_push(&o, &p2, &q.base, q.force)?;
        let fp = o.db.with_conn(|c| link::plan_fingerprint(c, "main"))?;
        Ok((m, fp))
    }).await;
    let _ = tokio::fs::remove_file(&p).await;
    let (m, fp) = r?;
    Ok(Json(json!({"ok": true, "message": m, "fingerprint": fp})).into_response())
}

/// Spustí hlídání a port propojení (znovu ho otevře při změně nastavení).
pub async fn run(orca: St) {
    link::start(orca.clone());
    orca_core::netsync::start(orca.clone());
    let mut current: Option<(String, tokio::task::JoinHandle<()>)> = None;
    loop {
        let c = orca.cfg.read().link.clone();
        let want = if c.role == "main" || c.role == "backup" { Some(c.bind.clone()) } else { None };
        if want.as_deref() != current.as_ref().map(|x| x.0.as_str()) {
            if let Some((_, h)) = current.take() { h.abort(); log::info!("propojení: port zavřen"); }
            if let Some(bind) = want {
                match tokio::net::TcpListener::bind(&bind).await {
                    Ok(l) => {
                        log::info!("propojení počítačů: port {bind}");
                        let app = peer_router(orca.clone());
                        current = Some((bind, tokio::spawn(async move { let _ = axum::serve(l, app).await; })));
                    }
                    Err(e) => { orca.events.alert("error", "link_port", format!("Propojení počítačů: port {bind} nelze otevřít ({e})")); current = Some((bind, tokio::spawn(async {}))); }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
