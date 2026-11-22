//! API: ORCA Music Selector (formát, schémata, generování, editor plánu, publikace) a Analytika vysílání.

use crate::{bad, ApiError, ApiResult, St};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Duration, Local, NaiveDate};
use orca_core::selector::{self, analytics, attrs, generator, logs, presets, SelectorConfig};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

pub fn routes() -> Router<St> {
    Router::new()
        .route("/selector/config", get(config_get).put(config_put))
        .route("/selector/config/revisions", get(config_revisions))
        .route("/selector/config/restore", post(config_restore))
        .route("/selector/describe", post(describe))
        .route("/selector/presets", get(presets_list))
        .route("/selector/presets/preview", post(preset_preview))
        .route("/selector/capacity", post(capacity))
        .route("/selector/simulate", post(simulate))
        .route("/selector/generate", post(generate))
        .route("/selector/generate/status", get(gen_status))
        .route("/selector/generate/cancel", post(gen_cancel))
        .route("/selector/logs", get(logs_list))
        .route("/selector/logs/compare", get(logs_compare))
        .route("/selector/logs/{id}", get(log_get))
        .route("/selector/logs/{id}/edit", post(log_edit))
        .route("/selector/logs/{id}/publish", post(log_publish))
        .route("/selector/logs/{id}/regenerate", post(log_regen))
        .route("/selector/logs/{id}/alternatives", get(log_alternatives))
        .route("/selector/reconcile", get(reconcile))
        .route("/selector/attrs", get(attrs_list))
        .route("/selector/attrs/bulk", post(attrs_bulk))
        .route("/selector/attrs/suggest", post(attrs_suggest))
        .route("/selector/attrs/import", post(attrs_import))
        .route("/selector/attrs/{id}", get(attrs_get).put(attrs_put))
        .route("/selector/duplicates", get(duplicates))
        .route("/selector/categories", get(lib_categories))
        .route("/analytics/station", post(an_station))
        .route("/analytics/track/{id}", get(an_track))
        .route("/analytics/artist", get(an_artist))
        .route("/analytics/rotation", get(an_rotation))
        .route("/analytics/export.csv", get(an_export))
        .route("/selector/analysis", get(af_status).post(af_start))
        .route("/selector/analysis/cancel", post(af_cancel))
        .route("/reports/cmo/config", get(cmo_get).put(cmo_put))
        .route("/reports/cmo/validate", get(cmo_validate))
        .route("/reports/cmo/{which}", get(cmo_export))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            let m = e.to_string();
            if let Some(x) = m.strip_prefix("CONFLICT: ") { Err(ApiError(StatusCode::CONFLICT, x.into())) } else { Err(bad(&m)) }
        }
        Err(e) => Err(ApiError::from(e)),
    }
}
fn user(h: &HeaderMap) -> String { h.get("x-orca-user").and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty()).unwrap_or("operátor").to_string() }
fn date(s: &str) -> Result<NaiveDate, ApiError> { NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| bad("datum ve formátu RRRR-MM-DD")) }
fn today() -> NaiveDate { Local::now().date_naive() }

// ---------------------------------------------------------------------------------------------
// Konfigurace

async fn config_get(State(o): State<St>) -> ApiResult {
    let v = blocking(move || selector::load_config(&o.db)).await?;
    Ok(Json(json!({ "version": v.version, "updated_at": v.updated_at, "updated_by": v.updated_by, "data": v.data, "rules_text": selector::describe_config(&v.data) })).into_response())
}
#[derive(Deserialize)]
struct SaveCfg { base_version: i64, data: SelectorConfig }
async fn config_put(State(o): State<St>, h: HeaderMap, Json(r): Json<SaveCfg>) -> ApiResult {
    let u = user(&h);
    let v = blocking(move || selector::save_config(&o.db, &r.data, r.base_version, &u)).await?;
    Ok(Json(json!({ "version": v })).into_response())
}
async fn config_revisions(State(o): State<St>) -> ApiResult { Ok(Json(blocking(move || selector::config_revisions(&o.db)).await?).into_response()) }
#[derive(Deserialize)]
struct Restore { version: i64 }
async fn config_restore(State(o): State<St>, h: HeaderMap, Json(r): Json<Restore>) -> ApiResult {
    let u = user(&h);
    Ok(Json(json!({ "version": blocking(move || selector::restore_config(&o.db, r.version, &u)).await? })).into_response())
}
async fn describe(Json(c): Json<SelectorConfig>) -> ApiResult { Ok(Json(selector::describe_config(&c)).into_response()) }

// ---------------------------------------------------------------------------------------------
// Presety, kapacita, simulace

async fn presets_list() -> ApiResult { Ok(Json(presets::catalog()).into_response()) }

