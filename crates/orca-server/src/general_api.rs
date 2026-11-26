//! API: obecná nastavení (zálohy, údržba, integrita, e-mail), uživatelé a přihlášení, předposlech.

use crate::{bad, ok, ApiError, ApiResult, St};
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use orca_core::general;
use orca_core::model::TrackKind;
use orca_core::users::{required_perm, User, PERMS};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<St> {
    Router::new()
        .route("/general/status", get(status))
        .route("/update/status", get(update_status))
        .route("/update/check", post(update_check))
        .route("/update/install", post(update_install))
        .route("/general/backup", post(backup))
        .route("/general/backups", get(backups))
        .route("/general/integrity", post(integrity))
        .route("/general/maintenance", post(maintenance))
        .route("/general/reset-library", post(reset_library))
        .route("/general/rewrite-paths", post(rewrite_paths))
        .route("/general/test-email", post(test_email))
        .route("/general/open-editor/{id}", post(open_editor))
        .route("/users", get(users).post(user_save))
        .route("/users/{id}", delete(user_delete))
        .route("/session", get(session))
        .route("/session/login", post(login))
        .route("/session/logout", post(logout))
        .route("/gpio/status", get(gpio_status))
        .route("/gpio/action", post(gpio_action))
        .route("/gpio/trigger", post(gpio_trigger))
        .route("/queue/live", post(queue_live))
        .route("/monitor/play", post(monitor_play))
        .route("/monitor/stop", post(monitor_stop))
        .route("/monitor/status", get(monitor_status))
        .route("/monitor/route", get(monitor_route))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::from)?.map_err(ApiError::from)
}

pub(crate) fn session_token(req_headers: &axum::http::HeaderMap, query: Option<&str>) -> Option<String> {
    req_headers.get("x-orca-session").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| query.and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("session=").map(|s| s.to_string()))))
        .filter(|s| !s.is_empty())
}

/// Middleware oprávnění. Bez uživatelů nebo bez povinného přihlášení je vše povoleno
/// (přihlášený uživatel je ale vždy omezen svou rolí). Čtení (GET) je povoleno vždy kromě správy uživatelů.
pub async fn perms(State(o): State<St>, mut req: Request, next: Next) -> Response {
    let (method, path) = (req.method().as_str().to_string(), req.uri().path().to_string());
    let Some(need) = required_perm(&method, &path) else { return next.run(req).await };
    let (require, auto) = { let c = o.cfg.read(); (c.general.require_login, c.general.auto_logout_min) };
    let token = session_token(req.headers(), req.uri().query());
    let user = token.as_deref().and_then(|t| o.users.touch(t, auto));
    let has_users = { let o2 = o.clone(); tokio::task::spawn_blocking(move || o2.users.any(&o2.db)).await.unwrap_or(false) };
    match user {
        Some(u) => {
            if !u.effective().iter().any(|p| p == need) {
                return (StatusCode::FORBIDDEN, Json(json!({"error": format!("uživatel {} nemá oprávnění „{need}“", u.name), "perm": need}))).into_response();
            }
            if let Ok(v) = axum::http::HeaderValue::from_str(&u.name) { req.headers_mut().insert("x-orca-user", v); }
            next.run(req).await
        }
        None if require && has_users => (StatusCode::UNAUTHORIZED, Json(json!({"error": "přihlaste se", "login": true}))).into_response(),
        // správu uživatelů bez přihlášení dovolíme jen dokud žádný uživatel neexistuje (první nastavení)
        None if need == "users" && has_users && method != "GET" => (StatusCode::UNAUTHORIZED, Json(json!({"error": "přihlaste se jako administrátor", "login": true}))).into_response(),
        None => next.run(req).await,
    }
}

async fn status(State(o): State<St>) -> ApiResult {
    let c = o.cfg.read().clone();
    let silence = o.general.silence_since.lock().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    Ok(Json(json!({
        "silence_s": silence,
        "last_backup": *o.general.last_backup.read(),
        "email": *o.general.email_status.read(),
        "backup_dir": general::backup_dir(&c, &o.data_dir),
        "data_dir": o.data_dir,
        "db_bytes": std::fs::metadata(o.data_dir.join("orca.db")).map(|m| m.len()).unwrap_or(0),
        "perms": PERMS,
    })).into_response())
}

async fn backup(State(o): State<St>) -> ApiResult {
    let p = blocking(move || { let c = o.cfg.read().clone(); let p = general::backup_now(&o.db, &c, &o.data_dir)?; *o.general.last_backup.write() = Some(p.clone()); Ok(p) }).await?;
    Ok(Json(json!({"ok": true, "path": p})).into_response())
}

async fn update_status(State(o): State<St>) -> ApiResult {
    let mut s = o.update_state.lock().clone();
    s.supported = o.update_hook.read().is_some();
    Ok(Json(json!(s)).into_response())
}

async fn update_check(State(o): State<St>) -> ApiResult {
    let s = blocking(move || Ok(o.update_check())).await?;
    Ok(Json(json!(s)).into_response())
}

