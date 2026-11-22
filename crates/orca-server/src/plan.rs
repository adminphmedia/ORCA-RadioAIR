//! API: plánování (pořady, epizody, moderátoři, playlisty, programová mřížka, události, plán vysílání),
//! rebroadcast a metadata (RDS, DAB DLS/SLS/EPG, počasí). Veřejné cesty pro SPI/RadioDNS a obrázky.

use crate::{bad, not_found, ok, ApiError, ApiResult, St};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::NaiveDate;
use orca_core::config::RelaySource;
use orca_core::sched::{self, Episode, GridEntry, PlItem, Playlist, Presenter, SchedEvent, Show};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<St> {
    Router::new()
        // moderátoři, pořady, epizody
        .route("/sched/presenters", get(list_doc::<Presenter>).post(create_doc::<Presenter>))
        .route("/sched/presenters/{id}", put(update_doc::<Presenter>).delete(delete_doc::<Presenter>))
        .route("/sched/presenters/{id}/photo", post(presenter_photo).layer(DefaultBodyLimit::max(30 * 1024 * 1024)))
        .route("/sched/shows", get(list_doc::<Show>).post(create_doc::<Show>))
        .route("/sched/shows/{id}", put(update_doc::<Show>).delete(show_delete))
        .route("/sched/shows/{id}/image", post(show_image).layer(DefaultBodyLimit::max(30 * 1024 * 1024)))
        .route("/sched/shows/{id}/episodes", get(episodes_list))
        .route("/sched/episodes", post(episode_create))
        .route("/sched/episodes/{id}", put(episode_update).delete(episode_delete))
        .route("/sched/episodes/{id}/queue", post(episode_queue))
        // playlisty
        .route("/sched/playlists", get(list_doc::<Playlist>).post(create_doc::<Playlist>))
        .route("/sched/playlists/{id}", get(playlist_get).put(update_doc::<Playlist>).delete(delete_doc::<Playlist>))
        .route("/sched/playlists/{id}/queue", post(playlist_queue))
        .route("/sched/playlists/{id}/preview", get(playlist_preview))
        .route("/sched/playlists/import", post(playlist_import).layer(DefaultBodyLimit::max(5 * 1024 * 1024)))
        .route("/sched/resolve", post(resolve_items))
        // programová mřížka, události
        .route("/sched/grid", get(list_doc::<GridEntry>).post(create_doc::<GridEntry>))
        .route("/sched/grid/{id}", put(update_doc::<GridEntry>).delete(delete_doc::<GridEntry>))
        .route("/sched/events", get(list_doc::<SchedEvent>).post(create_doc::<SchedEvent>))
        .route("/sched/events/{id}", put(update_doc::<SchedEvent>).delete(delete_doc::<SchedEvent>))
        .route("/sched/timetable", get(timetable))
        .route("/sched/fired/reset", post(fired_reset))
        .route("/sched/program", get(program))
        // plán vysílání
        .route("/sched/plan", get(plan_get))
        .route("/sched/plan/generate", post(plan_generate))
        .route("/sched/plan/clear", post(plan_clear))
        .route("/sched/plan/{date}/{hour}", put(plan_put))
        // rebroadcast
        .route("/relay/status", get(relay_status))
        .route("/relay/probe", post(relay_probe))
        .route("/relay/start", post(relay_start))
        .route("/relay/stop", post(relay_stop))
        // metadata
        .route("/meta/state", get(meta_state))
        .route("/meta/preview", post(meta_preview))
        .route("/meta/override", post(meta_override))
        .route("/meta/refresh", post(meta_refresh))
        .route("/meta/weather/refresh", post(weather_refresh))
        .route("/meta/weather/geocode", get(weather_geocode))
        .route("/meta/logo", post(logo_upload).delete(logo_delete).layer(DefaultBodyLimit::max(30 * 1024 * 1024)))
        .route("/meta/epg/si.xml", get(epg_si))
        .route("/meta/epg/pi.xml", get(epg_pi))
        .route("/meta/rds/ports", get(rds_ports))
        .route("/ui/window", post(ui_window))
}

