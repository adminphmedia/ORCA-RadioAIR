//! Voice tracky z domova: moderátor se přihlásí přes web, vybere playlist (nebo hodinu plánu Music Selectoru),
//! poslechne konec a začátek skladeb, nahraje hlas v prohlížeči a ORCA ho uloží do knihovny a vloží na místo.
//! Pro přístup z internetu běží na samostatném portu, kde je jen tato stránka – nic jiného z ORCA.

use crate::general_api::session_token;
use crate::{bad, ApiError, ApiResult, St};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use orca_core::model::TrackKind;
use orca_core::users::User;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

pub fn routes() -> Router<St> {
    Router::new()
        .route("/vt/me", get(me))
        .route("/vt/targets", get(targets))
        .route("/vt/playlist/{id}", get(playlist))
        .route("/vt/playlist/{id}/item/{item}", delete(playlist_item_delete))
        .route("/vt/selector/{log}", get(selector))
        .route("/vt/track/{id}/audio", get(track_audio))
        .route("/vt/track/{id}/waveform", get(track_waveform))
        .route("/vt/upload", post(upload).layer(DefaultBodyLimit::max(200 * 1024 * 1024)))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))
}

/// Přihlášený uživatel s oprávněním voice tracky (vyžaduje se vždy).
fn vt_user(o: &St, headers: &HeaderMap, query: Option<&str>) -> Result<User, ApiError> {
    let auto = o.cfg.read().general.auto_logout_min;
    let user = session_token(headers, query).and_then(|t| o.users.touch(&t, auto))
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "Přihlaste se jménem a PINem".into()))?;
    if !user.effective().iter().any(|p| p == "voicetrack") { return Err(ApiError(StatusCode::FORBIDDEN, format!("Uživatel {} nemá oprávnění nahrávat voice tracky", user.name))); }
    Ok(user)
}

fn track_json(t: &orca_core::model::Track) -> serde_json::Value {
    let (ci, mix, co) = t.effective_cues();
    json!({"id": t.id, "artist": t.artist, "title": t.title, "kind": t.kind().as_str(), "category": t.category, "duration_ms": t.duration_ms, "cue_in_ms": ci, "intro_ms": t.intro_ms, "mix_ms": mix, "cue_out_ms": co})
}

async fn me(State(o): State<St>, req: Request) -> ApiResult {
    let u = vt_user(&o, req.headers(), req.uri().query())?;
    let c = o.cfg.read().clone();
    Ok(Json(json!({"user": u.name, "station": c.station.name, "playlists": c.remote_vt.playlists, "selector": c.remote_vt.selector, "max_s": c.remote_vt.max_s, "default_position": c.remote_vt.default_position})).into_response())
}

async fn targets(State(o): State<St>, req: Request) -> ApiResult {
    vt_user(&o, req.headers(), req.uri().query())?;
    let (db, c) = (o.db.clone(), o.cfg.read().remote_vt.clone());
    let (pls, logs) = blocking(move || {
        let pls: Vec<_> = if c.playlists { orca_core::sched::list::<orca_core::sched::Playlist>(&db)?.into_iter().filter(|p| p.enabled).map(|p| json!({"id": p.id, "name": p.name, "description": p.description, "items": p.items.len(), "color": p.color})).collect() } else { vec![] };
        let logs: Vec<_> = if c.selector {
            let today = chrono::Local::now().date_naive();
            let to = today + chrono::Duration::days(7);
            orca_core::selector::logs::list_logs(&db, &today.to_string(), &to.to_string()).unwrap_or_default().into_iter().map(|l| json!({"id": l.id, "day": l.day, "status": l.status})).collect()
        } else { vec![] };
        Ok((pls, logs))
    }).await?;
    Ok(Json(json!({"playlists": pls, "selector": logs})).into_response())
}

async fn playlist(State(o): State<St>, Path(id): Path<i64>, req: Request) -> ApiResult {
    vt_user(&o, req.headers(), req.uri().query())?;
    let db = o.db.clone();
    let v = blocking(move || {
        let p = orca_core::sched::get::<orca_core::sched::Playlist>(&db, id)?.ok_or_else(|| anyhow::anyhow!("playlist neexistuje"))?;
        let items: Vec<_> = p.items.iter().enumerate().map(|(i, it)| {
            let tr = it.track_id.and_then(|tid| db.get_track(tid).ok().flatten());
            json!({"index": i, "id": it.id, "type": it.typ, "label": it.label, "category": it.category, "kind": it.kind.map(|k| k.as_str()), "position": it.position, "track": tr.as_ref().map(track_json)})
        }).collect();
        Ok(json!({"id": p.id, "name": p.name, "items": items}))
    }).await?;
    Ok(Json(v).into_response())
}