async fn update_install(State(o): State<St>) -> ApiResult {
    if o.update_hook.read().is_none() { return Err(bad("Aktualizace jsou dostupné jen v desktopové aplikaci ORCA RadioAIR.")); }
    if o.update_state.lock().available.is_none() { return Err(bad("Není k dispozici žádná nová verze – nejdřív zkontrolujte aktualizace.")); }
    // běží na pozadí; průběh hlásí /update/status, po instalaci se aplikace restartuje
    std::thread::Builder::new().name("orca-update".into()).spawn(move || { if let Err(e) = o.update_install() { log::error!("aktualizace: {e}"); } }).map_err(|e| bad(&e.to_string()))?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn backups(State(o): State<St>) -> ApiResult {
    let c = o.cfg.read().clone();
    let dir = general::backup_dir(&c, &o.data_dir);
    let mut v: Vec<_> = std::fs::read_dir(&dir).map(|r| r.flatten().filter_map(|e| {
        let n = e.file_name().to_string_lossy().to_string();
        if !(n.starts_with("orca_") && n.ends_with(".db")) { return None; }
        let m = e.metadata().ok()?;
        Some(json!({"name": n, "path": e.path(), "bytes": m.len()}))
    }).collect()).unwrap_or_default();
    v.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
    Ok(Json(json!({"dir": dir, "items": v})).into_response())
}

#[derive(Deserialize)]
struct IntegrityReq { #[serde(default)] action: String }
async fn integrity(State(o): State<St>, Json(r): Json<IntegrityReq>) -> ApiResult {
    let rep = blocking(move || { let c = o.cfg.read().clone(); general::integrity_check(&o.db, &c, &o.data_dir, &r.action) }).await?;
    Ok(Json(json!(rep)).into_response())
}

#[derive(Deserialize)]
struct MaintReq { #[serde(default)] days: u32, #[serde(default)] vacuum: bool }
async fn maintenance(State(o): State<St>, Json(r): Json<MaintReq>) -> ApiResult {
    let v = blocking(move || general::maintenance(&o.db, r.days, r.vacuum)).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct ResetReq { kinds: Vec<TrackKind>, confirm: String }
async fn reset_library(State(o): State<St>, Json(r): Json<ResetReq>) -> ApiResult {
    if r.confirm != "SMAZAT" && r.confirm != "DELETE" { return Err(bad("potvrďte napsáním SMAZAT")); }
    let n = blocking(move || { let _ = general::backup_now(&o.db, &o.cfg.read().clone(), &o.data_dir); general::reset_libraries(&o.db, &r.kinds) }).await?;
    Ok(Json(json!({"ok": true, "deleted": n})).into_response())
}

#[derive(Deserialize)]
struct RewriteReq { from: String, to: String }
async fn rewrite_paths(State(o): State<St>, Json(r): Json<RewriteReq>) -> ApiResult {
    let n = blocking(move || general::rewrite_paths(&o.db, &r.from, &r.to)).await?;
    Ok(Json(json!({"ok": true, "updated": n})).into_response())
}

async fn test_email(State(o): State<St>) -> ApiResult {
    let c = o.cfg.read().clone();
    let st = c.meta.station_long.clone();
    blocking(move || general::send_email(&c.general.email, &format!("[{st}] Test upozornění"), "Toto je zkušební e-mail z ORCA RadioAIR. Upozornění fungují.")).await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e.1))?;
    ok()
}

async fn open_editor(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| crate::not_found("skladba"))?;
    let c = o.cfg.read().clone();
    let path = general::map_path(&c, &t.path);
    let ed = c.general.editor_path.clone();
    let r = if ed.is_empty() {
        if cfg!(target_os = "macos") { std::process::Command::new("open").arg(&path).spawn() }
        else if cfg!(windows) { std::process::Command::new("cmd").args(["/C", "start", "", &path]).spawn() }
        else { std::process::Command::new("xdg-open").arg(&path).spawn() }
    } else if cfg!(target_os = "macos") && ed.ends_with(".app") {
        std::process::Command::new("open").args(["-a", &ed, &path]).spawn()
    } else { std::process::Command::new(&ed).arg(&path).spawn() };
    r.map_err(|e| bad(&format!("editor nelze spustit: {e}")))?;
    ok()
}

async fn users(State(o): State<St>) -> ApiResult {
    let v = blocking(move || o.users.list(&o.db)).await?;
    Ok(Json(json!({"items": v, "perms": PERMS})).into_response())
}

#[derive(Deserialize)]
struct UserReq { #[serde(flatten)] user: User, #[serde(default)] pin: Option<String> }
async fn user_save(State(o): State<St>, Json(r): Json<UserReq>) -> ApiResult {
    let id = blocking(move || o.users.save(&o.db, &r.user, r.pin.as_deref())).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.1))?;
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}