/// Veřejné cesty bez tokenu (přijímače, RadioDNS, web).
pub fn public_routes() -> Router<St> {
    Router::new()
        .route("/radiodns/spi/3.1/SI.xml", get(epg_si))
        .route("/radiodns/spi/3.1/id/{*rest}", get(epg_pi_path))
        .route("/meta/now.json", get(now_json))
        .route("/meta/slide/{name}", get(slide))
        .route("/meta/image/{kind}", get(image_logo))
        .route("/meta/image/{kind}/{id}", get(image))
        .route("/meta/cover/{id}", get(cover))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::from)?.map_err(ApiError::from)
}

// ---------------------------------------------------------------------------------------------
// Obecné CRUD pro dokumenty

async fn list_doc<T: sched::Doc + Send + 'static>(State(o): State<St>) -> ApiResult { Ok(Json(sched::list::<T>(&o.db)?).into_response()) }
async fn create_doc<T: sched::Doc + Send + 'static>(State(o): State<St>, Json(mut d): Json<T>) -> ApiResult {
    d.set_id(0);
    let mut v = serde_json::to_value(&d)?; v["id"] = serde_json::Value::Null;
    let d: T = serde_json::from_value(v)?;
    let r = sched::save(&o.db, &d)?;
    o.meta.refresh();
    Ok(Json(r).into_response())
}
async fn update_doc<T: sched::Doc + Send + 'static>(State(o): State<St>, Path(id): Path<i64>, Json(mut d): Json<T>) -> ApiResult {
    d.set_id(id);
    let r = sched::save(&o.db, &d)?;
    o.meta.refresh();
    Ok(Json(r).into_response())
}
async fn delete_doc<T: sched::Doc + Send + 'static>(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { sched::delete::<T>(&o.db, id)?; o.meta.refresh(); ok() }
async fn show_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { sched::delete_show(&o.db, id)?; o.meta.refresh(); ok() }

fn media_path(o: &St, kind: &str, id: i64, ctype: &str) -> std::path::PathBuf {
    let ext = if ctype.contains("png") { "png" } else if ctype.contains("webp") { "webp" } else { "jpg" };
    o.data_dir.join("Media").join(kind).join(format!("{id}-{}.{ext}", chrono::Utc::now().timestamp()))
}

fn check_image(body: &Bytes) -> Result<(), ApiError> {
    if body.is_empty() { return Err(bad("prázdný soubor")); }
    if orca_core::slides::load_image(body).is_none() { return Err(bad("soubor není obrázek (JPG/PNG)")); }
    Ok(())
}

async fn presenter_photo(State(o): State<St>, Path(id): Path<i64>, headers: HeaderMap, body: Bytes) -> ApiResult {
    check_image(&body)?;
    let mut p = sched::get::<Presenter>(&o.db, id)?.ok_or_else(|| not_found("moderátor"))?;
    let path = media_path(&o, "presenters", id, headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or(""));
    if let Some(d) = path.parent() { std::fs::create_dir_all(d)?; }
    std::fs::write(&path, &body)?;
    p.photo_path = path.to_string_lossy().to_string();
    let r = sched::save(&o.db, &p)?;
    o.meta.refresh();
    Ok(Json(r).into_response())
}

async fn show_image(State(o): State<St>, Path(id): Path<i64>, headers: HeaderMap, body: Bytes) -> ApiResult {
    check_image(&body)?;
    let mut s = sched::get::<Show>(&o.db, id)?.ok_or_else(|| not_found("pořad"))?;
    let path = media_path(&o, "shows", id, headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or(""));
    if let Some(d) = path.parent() { std::fs::create_dir_all(d)?; }
    std::fs::write(&path, &body)?;
    s.image_path = path.to_string_lossy().to_string();
    let r = sched::save(&o.db, &s)?;
    o.meta.refresh();
    Ok(Json(r).into_response())
}

// ---------------------------------------------------------------------------------------------
// Epizody

async fn episodes_list(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let db = o.db.clone();
    Ok(Json(blocking(move || sched::episodes(&db, id)).await?).into_response())
}
async fn episode_create(State(o): State<St>, Json(mut e): Json<Episode>) -> ApiResult {
    e.id = None;
    if sched::get::<Show>(&o.db, e.show_id)?.is_none() { return Err(bad("pořad neexistuje")); }
    Ok(Json(sched::save_episode(&o.db, &e)?).into_response())
}
async fn episode_update(State(o): State<St>, Path(id): Path<i64>, Json(mut e): Json<Episode>) -> ApiResult {
    e.id = Some(id);
    Ok(Json(sched::save_episode(&o.db, &e)?).into_response())
}
async fn episode_delete(State(o): State<St>, Path(id): Path<i64>) -> ApiResult { sched::delete_episode(&o.db, id)?; ok() }