async fn playlist_item_delete(State(o): State<St>, Path((id, item)): Path<(i64, String)>, req: Request) -> ApiResult {
    vt_user(&o, req.headers(), req.uri().query())?;
    let db = o.db.clone();
    blocking(move || {
        let mut p = orca_core::sched::get::<orca_core::sched::Playlist>(&db, id)?.ok_or_else(|| anyhow::anyhow!("playlist neexistuje"))?;
        let Some(i) = p.items.iter().position(|x| x.id == item) else { anyhow::bail!("položka neexistuje") };
        let is_vt = p.items[i].track_id.and_then(|t| db.get_track(t).ok().flatten()).map(|t| t.kind() == TrackKind::VoiceTrack).unwrap_or(false);
        if !is_vt { anyhow::bail!("smazat jde jen voice track"); }
        p.items.remove(i);
        orca_core::sched::save(&db, &p)?;
        Ok(())
    }).await?;
    crate::ok()
}

async fn selector(State(o): State<St>, Path(log): Path<i64>, req: Request) -> ApiResult {
    vt_user(&o, req.headers(), req.uri().query())?;
    let db = o.db.clone();
    let v = blocking(move || {
        let info = orca_core::selector::logs::get_log(&db, log)?.ok_or_else(|| anyhow::anyhow!("plán neexistuje"))?;
        let items = orca_core::selector::logs::load_items(&db, log)?;
        let mut hours: Vec<serde_json::Value> = vec![];
        let mut cur = String::new();
        for (hour, label, _clock, it) in items {
            if hour != cur { cur = hour.clone(); hours.push(json!({"hour": hour, "label": label, "items": []})); }
            let tr = it.track_id.and_then(|t| db.get_track(t).ok().flatten());
            if let Some(h) = hours.last_mut() { h["items"].as_array_mut().unwrap().push(json!({"item_id": it.id, "pos": it.pos, "slot": it.slot.kind, "label": it.slot.label, "start": it.est_start, "track": tr.as_ref().map(track_json)})); }
        }
        Ok(json!({"id": info.id, "day": info.day, "status": info.status, "hours": hours}))
    }).await?;
    Ok(Json(v).into_response())
}

async fn track_audio(State(o): State<St>, Path(id): Path<i64>, req: Request) -> ApiResult {
    use tower::ServiceExt;
    vt_user(&o, req.headers(), req.uri().query())?;
    let t = o.db.get_track(id)?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "skladba neexistuje".into()))?;
    let path = PathBuf::from(orca_core::general::map_path(&o.cfg.read(), &t.path));
    Ok(tower_http::services::ServeFile::new(path).oneshot(req).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.into_response())
}

async fn track_waveform(State(o): State<St>, Path(id): Path<i64>, req: Request) -> ApiResult {
    vt_user(&o, req.headers(), req.uri().query())?;
    let t = o.db.get_track(id)?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "skladba neexistuje".into()))?;
    let path = PathBuf::from(orca_core::general::map_path(&o.cfg.read(), &t.path));
    let key = format!("{id}-{}", std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0));
    let cache = o.data_dir.join("Cache").join("waveforms");
    let w = blocking(move || orca_core::analysis::waveform(&path, &cache, &key, 2000)).await?;
    Ok(Json(w).into_response())
}