async fn user_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    blocking(move || o.users.delete(&o.db, id)).await.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.1))?;
    ok()
}

async fn session(State(o): State<St>, req: Request) -> ApiResult {
    let token = session_token(req.headers(), req.uri().query());
    let (require, auto) = { let c = o.cfg.read(); (c.general.require_login, c.general.auto_logout_min) };
    let user = token.as_deref().and_then(|t| o.users.touch(t, auto));
    let o2 = o.clone();
    let has_users = tokio::task::spawn_blocking(move || o2.users.any(&o2.db)).await.unwrap_or(false);
    let names: Vec<String> = if has_users { let o3 = o.clone(); tokio::task::spawn_blocking(move || o3.users.list(&o3.db).unwrap_or_default()).await.unwrap_or_default().into_iter().filter(|u| u.enabled).map(|u| u.name).collect() } else { vec![] };
    Ok(Json(json!({
        "require_login": require && has_users, "has_users": has_users, "auto_logout_min": auto, "users": names,
        "user": user.as_ref(), "perms": user.as_ref().map(|u| u.effective()).unwrap_or_else(|| if require && has_users { vec![] } else { PERMS.iter().map(|s| s.to_string()).collect() }),
    })).into_response())
}

#[derive(Deserialize)]
struct LoginReq { name: String, #[serde(default)] pin: String }
async fn login(State(o): State<St>, Json(r): Json<LoginReq>) -> ApiResult {
    let o2 = o.clone();
    let (token, user) = blocking(move || o2.users.login(&o2.db, &r.name, &r.pin)).await.map_err(|e| ApiError(StatusCode::UNAUTHORIZED, e.1))?;
    orca_core::processing::audit(&o.db, &user.name, "login", "");
    Ok(Json(json!({"ok": true, "token": token, "perms": user.effective(), "user": user})).into_response())
}

async fn logout(State(o): State<St>, req: Request) -> ApiResult {
    if let Some(t) = session_token(req.headers(), req.uri().query()) { o.users.logout(&t); }
    ok()
}

#[derive(Deserialize)]
struct MonReq { track_id: i64, #[serde(default)] start_ms: Option<i64> }
async fn monitor_play(State(o): State<St>, Json(r): Json<MonReq>) -> ApiResult {
    let t = o.db.get_track(r.track_id)?.ok_or_else(|| crate::not_found("skladba"))?;
    let o2 = o.clone();
    blocking(move || {
        let c = o2.cfg.read().clone();
        let start = r.start_ms.unwrap_or(t.cue_in_ms);
        o2.monitor.play(&c, Some(t.id), general::map_path(&c, &t.path).into(), start, t.duration_ms)
    }).await?;
    ok()
}
async fn monitor_stop(State(o): State<St>) -> ApiResult { o.monitor.stop(); ok() }
async fn monitor_route(State(o): State<St>) -> ApiResult {
    let c = o.cfg.read().clone();
    Ok(Json(json!(blocking(move || Ok(orca_core::monitor::route(&c))).await?)).into_response())
}
async fn monitor_status(State(o): State<St>) -> ApiResult { Ok(Json(json!(o.monitor.status())).into_response()) }

// --- živý vstup a ovládání z pultu (GPI / MIDI / TCP / HTTP)
async fn gpio_status(State(o): State<St>) -> ApiResult {
    let g = o.gpio.clone();
    let v = tokio::task::spawn_blocking(move || g.status()).await.map_err(ApiError::from)?;
    Ok(Json(v).into_response())
}
#[derive(Deserialize)]
struct ActionReq { action: String }
/// Akce z ovladače (Stream Deck / Companion / skript): continue, next, play, stop, mic_on, live_insert…
async fn gpio_action(State(o): State<St>, Json(r): Json<ActionReq>) -> ApiResult {
    let res = o.gpio.run_action(&r.action);
    Ok(Json(json!({"ok": true, "result": res})).into_response())
}
#[derive(Deserialize)]
struct TriggerReq { id: String, on: bool }
async fn gpio_trigger(State(o): State<St>, Json(r): Json<TriggerReq>) -> ApiResult { o.gpio.manual_trigger(&r.id, r.on); ok() }
#[derive(Deserialize)]
struct LiveReq { #[serde(default)] label: String, #[serde(default)] duration_s: u32, #[serde(default)] auto_continue: bool, #[serde(default)] position: Option<usize>, #[serde(default)] now: bool }
/// Živý vstup do decků (nebo hned).
async fn queue_live(State(o): State<St>, Json(r): Json<LiveReq>) -> ApiResult {
    let it = orca_core::model::QueueItem::live_break(&r.label, r.duration_s as i64 * 1000, r.auto_continue, "manual", "živý vstup – přidáno ručně");
    let id = it.id.clone();
    o.engine.queue_push(it, if r.now { Some(0) } else { r.position });
    if r.now { o.engine.send(orca_core::engine::Cmd::PlayNow); }
    Ok(Json(json!({"ok": true, "id": id})).into_response())
}