#[derive(Deserialize, Default)]
#[serde(default)]
struct QueueReq { position: String, mark_aired: Option<bool> }

/// Vložení do decků: next (hned jako další), now (okamžitě), end (na konec).
fn push_items(o: &St, items: Vec<orca_core::model::QueueItem>, position: &str) -> usize {
    let n = items.len();
    match position {
        "end" => for it in items { o.engine.queue_push(it, None); },
        _ => for (i, it) in items.into_iter().enumerate() { o.engine.queue_push(it, Some(i)); },
    }
    if position == "now" { o.engine.send(orca_core::engine::Cmd::Next); }
    n
}

async fn episode_queue(State(o): State<St>, Path(id): Path<i64>, Json(q): Json<QueueReq>) -> ApiResult {
    let e = sched::get_episode(&o.db, id)?.ok_or_else(|| not_found("epizoda"))?;
    let cfg = o.cfg.read().clone();
    let show = sched::get::<Show>(&o.db, e.show_id)?.map(|s| s.name).unwrap_or_default();
    let (items, _, warnings) = sched::expand(&o.db, &cfg, &e.parts, "manual", &format!("{show} – díl {} {} (ručně)", e.num, e.title), 0)?;
    if items.is_empty() { return Err(bad("epizoda nemá žádné přehratelné části")); }
    let n = push_items(&o, items, &q.position);
    if q.mark_aired.unwrap_or(true) { sched::mark_episode_aired(&o.db, id)?; }
    Ok(Json(json!({"ok": true, "count": n, "warnings": warnings})).into_response())
}

// ---------------------------------------------------------------------------------------------
// Playlisty

async fn playlist_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(sched::get::<Playlist>(&o.db, id)?.ok_or_else(|| not_found("playlist"))?).into_response())
}

async fn playlist_queue(State(o): State<St>, Path(id): Path<i64>, Json(q): Json<QueueReq>) -> ApiResult {
    let mut p = sched::get::<Playlist>(&o.db, id)?.ok_or_else(|| not_found("playlist"))?;
    if p.shuffle { sched::shuffle(&mut p.items); }
    let cfg = o.cfg.read().clone();
    let (items, aired, warnings) = sched::expand(&o.db, &cfg, &p.items, "playlist", &format!("playlist {} (ručně)", p.name), 0)?;
    if items.is_empty() { return Err(bad("playlist nemá žádné přehratelné položky")); }
    let n = push_items(&o, items, &q.position);
    for e in aired { let _ = sched::mark_episode_aired(&o.db, e); }
    Ok(Json(json!({"ok": true, "count": n, "warnings": warnings})).into_response())
}

/// Náhled: co by se teď vybralo (bez zápisu do historie).
async fn playlist_preview(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let p = sched::get::<Playlist>(&o.db, id)?.ok_or_else(|| not_found("playlist"))?;
    let cfg = o.cfg.read().clone();
    let (items, _, warnings) = sched::expand(&o.db, &cfg, &p.items, "preview", "náhled", 0)?;
    let total = sched::estimate_items(&o.db, &p.items);
    Ok(Json(json!({"items": items, "warnings": warnings, "duration_ms": total})).into_response())
}

#[derive(Deserialize)]
struct ResolveReq { items: Vec<PlItem> }

/// Rozvinutí jednotlivých položek (UI ukazuje, který soubor se teď vybere – např. „2. nejnovější“).
async fn resolve_items(State(o): State<St>, Json(r): Json<ResolveReq>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let mut out = vec![];
    for it in &r.items {
        let (items, _, warnings) = sched::expand(&o.db, &cfg, std::slice::from_ref(it), "preview", "náhled", 0)?;
        out.push(json!({"id": it.id, "items": items, "warnings": warnings, "duration_ms": sched::estimate_items(&o.db, std::slice::from_ref(it))}));
    }
    Ok(Json(out).into_response())
}

async fn playlist_import(State(o): State<St>, body: Bytes) -> ApiResult {
    let text = String::from_utf8_lossy(&body).to_string();
    Ok(Json(sched::import_text(&o.db, &text)?).into_response())
}