async fn preset_preview(State(o): State<St>, Json(a): Json<presets::ApplyPreset>) -> ApiResult {
    let v = blocking(move || {
        let p = presets::catalog().into_iter().find(|p| p.id == a.preset_id).ok_or_else(|| anyhow::anyhow!("neznámý preset"))?;
        let base = selector::load_config(&o.db)?.data;
        let cfg = presets::apply(&p, &a, &base);
        let tracks = attrs::load_tracks(&o.db, &logs::all_kinds(&cfg))?;
        let cap = generator::capacity(&cfg, &tracks, &today().to_string());
        Ok(json!({ "config": cfg, "capacity": cap, "rules_text": selector::describe_config(&cfg) }))
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CfgOpt { config: Option<SelectorConfig>, from: Option<String>, days: Option<u32> }

async fn capacity(State(o): State<St>, Json(r): Json<CfgOpt>) -> ApiResult {
    let v = blocking(move || {
        let cfg = match r.config { Some(c) => c, None => selector::load_config(&o.db)?.data };
        let tracks = attrs::load_tracks(&o.db, &logs::all_kinds(&cfg))?;
        Ok(generator::capacity(&cfg, &tracks, &today().to_string()))
    }).await?;
    Ok(Json(v).into_response())
}

/// Simulace bez zápisu: vygeneruje N dní v paměti nad stejnými daty a spočítá stejné metriky jako analytika.
async fn simulate(State(o): State<St>, Json(r): Json<CfgOpt>) -> ApiResult {
    let v = blocking(move || {
        let cfg = match r.config { Some(c) => c, None => selector::load_config(&o.db)?.data };
        if cfg.clocks.is_empty() { anyhow::bail!("formát nemá žádná schémata"); }
        let from = r.from.as_deref().map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d")).transpose()?.unwrap_or_else(|| today() + Duration::days(1));
        let days = r.days.unwrap_or(7).clamp(1, 14);
        let to = from + Duration::days(days as i64 - 1);
        let t0 = std::time::Instant::now();
        let tracks = attrs::load_tracks(&o.db, &logs::all_kinds(&cfg))?;
        let plans = logs::plan_days(&o.db, &cfg, &tracks, from, to, false, None, |_, _| {})?;
        let gen_ms = t0.elapsed().as_millis();
        let map: HashMap<i64, attrs::SelTrack> = tracks.iter().map(|t| (t.id, t.clone())).collect();
        let mut spins = analytics::plan_spins(&plans, &map);
        let years: HashMap<i64, (Option<i32>, String)> = tracks.iter().map(|t| (t.id, (t.year, t.genre.clone()))).collect();
        analytics::enrich(&mut spins, &HashMap::new(), &years);
        let report = analytics::station_report(&cfg, &spins, from, to, "plan", None);
        let days_v: Vec<_> = plans.iter().map(|p| json!({ "day": p.day, "stats": p.stats })).collect();
        let unfilled: Vec<_> = plans.iter().flat_map(|p| p.hours.iter().flat_map(move |h| h.items.iter().filter(|i| i.origin == "unfilled").map(move |i| json!({ "day": p.day, "hour": h.spec.label, "pos": i.pos + 1, "category": i.slot.category, "reason": i.note })))).take(50).collect();
        Ok(json!({ "from": from.to_string(), "to": to.to_string(), "days": days_v, "report": report, "capacity": generator::capacity(&cfg, &tracks, &from.to_string()), "unfilled": unfilled,
            "tracks": tracks.len(), "generate_ms": gen_ms }))
    }).await?;
    Ok(Json(v).into_response())
}

// ---------------------------------------------------------------------------------------------
// Generování a plány

#[derive(Deserialize)]
struct GenReq { from: String, to: String }
async fn generate(State(o): State<St>, h: HeaderMap, Json(r): Json<GenReq>) -> ApiResult {
    let (from, to) = (date(&r.from)?, date(&r.to)?);
    let u = user(&h);
    let db = o.db.clone();
    Ok(Json(blocking(move || logs::start_generate(db, from, to, u)).await?).into_response())
}
async fn gen_status() -> ApiResult { Ok(Json(logs::gen_status()).into_response()) }
async fn gen_cancel() -> ApiResult { logs::cancel_gen(); crate::ok() }

#[derive(Deserialize)]
struct Range { from: Option<String>, to: Option<String> }
async fn logs_list(State(o): State<St>, Query(q): Query<Range>) -> ApiResult {
    let from = q.from.unwrap_or_else(|| (today() - Duration::days(7)).to_string());
    let to = q.to.unwrap_or_else(|| (today() + Duration::days(14)).to_string());
    Ok(Json(blocking(move || logs::list_logs(&o.db, &from, &to)).await?).into_response())
}

fn track_map(db: &orca_core::db::Db, ids: impl Iterator<Item = i64>) -> anyhow::Result<HashMap<i64, serde_json::Value>> {
    let mut m = HashMap::new();
    for id in ids {
        if m.contains_key(&id) { continue; }
        if let Some(t) = db.get_track(id)? {
            let (ci, mix, _) = t.effective_cues();
            m.insert(id, json!({ "id": t.id, "kind": t.kind().as_str(), "artist": t.artist, "title": t.title, "category": t.category, "len_ms": mix - ci, "intro_ms": (t.intro_ms - ci).max(0), "missing": t.missing, "year": t.year }));
        }
    }
    Ok(m)
}

async fn log_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    let v = blocking(move || {
        let info = logs::get_log(&o.db, id)?.ok_or_else(|| anyhow::anyhow!("plán nenalezen"))?;
        let plan = logs::load_plan(&o.db, id)?;
        let tracks = track_map(&o.db, plan.hours.iter().flat_map(|h| h.items.iter().filter_map(|i| i.track_id)))?;
        let consumed = logs::consumed_set(&o.db, &info.day)?;
        let aired: Vec<i64> = plan.hours.iter().flat_map(|h| h.items.iter().filter(|i| consumed.contains(&(h.spec.hour_utc.to_rfc3339(), i.pos as i64))).filter_map(|i| i.id)).collect();
        let revisions = logs::list_logs(&o.db, &info.day, &info.day)?;
        Ok(json!({ "info": info, "plan": plan, "tracks": tracks, "aired": aired, "revisions": revisions }))
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct EditReq { token: i64, edit: logs::Edit }
async fn log_edit(State(o): State<St>, h: HeaderMap, Path(id): Path<i64>, Json(r): Json<EditReq>) -> ApiResult {
    let u = user(&h);
    let v = blocking(move || {
        let draft = logs::ensure_draft(&o.db, id, &u)?;
        let mut token = r.token;
        let mut edit = r.edit;
        if draft != id {
            // úprava publikovaného plánu → nový koncept; položky mají nová id – dohledat podle hodiny a pozice
            token = logs::get_log(&o.db, draft)?.map(|l| l.token).unwrap_or(1);
            let old = logs::load_items(&o.db, id)?;
            let new = logs::load_items(&o.db, draft)?;
            let map = |iid: i64| -> i64 {
                old.iter().find(|x| x.3.id == Some(iid)).and_then(|(h, _, _, it)| new.iter().find(|y| &y.0 == h && y.3.pos == it.pos)).and_then(|y| y.3.id).unwrap_or(iid)
            };
            edit = match edit {
                logs::Edit::Replace { item_id, track_id, reason } => logs::Edit::Replace { item_id: map(item_id), track_id, reason },
                logs::Edit::Lock { item_id, locked } => logs::Edit::Lock { item_id: map(item_id), locked },
                logs::Edit::Clear { item_id } => logs::Edit::Clear { item_id: map(item_id) },
                logs::Edit::Move { item_id, delta } => logs::Edit::Move { item_id: map(item_id), delta },
                logs::Edit::Insert { after_item_id, slot, track_id } => logs::Edit::Insert { after_item_id: map(after_item_id), slot, track_id },
                logs::Edit::Delete { item_id } => logs::Edit::Delete { item_id: map(item_id) },
            };
        }
        let t = logs::edit(&o.db, draft, token, &edit, &u)?;
        Ok(json!({ "log_id": draft, "token": t }))
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct TokenReq { token: i64 }
async fn log_publish(State(o): State<St>, h: HeaderMap, Path(id): Path<i64>, Json(r): Json<TokenReq>) -> ApiResult {
    let u = user(&h);
    Ok(Json(blocking(move || logs::publish(&o.db, id, r.token, &u)).await?).into_response())
}

#[derive(Deserialize)]
struct RegenReq { token: i64, hours: Vec<String>, #[serde(default)] seed_bump: u64 }
async fn log_regen(State(o): State<St>, h: HeaderMap, Path(id): Path<i64>, Json(r): Json<RegenReq>) -> ApiResult {
    let u = user(&h);
    let (lid, tok) = blocking(move || logs::regenerate_hours(&o.db, id, r.token, &r.hours, r.seed_bump.max(1), &u)).await?;
    Ok(Json(json!({ "log_id": lid, "token": tok })).into_response())
}

#[derive(Deserialize)]
struct AltQ { item_id: i64, limit: Option<usize>, all: Option<bool> }
/// Alternativy pro pozici se skóre a rozkladem; s all=true i kandidáti porušující pravidla (pro ruční výjimku).
async fn log_alternatives(State(o): State<St>, Path(id): Path<i64>, Query(q): Query<AltQ>) -> ApiResult {
    let v = blocking(move || {
        let cfg = selector::load_config(&o.db)?.data;
        let plan = logs::load_plan(&o.db, id)?;
        let tracks = attrs::load_tracks(&o.db, &logs::all_kinds(&cfg))?;
        let mut ctx = generator::Ctx::new(&cfg, &tracks);
        let start = plan.hours.first().map(|h| h.spec.hour_utc).unwrap_or_else(chrono::Utc::now);
        for p in logs::history(&o.db, start - Duration::days(8), chrono::Utc::now().min(start))? { ctx.add_history(&p); }
        let (mut hi, mut ii) = (None, 0);
        for (a, h) in plan.hours.iter().enumerate() {
            for (b, it) in h.items.iter().enumerate() {
                if it.id == Some(q.item_id) { hi = Some(a); ii = b; continue; }
                if let (Some(t), Some(at)) = (it.track_id, it.est_start) {
                    if matches!(it.slot.kind.as_str(), "music" | "fixed") {
                        ctx.add_history(&generator::Placement { track_id: t, at: at.timestamp_millis(), date: h.spec.date, hour: h.spec.hour });
                    }
                }
            }
        }
        let hi = hi.ok_or_else(|| anyhow::anyhow!("položka nenalezena"))?;
        let (c, rej) = generator::candidates(&ctx, &plan.hours[hi], ii, &plan.day, plan.seed, q.all.unwrap_or(true), q.limit.unwrap_or(25), None);
        let current = plan.hours[hi].items[ii].clone();
        Ok(json!({ "candidates": c, "rejected": rej, "item": current, "hour": plan.hours[hi].spec }))
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct CmpQ { a: i64, b: i64 }
async fn logs_compare(State(o): State<St>, Query(q): Query<CmpQ>) -> ApiResult {
    let v = blocking(move || {
        let a = logs::load_items(&o.db, q.a)?;
        let b = logs::load_items(&o.db, q.b)?;
        let ka: BTreeMap<(String, usize), Option<i64>> = a.iter().map(|x| ((x.1.clone(), x.3.pos), x.3.track_id)).collect();
        let kb: BTreeMap<(String, usize), Option<i64>> = b.iter().map(|x| ((x.1.clone(), x.3.pos), x.3.track_id)).collect();
        let mut diff = vec![];
        for (k, va) in &ka { let vb = kb.get(k).copied().flatten(); if *va != vb { diff.push(json!({ "hour": k.0, "pos": k.1 + 1, "a": va, "b": vb })); } }
        for (k, vb) in &kb { if !ka.contains_key(k) { diff.push(json!({ "hour": k.0, "pos": k.1 + 1, "a": null, "b": vb })); } }
        let tracks = track_map(&o.db, a.iter().chain(b.iter()).filter_map(|x| x.3.track_id))?;
        Ok(json!({ "changes": diff, "tracks": tracks }))
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct DayQ { day: String }
async fn reconcile(State(o): State<St>, Query(q): Query<DayQ>) -> ApiResult {
    let v = blocking(move || {
        let r = logs::reconcile(&o.db, &q.day)?;
        let tracks = track_map(&o.db, r.iter().flat_map(|x| [x.planned_track, x.aired_track]).flatten())?;
        Ok(json!({ "items": r, "tracks": tracks }))
    }).await?;
    Ok(Json(v).into_response())
}

// ---------------------------------------------------------------------------------------------
// Atributy

#[derive(Deserialize)]
struct AttrQ { kind: Option<String>, category: Option<String>, q: Option<String>, missing: Option<String> }
async fn attrs_list(State(o): State<St>, Query(q): Query<AttrQ>) -> ApiResult {
    let v = blocking(move || {
        let all = attrs::all(&o.db)?;
        let (tracks, _) = o.db.list_tracks(&orca_core::db::TrackFilter { kind: Some(q.kind.unwrap_or_else(|| "song".into())), category: q.category.filter(|c| !c.is_empty()), q: q.q.filter(|c| !c.is_empty()), include_disabled: Some(true), limit: Some(5000), ..Default::default() })?;
        let rows: Vec<_> = tracks.iter().map(|t| {
            let st = attrs::SelTrack::from(t, all.get(&t.id));
            let sug = attrs::suggest(t, &st.attrs);
            json!({ "id": t.id, "artist": t.artist, "title": t.title, "category": t.category, "year": t.year, "len_ms": st.len_ms, "enabled": t.enabled, "missing": t.missing,
                "attrs": st.attrs, "artists": st.artists, "work": st.work, "suggest": sug })
        }).filter(|r| match q.missing.as_deref() { Some(f) if !f.is_empty() => r["attrs"][f].is_null() || r["attrs"][f] == "", _ => true }).collect();
        Ok(rows)
    }).await?;
    Ok(Json(v).into_response())
}
async fn attrs_get(State(o): State<St>, Path(id): Path<i64>) -> ApiResult {
    Ok(Json(blocking(move || Ok(attrs::get(&o.db, id)?.unwrap_or_else(|| attrs::MusicAttrs::new(id)))).await?).into_response())
}
async fn attrs_put(State(o): State<St>, Path(id): Path<i64>, Json(mut a): Json<attrs::MusicAttrs>) -> ApiResult {
    a.track_id = id;
    blocking(move || attrs::save(&o.db, &a)).await?;
    crate::ok()
}
#[derive(Deserialize)]
struct BulkReq { ids: Vec<i64>, set: BTreeMap<String, String>, #[serde(default)] dry_run: bool }
async fn attrs_bulk(State(o): State<St>, h: HeaderMap, Json(r): Json<BulkReq>) -> ApiResult {
    let u = user(&h);
    let v = blocking(move || {
        let ch = attrs::bulk_set(&o.db, &r.ids, &r.set, r.dry_run)?;
        if !r.dry_run { orca_core::processing::audit(&o.db, &u, "selector_attrs_bulk", &format!("{} změn u {} skladeb", ch.len(), r.ids.len())); }
        Ok(json!({ "changes": ch.len(), "preview": ch.into_iter().take(200).collect::<Vec<_>>(), "dry_run": r.dry_run }))
    }).await?;
    Ok(Json(v).into_response())
}
async fn attrs_suggest(State(o): State<St>) -> ApiResult {
    Ok(Json(json!({ "applied": blocking(move || attrs::apply_suggestions(&o.db, &[orca_core::model::TrackKind::Song])).await? })).into_response())
}

/// Import atributů z CSV: sloupec id nebo filename (nebo artist + title) a sloupce s názvy polí (language, local, energy, bpm …).
#[derive(Deserialize)]
struct AttrImport { text: String, #[serde(default)] dry_run: bool }
async fn attrs_import(State(o): State<St>, Json(r): Json<AttrImport>) -> ApiResult {
    let v = blocking(move || {
        let t = orca_core::import::parse_csv(&r.text, None, true);
        let col = |n: &str| t.headers.iter().position(|h| orca_core::import::norm(h) == n);
        let (ci, cf, ca, ct) = (col("id"), col("filename"), col("artist"), col("title"));
        let fields: Vec<(usize, String)> = t.headers.iter().enumerate().filter_map(|(i, h)| {
            let n = h.trim().to_lowercase();
            ["language", "country", "local", "bpm", "energy", "mood", "familiarity", "explicit", "gender", "sound", "work_key", "artists", "active_from", "active_to", "paused", "weight"].contains(&n.as_str()).then(|| (i, n.clone()))
                .or_else(|| n.starts_with("custom:").then(|| (i, n.clone())))
        }).collect();
        let (tracks, _) = o.db.list_tracks(&orca_core::db::TrackFilter { include_disabled: Some(true), include_missing: Some(true), limit: Some(1_000_000), ..Default::default() })?;
        let mut ok = 0; let mut errors = vec![]; let mut changes = 0;
        for (n, row) in t.rows.iter().enumerate() {
            let get = |i: Option<usize>| i.and_then(|i| row.get(i)).map(|s| s.trim().to_string()).unwrap_or_default();
            let found = if let Ok(id) = get(ci).parse::<i64>() { tracks.iter().find(|x| x.id == id) }
                else if !get(cf).is_empty() { tracks.iter().find(|x| x.filename.eq_ignore_ascii_case(&get(cf))) }
                else { tracks.iter().find(|x| x.artist.eq_ignore_ascii_case(&get(ca)) && x.title.eq_ignore_ascii_case(&get(ct))) };
            let Some(tr) = found else { errors.push(format!("řádek {}: skladba nenalezena", n + 2)); continue };
            let set: BTreeMap<String, String> = fields.iter().filter_map(|(i, f)| row.get(*i).map(|v| (f.clone(), v.clone()))).collect();
            let ch = attrs::bulk_set(&o.db, &[tr.id], &set, r.dry_run)?;
            if !r.dry_run { for (_, k, _, _) in &ch { let mut a = attrs::get(&o.db, tr.id)?.unwrap_or_else(|| attrs::MusicAttrs::new(tr.id)); a.origin.insert(k.clone(), "import".into()); attrs::save(&o.db, &a)?; } }
            changes += ch.len(); ok += 1;
        }
        Ok(json!({ "rows": t.rows.len(), "matched": ok, "changes": changes, "errors": errors.into_iter().take(100).collect::<Vec<_>>(), "fields": fields.iter().map(|f| f.1.clone()).collect::<Vec<_>>(), "dry_run": r.dry_run }))
    }).await?;
    Ok(Json(v).into_response())
}
async fn duplicates(State(o): State<St>) -> ApiResult { Ok(Json(blocking(move || attrs::duplicates(&o.db)).await?).into_response()) }
async fn lib_categories(State(o): State<St>) -> ApiResult { Ok(Json(blocking(move || Ok(o.db.categories()?)).await?).into_response()) }

// ---------------------------------------------------------------------------------------------
// Analytika

fn spins_for(o: &St, cfg: &SelectorConfig, f: &analytics::Filter, from: NaiveDate, to: NaiveDate) -> anyhow::Result<Vec<analytics::Spin>> {
    let mut spins = if f.basis.as_deref() == Some("plan") {
        let tracks = attrs::load_tracks(&o.db, &logs::all_kinds(cfg))?;
        let map: HashMap<i64, attrs::SelTrack> = tracks.into_iter().map(|t| (t.id, t)).collect();
        let mut plans = vec![];
        let mut d = from;
        while d <= to {
            if let Some(l) = logs::published(&o.db, &d.to_string())?.or(logs::latest(&o.db, &d.to_string())?) { plans.push(logs::load_plan(&o.db, l.id)?); }
            d = d.succ_opt().unwrap();
        }
        analytics::plan_spins(&plans, &map)
    } else { analytics::load_spins(&o.db, cfg, from, to)? };
    let all = attrs::all(&o.db)?;
    let (tracks, _) = o.db.list_tracks(&orca_core::db::TrackFilter { include_disabled: Some(true), include_missing: Some(true), limit: Some(1_000_000), ..Default::default() })?;
    let years: HashMap<i64, (Option<i32>, String)> = tracks.iter().map(|t| (t.id, (t.year, t.genre.clone()))).collect();
    analytics::enrich(&mut spins, &all, &years);
    Ok(analytics::filter(spins, f, cfg, from, to))
}

async fn an_station(State(o): State<St>, Json(f): Json<analytics::Filter>) -> ApiResult {
    let v = blocking(move || {
        let cfg = selector::load_config(&o.db)?.data;
        let from = NaiveDate::parse_from_str(&f.from, "%Y-%m-%d").unwrap_or_else(|_| today() - Duration::days(6));
        let to = NaiveDate::parse_from_str(&f.to, "%Y-%m-%d").unwrap_or_else(|_| today());
        if (to - from).num_days() > 400 { anyhow::bail!("nejvýše 400 dní"); }
        let basis = f.basis.clone().unwrap_or_else(|| "actual".into());
        let spins = spins_for(&o, &cfg, &f, from, to)?;
        let pva = if basis != "plan" {
            let mut p = analytics::PlanVsActual::default();
            let mut d = from; let mut any = false;
            while d <= to {
                for r in logs::reconcile(&o.db, &d.to_string())? {
                    any = true; p.planned += 1;
                    match r.status.as_str() { "odvysíláno" => p.aired_as_planned += 1, "jiná skladba" => p.replaced += 1, "vypuštěno" => p.dropped += 1, _ => {} }
                }
                d = d.succ_opt().unwrap();
            }
            if any {
                p.unplanned_music = spins.iter().filter(|s| analytics::is_music(s, &cfg) && s.qualified && s.plan_item.is_none()).count();
                let done = p.aired_as_planned + p.replaced + p.dropped;
                p.match_pct = if done > 0 { (p.aired_as_planned as f64 * 1000.0 / done as f64).round() / 10.0 } else { 0.0 };
                Some(p)
            } else { None }
        } else { None };
        let mut r = analytics::station_report(&cfg, &spins, from, to, &basis, pva);
        // aktivní rotace bez spinů
        if basis == "actual" {
            let cats: Vec<String> = cfg.categories.iter().map(|c| c.name.clone()).collect();
            if !cats.is_empty() {
                let played: std::collections::HashSet<i64> = spins.iter().filter(|s| s.qualified).filter_map(|s| s.track_id).collect();
                let tracks = attrs::load_tracks(&o.db, &[orca_core::model::TrackKind::Song])?;
                r.zero_spins = tracks.iter().filter(|t| cats.contains(&t.category) && t.active_on(&to.to_string()) && !played.contains(&t.id)).take(100).map(|t| (t.id, t.artist.clone(), t.title.clone(), t.category.clone())).collect();
            }
        }
        Ok(r)
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct DetQ { from: Option<String>, to: Option<String>, name: Option<String> }
fn det_range(q: &DetQ) -> (NaiveDate, NaiveDate) {
    let to = q.to.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()).unwrap_or_else(today);
    let from = q.from.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()).unwrap_or(to - Duration::days(62));
    (from, to)
}
async fn an_track(State(o): State<St>, Path(id): Path<i64>, Query(q): Query<DetQ>) -> ApiResult {
    let v = blocking(move || {
        let cfg = selector::load_config(&o.db)?.data;
        let (from, to) = det_range(&q);
        let lo = (today() - Duration::days(62)).min(from);
        let spins = spins_for(&o, &cfg, &analytics::Filter::default(), lo, today().max(to))?;
        // nejbližší plánované hraní
        let mut next = None;
        for l in logs::list_logs(&o.db, &today().to_string(), &(today() + Duration::days(14)).to_string())? {
            if l.status == "superseded" { continue; }
            for (_, _, _, it) in logs::load_items(&o.db, l.id)? {
                if it.track_id == Some(id) { if let Some(t) = it.est_start { if t > chrono::Utc::now() && next.as_ref().map(|(x, _)| t < *x).unwrap_or(true) { next = Some((t, l.status.clone())); } } }
            }
        }
        let tracks = attrs::load_tracks(&o.db, &[orca_core::model::TrackKind::Song])?;
        let me = tracks.iter().find(|t| t.id == id);
        let expected = me.and_then(|t| {
            let cap = generator::capacity(&cfg, &tracks, &today().to_string());
            cap.into_iter().find(|c| c.category == t.category).map(|c| c.spins_per_track_week)
        });
        let next_s = next.map(|(t, s)| format!("{} ({})", t.with_timezone(&Local).format("%Y-%m-%d %H:%M"), if s == "published" { "publikováno" } else { "koncept" }));
        let mut d = analytics::detail(&cfg, &spins, |s| s.track_id == Some(id), &format!("track:{id}"), Some((from, to)), next_s, expected);
        if d.name.is_empty() { if let Some(t) = me { d.name = format!("{} – {}", t.artist, t.title); } }
        let cat_hist: Vec<(String, String)> = {
            let mut v: Vec<(String, String)> = vec![];
            for s in spins.iter().filter(|s| s.track_id == Some(id)) { if v.last().map(|x| x.1 != s.category).unwrap_or(true) { v.push((s.started.with_timezone(&Local).format("%Y-%m-%d").to_string(), s.category.clone())); } }
            v
        };
        Ok(json!({ "report": d, "category_history": cat_hist, "attrs": me.map(|t| &t.attrs), "artists": me.map(|t| &t.artists), "work": me.map(|t| &t.work) }))
    }).await?;
    Ok(Json(v).into_response())
}
async fn an_artist(State(o): State<St>, Query(q): Query<DetQ>) -> ApiResult {
    let v = blocking(move || {
        let cfg = selector::load_config(&o.db)?.data;
        let (from, to) = det_range(&q);
        let key = orca_core::import::norm(q.name.as_deref().unwrap_or_default());
        if key.is_empty() { anyhow::bail!("zadejte interpreta"); }
        let lo = (today() - Duration::days(62)).min(from);
        let spins = spins_for(&o, &cfg, &analytics::Filter::default(), lo, today().max(to))?;
        let mut d = analytics::detail(&cfg, &spins, |s| analytics::artist_keys(s).contains(&key), &format!("artist:{key}"), Some((from, to)), None, None);
        if d.name.is_empty() { d.name = q.name.clone().unwrap_or_default(); }
        Ok(d)
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct RotQ { category: Option<String> }
/// Stav rotace skladeb kategorií formátu za posledních 7 dní (0 spinů / méně / nad cílem / nové / sezónní).
async fn an_rotation(State(o): State<St>, Query(q): Query<RotQ>) -> ApiResult {
    let v = blocking(move || {
        let cfg = selector::load_config(&o.db)?.data;
        let tracks = attrs::load_tracks(&o.db, &[orca_core::model::TrackKind::Song])?;
        let cap = generator::capacity(&cfg, &tracks, &today().to_string());
        let spins = analytics::load_spins(&o.db, &cfg, today() - Duration::days(6), today())?;
        let mut cnt: HashMap<i64, usize> = HashMap::new();
        for s in spins.iter().filter(|s| s.qualified) { if let Some(id) = s.track_id { *cnt.entry(id).or_default() += 1; } }
        let added: HashMap<i64, String> = o.db.list_tracks(&orca_core::db::TrackFilter { kind: Some("song".into()), include_disabled: Some(true), limit: Some(1_000_000), ..Default::default() })?.0.into_iter().map(|t| (t.id, t.added_at)).collect();
        let week_ago = (chrono::Utc::now() - Duration::days(7)).to_rfc3339();
        let rows: Vec<_> = tracks.iter().filter(|t| q.category.as_deref().map(|c| c.is_empty() || t.category == c).unwrap_or(true)).filter(|t| cfg.category(&t.category).is_some())
            .map(|t| {
                let e = cap.iter().find(|c| c.category == t.category).map(|c| c.spins_per_track_week).unwrap_or(0.0);
                let n = cnt.get(&t.id).copied().unwrap_or(0);
                let new = added.get(&t.id).map(|a| a.as_str() > week_ago.as_str()).unwrap_or(false);
                let seasonal = !t.attrs.active_from.is_empty() || !t.attrs.active_to.is_empty();
                let status = if !t.active_on(&today().to_string()) { "neaktivní" } else if new { "nově zařazená" } else if n == 0 { "0 spinů" } else if (n as f64) < e * 0.6 { "méně než očekáváno" } else if n as f64 > e * 1.4 { "nad cílem" } else { "v normě" };
                json!({ "id": t.id, "artist": t.artist, "title": t.title, "category": t.category, "spins_7d": n, "expected_7d": e, "status": status, "seasonal": seasonal })
            }).collect();
        Ok(rows)
    }).await?;
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct ExpQ { from: Option<String>, to: Option<String>, kind: Option<String>, category: Option<String>, basis: Option<String> }
async fn an_export(State(o): State<St>, Query(q): Query<ExpQ>) -> ApiResult {
    let s = blocking(move || {
        let cfg = selector::load_config(&o.db)?.data;
        let to = q.to.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()).unwrap_or_else(today);
        let from = q.from.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()).unwrap_or(to - Duration::days(6));
        let f = analytics::Filter { from: from.to_string(), to: to.to_string(), kind: q.kind, category: q.category, basis: q.basis, ..Default::default() };
        Ok(analytics::csv(&spins_for(&o, &cfg, &f, from, to)?))
    }).await?;
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()), (header::CONTENT_DISPOSITION, "attachment; filename=\"orca-as-played.csv\"".to_string())], s).into_response())
}

// ---------------------------------------------------------------------------------------------
// Automatická analýza BPM a energie

#[derive(Deserialize)]
struct AfReq { #[serde(default)] kinds: Vec<orca_core::model::TrackKind>, #[serde(default = "yes")] only_missing: bool }
fn yes() -> bool { true }
async fn af_start(State(o): State<St>, Json(r): Json<AfReq>) -> ApiResult {
    let kinds = if r.kinds.is_empty() { vec![orca_core::model::TrackKind::Song] } else { r.kinds };
    let db = o.db.clone();
    Ok(Json(blocking(move || selector::audiofeat::start(db, kinds, r.only_missing)).await?).into_response())
}
async fn af_status() -> ApiResult { Ok(Json(selector::audiofeat::status()).into_response()) }
async fn af_cancel() -> ApiResult { selector::audiofeat::cancel(); crate::ok() }

// ---------------------------------------------------------------------------------------------
// Hlášení OSA / INTERGRAM

use orca_core::cmo;

async fn cmo_get(State(o): State<St>) -> ApiResult {
    let c = cmo::load_config(&o.db)?;
    let cols: Vec<_> = cmo::COLUMN_KEYS.iter().map(|(k, l)| json!({ "key": k, "label": l })).collect();
    Ok(Json(json!({ "config": c, "columns": cols, "defaults": cmo::CmoConfig::default() })).into_response())
}
async fn cmo_put(State(o): State<St>, Json(c): Json<cmo::CmoConfig>) -> ApiResult {
    cmo::save_config(&o.db, &c).map_err(|e| bad(&e.to_string()))?;
    Ok(Json(c).into_response())
}
#[derive(Deserialize)]
struct CmoQ { month: Option<String>, profile: Option<String> }
fn prev_month() -> String { let t = today(); let d = t - Duration::days(t.format("%d").to_string().parse::<i64>().unwrap_or(1)); d.format("%Y-%m").to_string() }
async fn cmo_validate(State(o): State<St>, Query(q): Query<CmoQ>) -> ApiResult {
    let month = q.month.unwrap_or_else(prev_month);
    let which = q.profile.unwrap_or_else(|| "osa".into());
    let r = blocking(move || { let c = cmo::load_config(&o.db)?; let u = cmo::uses(&o.db, &month, &c)?; cmo::validate(&c, &which, &month, &u) }).await?;
    Ok(Json(r).into_response())
}
async fn cmo_export(State(o): State<St>, Path(which): Path<String>, Query(q): Query<CmoQ>) -> ApiResult {
    let which = which.trim_end_matches(".csv").to_string();
    if which != "osa" && which != "intergram" { return Err(bad("hlášení: osa | intergram")); }
    let month = q.month.unwrap_or_else(prev_month);
    let (w2, m2) = (which.clone(), month.clone());
    let (bytes, enc) = blocking(move || { let c = cmo::load_config(&o.db)?; let u = cmo::uses(&o.db, &m2, &c)?; let t = cmo::export_text(&c, &w2, &u)?; Ok(cmo::encode(&c, &t)) }).await?;
    let name = format!("attachment; filename=\"{}-{}.csv\"", which, month);
    Ok(([(header::CONTENT_TYPE, format!("text/csv; charset={enc}")), (header::CONTENT_DISPOSITION, name)], bytes).into_response())
}