/// Nahraný hlas → převod (MP3) → knihovna (Voice tracky / kategorie moderátora) → vložení na místo.
async fn upload(State(o): State<St>, req: Request) -> ApiResult {
    let user = vt_user(&o, req.headers(), req.uri().query())?;
    let mut mp = Multipart::from_request(req, &o).await.map_err(|e| bad(&e.to_string()))?;
    let tmpdir = std::env::temp_dir().join("orca-vt");
    tokio::fs::create_dir_all(&tmpdir).await?;
    let mut fields = std::collections::HashMap::<String, String>::new();
    let mut audio: Option<PathBuf> = None;
    while let Some(mut f) = mp.next_field().await.map_err(|e| bad(&e.to_string()))? {
        let name = f.name().unwrap_or("").to_string();
        if name == "audio" {
            let p = tmpdir.join(format!("rec-{}-{}", std::process::id(), chrono::Utc::now().timestamp_millis()));
            let mut file = tokio::fs::File::create(&p).await?;
            while let Some(ch) = f.chunk().await.map_err(|e| bad(&e.to_string()))? { file.write_all(&ch).await?; }
            file.flush().await?;
            audio = Some(p);
        } else { fields.insert(name, f.text().await.unwrap_or_default()); }
    }
    let src = audio.ok_or_else(|| bad("chybí nahrávka"))?;
    let cfg = o.cfg.read().clone();
    let title = fields.get("title").cloned().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("VT {} {}", user.name, chrono::Local::now().format("%d.%m. %H:%M")));
    let category = if cfg.remote_vt.category.trim().is_empty() { user.name.clone() } else { cfg.remote_vt.category.clone() };
    let target = fields.get("target").cloned().unwrap_or_default();
    let target_id: i64 = fields.get("target_id").and_then(|s| s.parse().ok()).unwrap_or(0);
    let after = fields.get("after").cloned().unwrap_or_default();
    let position = fields.get("position").cloned().unwrap_or_else(|| cfg.remote_vt.default_position.clone());
    let (o2, uname) = (o.clone(), user.name.clone());
    let res = blocking(move || {
        // převod na MP3 s názvem podle titulku
        let out_dir = std::env::temp_dir().join("orca-vt");
        let fname = format!("{}.mp3", orca_core::library::sanitize(&title));
        let out = out_dir.join(&fname);
        let o3 = std::process::Command::new(orca_core::encoder::resolve_ffmpeg(&cfg)).args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(&src)
            .args(["-vn", "-ac", "1", "-ar", "44100", "-af", "silenceremove=start_periods=1:start_threshold=-50dB:start_silence=0.05,areverse,silenceremove=start_periods=1:start_threshold=-50dB:start_silence=0.1,areverse", "-c:a", "libmp3lame", "-b:a", "192k",
                "-metadata", &format!("title={title}"), "-metadata", &format!("artist={uname}")]).arg(&out).output()?;
        let _ = std::fs::remove_file(&src);
        if !o3.status.success() { anyhow::bail!("nahrávku nelze převést: {}", String::from_utf8_lossy(&o3.stderr).lines().last().unwrap_or("")); }
        let ms = orca_core::podcast::probe_ms(&cfg, &out).unwrap_or(0);
        if ms < 300 { anyhow::bail!("nahrávka je prázdná nebo příliš krátká"); }
        if ms > cfg.remote_vt.max_s as i64 * 1000 + 2000 { anyhow::bail!("nahrávka je delší než povolených {} s", cfg.remote_vt.max_s); }
        let rep = o2.library.add_paths(&[out.clone()], TrackKind::VoiceTrack, &category);
        let _ = std::fs::remove_file(&out);
        let t = rep.added.into_iter().next().ok_or_else(|| anyhow::anyhow!("uložení do knihovny selhalo: {}", rep.errors.join(", ")))?;
        let pos = if position == "none" || position.is_empty() { None } else { Some(position.clone()) };
        let place = match target.as_str() {
            "playlist" => {
                if !cfg.remote_vt.playlists { anyhow::bail!("vkládání do playlistů je vypnuté"); }
                let mut p = orca_core::sched::get::<orca_core::sched::Playlist>(&o2.db, target_id)?.ok_or_else(|| anyhow::anyhow!("playlist neexistuje"))?;
                let idx: usize = after.parse::<i64>().map(|x| (x + 1).clamp(0, p.items.len() as i64) as usize).unwrap_or(p.items.len());
                let item = orca_core::sched::PlItem { id: orca_core::podcast::new_id(), typ: "track".into(), track_id: Some(t.id), position: pos, label: format!("VT {}", uname), ..Default::default() };
                p.items.insert(idx, item);
                orca_core::sched::save(&o2.db, &p)?;
                format!("vloženo do playlistu {} na pozici {}", p.name, idx + 1)
            }
            "selector" => {
                if !cfg.remote_vt.selector { anyhow::bail!("vkládání do plánu je vypnuté"); }
                let after_id: i64 = after.parse().map_err(|_| anyhow::anyhow!("chybí místo v plánu"))?;
                let draft = orca_core::selector::logs::ensure_draft(&o2.db, target_id, &uname)?;
                let info = orca_core::selector::logs::get_log(&o2.db, draft)?.ok_or_else(|| anyhow::anyhow!("koncept plánu nelze vytvořit"))?;
                // položka v konceptu má jiné id – najít podle hodiny a pozice
                let src_items = orca_core::selector::logs::load_items(&o2.db, target_id)?;
                let (h, _, _, it) = src_items.into_iter().find(|x| x.3.id == Some(after_id)).ok_or_else(|| anyhow::anyhow!("místo v plánu nenalezeno"))?;
                let d_items = orca_core::selector::logs::load_items(&o2.db, draft)?;
                let d_after = d_items.iter().find(|x| x.0 == h && x.3.pos == it.pos).and_then(|x| x.3.id).ok_or_else(|| anyhow::anyhow!("místo v konceptu nenalezeno"))?;
                let slot = orca_core::selector::Slot { kind: "vt".into(), track_id: Some(t.id), track_kind: Some(TrackKind::VoiceTrack), label: format!("VT {uname}"), position: pos, ..Default::default() };
                orca_core::selector::logs::edit(&o2.db, draft, info.token, &orca_core::selector::logs::Edit::Insert { after_item_id: d_after, slot, track_id: Some(t.id) }, &uname)?;
                format!("vloženo do konceptu plánu {} – zveřejní ho dramaturg", info.day)
            }
            _ => "uloženo do knihovny (Voice tracky)".to_string(),
        };
        orca_core::processing::audit(&o2.db, &uname, "remote_vt", &format!("{} ({} ms): {place}", t.title, ms));
        Ok(json!({"ok": true, "track": track_json(&t), "message": place}))
    }).await?;
    Ok(Json(res).into_response())
}