// ---------------------------------------------------------------------------------------------
// Rozvrh, program

#[derive(Deserialize)]
struct DateQ { date: Option<String> }

fn date_or_today(o: &St, d: &Option<String>) -> Result<NaiveDate, ApiError> {
    match d.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| bad("neplatné datum")),
        None => Ok(orca_core::automation::now_local(&o.cfg.read()).date_naive()),
    }
}

async fn timetable(State(o): State<St>, Query(q): Query<DateQ>) -> ApiResult {
    let d = date_or_today(&o, &q.date)?;
    Ok(Json(sched::day_occurrences(&o.db, d)?).into_response())
}

#[derive(Deserialize)]
struct KeyReq { key: String }
async fn fired_reset(State(o): State<St>, Json(k): Json<KeyReq>) -> ApiResult { sched::unmark_fired(&o.db, &k.key)?; ok() }

async fn program(State(o): State<St>, Query(q): Query<DateQ>) -> ApiResult {
    let d = date_or_today(&o, &q.date)?;
    Ok(Json(sched::program_for_day(&o.db, d)?).into_response())
}

// ---------------------------------------------------------------------------------------------
// Plán vysílání

async fn plan_get(State(o): State<St>, Query(q): Query<DateQ>) -> ApiResult {
    let d = date_or_today(&o, &q.date)?.format("%Y-%m-%d").to_string();
    let db = o.db.clone();
    let (hours, rows) = blocking(move || Ok((sched::plan_hours_with_items(&db, &d)?, sched::plan_rows(&db, &d, None)?))).await?;
    let use_plan = o.cfg.read().schedule.use_plan;
    Ok(Json(json!({"hours": hours.into_iter().map(|(h, n, p)| json!({"hour": h, "count": n, "planned": p})).collect::<Vec<_>>(), "rows": rows, "use_plan": use_plan})).into_response())
}