/// Router samostatného portu pro moderátory z domova: jen přihlášení, voice tracky a stránka UI.
pub fn remote_router(orca: St) -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let api = Router::new()
        .route("/session/login", post(crate::general_api::login))
        .route("/session/logout", post(crate::general_api::logout))
        .merge(routes())
        .fallback(|| async { (StatusCode::NOT_FOUND, Json(json!({"error": "na tomto portu jsou jen voice tracky"}))) });
    let mut app = Router::new().nest("/api", api).route("/", get(remote_root));
    if let Some(d) = crate::ui_dir(&orca) {
        app = app.nest_service("/assets", ServeDir::new(d.join("assets"))).fallback_service(ServeDir::new(&d).fallback(ServeFile::new(d.join("index.html"))));
    }
    app.layer(axum::middleware::from_fn(security_headers)).with_state(orca)
}

/// Kořen: stránka pro voice tracky (/?vt=1).
async fn remote_root(State(o): State<St>, req: Request) -> Response {
    use tower::ServiceExt;
    if !req.uri().query().map(|q| q.contains("vt")).unwrap_or(false) { return Redirect::to("/?vt=1").into_response(); }
    match crate::ui_dir(&o) {
        Some(d) => tower_http::services::ServeFile::new(d.join("index.html")).oneshot(req).await.map(|r| r.into_response()).unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response()),
        None => (StatusCode::NOT_FOUND, "UI není k dispozici").into_response(),
    }
}

async fn security_headers(req: Request, next: axum::middleware::Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, header::HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    r
}

/// Spouští / přepíná samostatný port podle nastavení (běží po celou dobu aplikace).
pub async fn run_remote(orca: St) {
    let mut current: Option<(String, tokio::task::JoinHandle<()>)> = None;
    loop {
        let c = orca.cfg.read().remote_vt.clone();
        let want = if c.enabled { Some(c.bind.clone()) } else { None };
        if want.as_deref() != current.as_ref().map(|x| x.0.as_str()) {
            if let Some((_, h)) = current.take() { h.abort(); log::info!("voice tracky z domova: port zavřen"); }
            if let Some(bind) = want {
                match tokio::net::TcpListener::bind(&bind).await {
                    Ok(l) => {
                        log::info!("voice tracky z domova: http://{bind}");
                        let app = remote_router(orca.clone());
                        current = Some((bind, tokio::spawn(async move { let _ = axum::serve(l, app).await; })));
                    }
                    Err(e) => { orca.events.alert("warning", "remote_vt", format!("Voice tracky z domova: port {bind} nelze otevřít ({e})")); current = Some((bind, tokio::spawn(async {}))); }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