#[derive(Deserialize)]
struct GenReq { date: String, hours: Vec<u32>, #[serde(default)] overwrite: bool }
async fn plan_generate(State(o): State<St>, Json(g): Json<GenReq>) -> ApiResult {
    let d = NaiveDate::parse_from_str(&g.date, "%Y-%m-%d").map_err(|_| bad("neplatné datum"))?;
    let (db, cfg) = (o.db.clone(), o.cfg.read().clone());
    let n = blocking(move || sched::generate_plan(&db, &cfg, d, &g.hours, g.overwrite)).await?;
    Ok(Json(json!({"ok": true, "created": n})).into_response())
}

#[derive(Deserialize)]
struct ClearReq { date: String, hour: Option<u32>, #[serde(default)] keep_locked: bool }
async fn plan_clear(State(o): State<St>, Json(c): Json<ClearReq>) -> ApiResult {
    let n = sched::plan_clear(&o.db, &c.date, c.hour, c.keep_locked)?;
    Ok(Json(json!({"ok": true, "deleted": n})).into_response())
}

#[derive(Deserialize)]
struct PlanRowIn { item: PlItem, #[serde(default)] locked: bool }
async fn plan_put(State(o): State<St>, Path((date, hour)): Path<(String, u32)>, Json(rows): Json<Vec<PlanRowIn>>) -> ApiResult {
    if NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() || hour > 23 { return Err(bad("neplatné datum nebo hodina")); }
    let v: Vec<(PlItem, bool)> = rows.into_iter().map(|r| (r.item, r.locked)).collect();
    sched::plan_save_hour(&o.db, &date, hour, &v)?;
    ok()
}

// ---------------------------------------------------------------------------------------------
// Rebroadcast

async fn relay_status(State(o): State<St>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let cur = o.engine.snapshot().current.and_then(|c| c.item.relay.clone());
    Ok(Json(json!({"sources": orca_core::relay::statuses(&cfg), "on_air": cur})).into_response())
}

#[derive(Deserialize)]
struct ProbeReq { source: RelaySource, #[serde(default)] seconds: Option<u32> }
async fn relay_probe(State(o): State<St>, Json(p): Json<ProbeReq>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let secs = p.seconds.unwrap_or(4).clamp(1, 20);
    Ok(Json(blocking(move || orca_core::relay::probe(&cfg, &p.source, secs)).await?).into_response())
}

#[derive(Deserialize)]
struct RelayStart { id: String, #[serde(default)] duration_s: u32, #[serde(default)] position: String }
async fn relay_start(State(o): State<St>, Json(r): Json<RelayStart>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let it = orca_core::relay::queue_item(&cfg, &r.id, r.duration_s as i64 * 1000, "rebroadcast (ručně)").ok_or_else(|| not_found("zdroj"))?;
    let pos = if r.position.is_empty() { "now" } else { r.position.as_str() };
    push_items(&o, vec![it], pos);
    ok()
}
async fn relay_stop(State(o): State<St>) -> ApiResult {
    if o.engine.snapshot().current.map(|c| c.item.relay.is_some()).unwrap_or(false) { o.engine.send(orca_core::engine::Cmd::Next); }
    let ids: Vec<String> = o.engine.queue_snapshot().into_iter().filter(|i| i.relay.is_some()).map(|i| i.id).collect();
    for id in ids { o.engine.queue_remove(&id); }
    ok()
}

// ---------------------------------------------------------------------------------------------
// Metadata

async fn meta_state(State(o): State<St>) -> ApiResult {
    let st = o.meta.state.read().clone();
    Ok(Json(st).into_response())
}

#[derive(Deserialize)]
struct PreviewReq { templates: Vec<String> }
async fn meta_preview(State(o): State<St>, Json(p): Json<PreviewReq>) -> ApiResult {
    let cfg = o.cfg.read().clone();
    let ctx = o.meta.context();
    let out: Vec<_> = p.templates.iter().map(|t| {
        let m = orca_core::meta::message(&cfg, &ctx, t);
        json!({"template": t, "text": m.as_ref().map(|x| x.text.clone()), "tags": m.map(|x| x.tags).unwrap_or_default()})
    }).collect();
    Ok(Json(json!({"messages": out, "vars": ctx.vars})).into_response())
}

#[derive(Deserialize)]
struct OverrideReq { text: String, #[serde(default)] minutes: u32 }
async fn meta_override(State(o): State<St>, Json(r): Json<OverrideReq>) -> ApiResult { o.meta.set_override(&r.text, if r.minutes == 0 { 5 } else { r.minutes }); ok() }
async fn meta_refresh(State(o): State<St>) -> ApiResult { o.meta.refresh(); ok() }

async fn weather_refresh(State(o): State<St>) -> ApiResult {
    let m = o.meta.clone();
    Ok(Json(blocking(move || m.refresh_weather()).await?).into_response())
}

#[derive(Deserialize)]
struct GeoQ { q: String }
async fn weather_geocode(State(o): State<St>, Query(g): Query<GeoQ>) -> ApiResult {
    let lang = o.cfg.read().station.language.clone();
    Ok(Json(blocking(move || orca_core::weather::geocode(&g.q, &lang)).await?).into_response())
}

async fn logo_upload(State(o): State<St>, headers: HeaderMap, body: Bytes) -> ApiResult {
    check_image(&body)?;
    let path = media_path(&o, "station", 0, headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or(""));
    if let Some(d) = path.parent() { std::fs::create_dir_all(d)?; }
    std::fs::write(&path, &body)?;
    let mut cfg = o.cfg.read().clone();
    cfg.meta.logo_path = path.to_string_lossy().to_string();
    o.apply_config(cfg)?;
    Ok(Json(json!({"ok": true, "path": path})).into_response())
}
async fn logo_delete(State(o): State<St>) -> ApiResult {
    let mut cfg = o.cfg.read().clone();
    cfg.meta.logo_path.clear();
    o.apply_config(cfg)?;
    ok()
}

fn xml(body: String) -> Response { ([(header::CONTENT_TYPE, "application/xml; charset=utf-8")], body).into_response() }

async fn epg_si(State(o): State<St>) -> ApiResult { Ok(xml(orca_core::epg::si_xml(&o.cfg.read()))) }
async fn epg_pi(State(o): State<St>, Query(q): Query<DateQ>) -> ApiResult {
    let d = date_or_today(&o, &q.date)?;
    let cfg = o.cfg.read().clone();
    Ok(xml(orca_core::epg::pi_xml(&cfg, &o.db, d)?))
}
async fn epg_pi_path(State(o): State<St>, Path(rest): Path<String>) -> ApiResult {
    let file = rest.rsplit('/').next().unwrap_or("");
    let ds = file.strip_suffix("_PI.xml").ok_or_else(|| not_found("soubor"))?;
    let d = NaiveDate::parse_from_str(ds, "%Y%m%d").map_err(|_| not_found("soubor"))?;
    let cfg = o.cfg.read().clone();
    Ok(xml(orca_core::epg::pi_xml(&cfg, &o.db, d)?))
}

async fn rds_ports() -> ApiResult {
    Ok(Json(orca_core::rds::available_ports()).into_response())
}

async fn now_json(State(o): State<St>) -> ApiResult {
    let ctx = o.meta.context();
    Ok(([(header::CACHE_CONTROL, "no-cache"), (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], Json(o.meta.now_json(&ctx))).into_response())
}

fn img_resp(bytes: Vec<u8>, mime: &str) -> Response {
    ([(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("image/jpeg"))), (header::CACHE_CONTROL, HeaderValue::from_static("max-age=300"))], Body::from(bytes)).into_response()
}

async fn slide(State(o): State<St>, Path(name): Path<String>) -> ApiResult {
    let b = o.meta.slide_bytes(&name).ok_or_else(|| not_found("slide"))?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-cache")], b).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SizeQ { w: Option<u32>, h: Option<u32> }

async fn serve_image(path: String, s: SizeQ, bg: String) -> ApiResult {
    if path.is_empty() { return Err(not_found("obrázek")); }
    let res = blocking(move || {
        let raw = std::fs::read(&path)?;
        match (s.w, s.h) {
            (Some(w), Some(h)) if w > 0 && h > 0 && w <= 1600 && h <= 1600 => {
                let img = orca_core::slides::load_image(&raw).ok_or_else(|| anyhow::anyhow!("neplatný obrázek"))?;
                let png = w == h && w <= 128;
                Ok((orca_core::slides::resized(&img, w, h, png, orca_core::slides::parse_color(&bg))?, if png { "image/png" } else { "image/jpeg" }.to_string()))
            }
            _ => Ok((raw, if path.ends_with(".png") { "image/png".into() } else { "image/jpeg".into() })),
        }
    }).await?;
    Ok(img_resp(res.0, &res.1))
}

async fn image_logo(State(o): State<St>, Path(kind): Path<String>, Query(s): Query<SizeQ>) -> ApiResult {
    if kind != "logo" { return Err(not_found("obrázek")); }
    let (p, bg) = { let c = o.cfg.read(); (c.meta.logo_path.clone(), c.meta.brand_color.clone()) };
    serve_image(p, s, bg).await
}

async fn image(State(o): State<St>, Path((kind, id)): Path<(String, i64)>, Query(s): Query<SizeQ>) -> ApiResult {
    let bg = o.cfg.read().meta.brand_color.clone();
    let p = match kind.as_str() {
        "presenter" => sched::get::<Presenter>(&o.db, id)?.map(|p| p.photo_path),
        "show" => sched::get::<Show>(&o.db, id)?.map(|s| s.image_path),
        _ => None,
    }.ok_or_else(|| not_found("obrázek"))?;
    serve_image(p, s, bg).await
}

async fn cover(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let t = o.db.get_track(id)?.ok_or_else(|| not_found("skladba"))?;
    match tokio::task::spawn_blocking(move || orca_core::library::cover_for(&t)).await? {
        Some((d, mime)) => Ok(img_resp(d, &mime)),
        None => Err(not_found("obal")),
    }
}

#[derive(Deserialize)]
struct WinReq { panel: String, #[serde(default)] title: String }

/// Samostatné okno s panelem (desktop: nativní okno, prohlížeč: UI otevře window.open).
async fn ui_window(State(o): State<St>, Json(w): Json<WinReq>) -> ApiResult {
    if !w.panel.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || w.panel.is_empty() { return Err(bad("neplatný panel")); }
    let hook = o.window_hook.read().clone();
    match hook {
        Some(h) => { h(&w.panel, if w.title.is_empty() { &w.panel } else { &w.title }).map_err(|e| bad(&e))?; Ok(Json(json!({"ok": true, "native": true})).into_response()) }
        None => Ok(Json(json!({"ok": true, "native": false})).into_response()),
    }
}
