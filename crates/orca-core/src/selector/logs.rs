//! Plány (logy) dne: revize, koncept → publikováno, editor (výměna, přesun, vložení, zámek, přegenerování hodiny),
//! odbavení automatikou a historie pro generátor.

use super::attrs::{load_tracks, SelTrack};
use super::generator::{self, Ctx, DayInput, DayPlan, HourSpec, PlanHour, PlanItem, Placement};
use super::{load_config, SelectorConfig, Slot};
use crate::db::{now_utc, Db};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogInfo { pub id: i64, pub day: String, pub revision: i64, pub status: String, pub seed: i64, pub config_version: i64, pub token: i64, pub created_at: String, pub created_by: String, pub published_at: Option<String>, pub stats: serde_json::Value, pub note: String }

fn log_row(r: &rusqlite::Row) -> rusqlite::Result<LogInfo> {
    Ok(LogInfo { id: r.get(0)?, day: r.get(1)?, revision: r.get(2)?, status: r.get(3)?, seed: r.get(4)?, config_version: r.get(5)?, token: r.get(6)?, created_at: r.get(7)?, created_by: r.get(8)?, published_at: r.get(9)?, stats: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(), note: r.get(11)? })
}
const LOG_COLS: &str = "id,day,revision,status,seed,config_version,token,created_at,created_by,published_at,stats,note";

pub fn list_logs(db: &Db, from: &str, to: &str) -> Result<Vec<LogInfo>> {
    db.with_conn(|c| -> Result<Vec<LogInfo>> {
        let mut st = c.prepare(&format!("SELECT {LOG_COLS} FROM sel_logs WHERE day >= ?1 AND day <= ?2 ORDER BY day, revision DESC"))?;
        Ok(st.query_map([from, to], log_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn get_log(db: &Db, id: i64) -> Result<Option<LogInfo>> {
    db.with_conn(|c| Ok(c.query_row(&format!("SELECT {LOG_COLS} FROM sel_logs WHERE id=?1"), [id], log_row).optional()?))
}

/// Publikovaný plán dne (nebo None).
pub fn published(db: &Db, day: &str) -> Result<Option<LogInfo>> {
    db.with_conn(|c| Ok(c.query_row(&format!("SELECT {LOG_COLS} FROM sel_logs WHERE day=?1 AND status='published' ORDER BY revision DESC LIMIT 1"), [day], log_row).optional()?))
}

/// Nejnovější revize dne (koncept nebo publikovaná).
pub fn latest(db: &Db, day: &str) -> Result<Option<LogInfo>> {
    db.with_conn(|c| Ok(c.query_row(&format!("SELECT {LOG_COLS} FROM sel_logs WHERE day=?1 AND status<>'superseded' ORDER BY revision DESC LIMIT 1"), [day], log_row).optional()?))
}

pub fn load_items(db: &Db, log_id: i64) -> Result<Vec<(String, String, String, PlanItem)>> {
    db.with_conn(|c| -> Result<Vec<(String, String, String, PlanItem)>> {
        let mut st = c.prepare("SELECT id,hour_utc,hour_label,clock,pos,slot,track_id,origin,locked,score,breakdown,note,est_start,length_ms FROM sel_items WHERE log_id=?1 ORDER BY hour_utc, pos")?;
        let v = st.query_map([log_id], |r| {
            let it = PlanItem {
                id: Some(r.get(0)?), pos: r.get::<_, i64>(4)? as usize, slot: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(), track_id: r.get(6)?, origin: r.get(7)?,
                locked: r.get::<_, i64>(8)? != 0, score: r.get(9)?, breakdown: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(), note: r.get(11)?,
                est_start: DateTime::parse_from_rfc3339(&r.get::<_, String>(12)?).ok().map(|d| d.with_timezone(&Utc)), length_ms: r.get(13)?,
            };
            Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, it))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

/// Plán dne z databáze (hodiny + položky).
pub fn load_plan(db: &Db, log_id: i64) -> Result<DayPlan> {
    let info = get_log(db, log_id)?.ok_or_else(|| anyhow!("plán nenalezen"))?;
    let date = NaiveDate::parse_from_str(&info.day, "%Y-%m-%d")?;
    let mut hours: Vec<PlanHour> = vec![];
    for (hu, label, clock, it) in load_items(db, log_id)? {
        let utc = DateTime::parse_from_rfc3339(&hu)?.with_timezone(&Utc);
        if hours.last().map(|h| h.spec.hour_utc != utc).unwrap_or(true) {
            let local = utc.with_timezone(&Local);
            hours.push(PlanHour { spec: HourSpec { hour_utc: utc, date, hour: local.hour(), weekday: chrono::Datelike::weekday(&local).number_from_monday(), label: label.clone(), clock: clock.clone(), clock_source: String::new(), len_ms: 3_600_000 }, items: vec![], total_ms: 0, over_ms: 0, fixed_point: None });
        }
        hours.last_mut().unwrap().items.push(it);
    }
    for h in hours.iter_mut() {
        h.total_ms = h.items.iter().map(|i| i.length_ms).sum();
        h.over_ms = h.total_ms - h.spec.len_ms;
    }
    let stats = serde_json::from_value(info.stats.clone()).unwrap_or_default();
    Ok(DayPlan { day: info.day, hours, stats, seed: info.seed as u64 })
}

fn insert_items(tx: &rusqlite::Transaction, log_id: i64, plan: &DayPlan) -> Result<()> {
    let mut st = tx.prepare("INSERT INTO sel_items(log_id,hour_utc,hour_label,clock,pos,slot,track_id,origin,locked,score,breakdown,note,est_start,length_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)")?;
    for h in &plan.hours {
        for it in &h.items {
            st.execute(params![log_id, h.spec.hour_utc.to_rfc3339(), h.spec.label, h.spec.clock, it.pos as i64, serde_json::to_string(&it.slot)?, it.track_id, it.origin, it.locked as i64, it.score,
                serde_json::to_string(&it.breakdown)?, it.note, it.est_start.map(|d| d.to_rfc3339()).unwrap_or_default(), it.length_ms])?;
        }
    }
    Ok(())
}

/// Uloží plán jako novou revizi konceptu. Vrací id.
pub fn save_draft(db: &Db, plan: &DayPlan, config_version: i64, user: &str, note: &str) -> Result<i64> {
    db.with_conn(|c| -> Result<i64> {
        let tx = c.unchecked_transaction()?;
        let rev: i64 = tx.query_row("SELECT COALESCE(MAX(revision),0)+1 FROM sel_logs WHERE day=?1", [&plan.day], |r| r.get(0))?;
        // starší koncepty dne nahrazuje nový koncept (publikovaná revize zůstává až do publikace nové)
        tx.execute("UPDATE sel_logs SET status='superseded' WHERE day=?1 AND status='draft'", [&plan.day])?;
        tx.execute("INSERT INTO sel_logs(day,revision,status,seed,config_version,token,created_at,created_by,stats,note) VALUES(?1,?2,'draft',?3,?4,1,?5,?6,?7,?8)",
            params![plan.day, rev, plan.seed as i64, config_version, now_utc(), user, serde_json::to_string(&plan.stats)?, note])?;
        let id = tx.last_insert_rowid();
        insert_items(&tx, id, plan)?;
        tx.commit()?;
        Ok(id)
    })
}

/// Publikace: atomicky přepne aktivní plán dne. `token` chrání před souběžnou úpravou.
pub fn publish(db: &Db, log_id: i64, token: i64, user: &str) -> Result<LogInfo> {
    db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        let (day, status, tok): (String, String, i64) = tx.query_row("SELECT day,status,token FROM sel_logs WHERE id=?1", [log_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if status == "published" { return Ok(()); } // opakování je bezpečné
        if status != "draft" { return Err(anyhow!("CONFLICT: tuto revizi už nahradila novější")); }
        if tok != token { return Err(anyhow!("CONFLICT: plán mezitím upravil jiný uživatel – načtěte ho znovu")); }
        let unfilled: i64 = tx.query_row("SELECT COUNT(*) FROM sel_items WHERE log_id=?1 AND origin='unfilled'", [log_id], |r| r.get(0))?;
        tx.execute("UPDATE sel_logs SET status='superseded' WHERE day=?1 AND status='published'", [&day])?;
        tx.execute("UPDATE sel_logs SET status='published', published_at=?2, note=CASE WHEN ?3 > 0 THEN note || ' (publikováno s ' || ?3 || ' neobsazenými)' ELSE note END WHERE id=?1", params![log_id, now_utc(), unfilled])?;
        tx.execute("INSERT INTO audit_log(at,user,action,detail) VALUES(?1,?2,'selector_publish',?3)", params![now_utc(), user, format!("plán {day} (id {log_id})")])?;
        tx.commit()?;
        Ok(())
    })?;
    get_log(db, log_id)?.ok_or_else(|| anyhow!("plán nenalezen"))
}

/// Upravit lze jen koncept; z publikovaného se vytvoří nový koncept (kopie).
pub fn ensure_draft(db: &Db, log_id: i64, user: &str) -> Result<i64> {
    let info = get_log(db, log_id)?.ok_or_else(|| anyhow!("plán nenalezen"))?;
    if info.status == "draft" { return Ok(log_id); }
    if let Some(l) = latest(db, &info.day)? { if l.status == "draft" { return Ok(l.id); } }
    let plan = load_plan(db, log_id)?;
    save_draft(db, &plan, info.config_version, user, &format!("úprava revize {}", info.revision))
}

fn bump(db: &Db, log_id: i64, token: i64) -> Result<i64> {
    db.with_conn(|c| -> Result<i64> {
        let n = c.execute("UPDATE sel_logs SET token=token+1 WHERE id=?1 AND token=?2 AND status='draft'", params![log_id, token])?;
        if n == 0 { return Err(anyhow!("CONFLICT: plán mezitím upravil jiný uživatel nebo už není koncept – načtěte ho znovu")); }
        Ok(token + 1)
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    /// Výměna skladby (ruční výjimka z pravidel vyžaduje důvod)
    Replace { item_id: i64, track_id: i64, #[serde(default)] reason: String },
    Lock { item_id: i64, locked: bool },
    Clear { item_id: i64 },
    Move { item_id: i64, delta: i64 },
    Insert { after_item_id: i64, slot: Slot, track_id: Option<i64> },
    Delete { item_id: i64 },
}

/// Provede úpravu plánu (jen koncept, s kontrolou tokenu). Vrací nový token.
pub fn edit(db: &Db, log_id: i64, token: i64, e: &Edit, user: &str) -> Result<i64> {
    let new_token = bump(db, log_id, token)?;
    db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        let belongs = |id: i64| -> Result<(String, i64)> {
            Ok(tx.query_row("SELECT hour_utc,pos FROM sel_items WHERE id=?1 AND log_id=?2", params![id, log_id], |r| Ok((r.get(0)?, r.get(1)?)))?)
        };
        match e {
            Edit::Replace { item_id, track_id, reason } => {
                belongs(*item_id)?;
                let len: i64 = tx.query_row("SELECT CASE WHEN mix_ms>0 THEN mix_ms-cue_in_ms ELSE duration_ms-cue_in_ms END FROM tracks WHERE id=?1", [track_id], |r| r.get(0))?;
                tx.execute("UPDATE sel_items SET track_id=?2, origin='manual', locked=1, score=NULL, breakdown='[]', note=?3, length_ms=?4 WHERE id=?1", params![item_id, track_id, reason, len])?;
                if !reason.is_empty() { tx.execute("INSERT INTO audit_log(at,user,action,detail) VALUES(?1,?2,'selector_override',?3)", params![now_utc(), user, format!("položka {item_id} → skladba {track_id}: {reason}")])?; }
            }
            Edit::Lock { item_id, locked } => { belongs(*item_id)?; tx.execute("UPDATE sel_items SET locked=?2 WHERE id=?1", params![item_id, *locked as i64])?; }
            Edit::Clear { item_id } => { belongs(*item_id)?; tx.execute("UPDATE sel_items SET track_id=NULL, origin='unfilled', locked=0, score=NULL, note='vyprázdněno ručně' WHERE id=?1", [item_id])?; }
            Edit::Move { item_id, delta } => {
                let (hour, pos) = belongs(*item_id)?;
                let target = pos + delta;
                let other: Option<i64> = tx.query_row("SELECT id FROM sel_items WHERE log_id=?1 AND hour_utc=?2 AND pos=?3", params![log_id, hour, target], |r| r.get(0)).optional()?;
                if let Some(o) = other {
                    tx.execute("UPDATE sel_items SET pos=?2 WHERE id=?1", params![o, pos])?;
                    tx.execute("UPDATE sel_items SET pos=?2 WHERE id=?1", params![item_id, target])?;
                }
            }
            Edit::Insert { after_item_id, slot, track_id } => {
                let (hour, pos) = belongs(*after_item_id)?;
                let (label, clock): (String, String) = tx.query_row("SELECT hour_label, clock FROM sel_items WHERE id=?1", [after_item_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                tx.execute("UPDATE sel_items SET pos=pos+1 WHERE log_id=?1 AND hour_utc=?2 AND pos>?3", params![log_id, hour, pos])?;
                tx.execute("INSERT INTO sel_items(log_id,hour_utc,hour_label,clock,pos,slot,track_id,origin,locked,note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,'vloženo ručně')",
                    params![log_id, hour, label, clock, pos + 1, serde_json::to_string(slot)?, track_id, if track_id.is_some() { "manual" } else { "unfilled" }])?;
            }
            Edit::Delete { item_id } => {
                let (hour, pos) = belongs(*item_id)?;
                tx.execute("DELETE FROM sel_items WHERE id=?1", [item_id])?;
                tx.execute("UPDATE sel_items SET pos=pos-1 WHERE log_id=?1 AND hour_utc=?2 AND pos>?3", params![log_id, hour, pos])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    retime_log(db, log_id)?;
    // výměna/vyprázdnění skladby mění vlastnosti → jingly podle vlastností skladby se přepočítají pro hodinu
    let item = match e { Edit::Replace { item_id, .. } | Edit::Clear { item_id } => Some(*item_id), _ => None };
    if let Some(item_id) = item {
        let hour: Option<String> = db.with_conn(|c| -> Result<Option<String>> { Ok(c.query_row("SELECT hour_utc FROM sel_items WHERE id=?1", [item_id], |r| r.get(0)).optional()?) })?;
        if let Some(h) = hour { relink_hour(db, log_id, &h)?; }
    }
    Ok(new_token)
}

/// Znovu vloží automatické jingly (linkery) v jedné hodině konceptu podle aktuálních skladeb.
pub fn relink_hour(db: &Db, log_id: i64, hour_utc: &str) -> Result<()> {
    let cfg = load_config(db)?;
    if !cfg.data.linkers.iter().any(|l| l.enabled) { return Ok(()); }
    let tracks = load_tracks(db, &all_kinds(&cfg.data))?;
    let plan = load_plan(db, log_id)?;
    let hu = DateTime::parse_from_rfc3339(hour_utc)?.with_timezone(&Utc);
    let mut ctx = Ctx::new(&cfg.data, &tracks);
    for h in plan.hours.iter().filter(|h| h.spec.hour_utc != hu) {
        for it in &h.items { if let (Some(t), Some(at)) = (it.track_id, it.est_start) { ctx.add_history(&Placement { track_id: t, at: at.timestamp_millis(), date: h.spec.date, hour: h.spec.hour }); } }
    }
    let Some(mut hour) = plan.hours.into_iter().find(|h| h.spec.hour_utc == hu) else { return Ok(()); };
    let seed = cfg.data.seed ^ hu.timestamp() as u64;
    generator::apply_linkers(&mut ctx, &mut hour, seed);
    db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        tx.execute("DELETE FROM sel_items WHERE log_id=?1 AND hour_utc=?2", params![log_id, hour_utc])?;
        insert_items(&tx, log_id, &DayPlan { day: String::new(), hours: vec![hour], stats: Default::default(), seed: 0 })?;
        tx.commit()?;
        Ok(())
    })?;
    retime_log(db, log_id)
}

/// Přepočet odhadovaných časů a délek po úpravě.
pub fn retime_log(db: &Db, log_id: i64) -> Result<()> {
    let items = load_items(db, log_id)?;
    let lens: HashMap<i64, i64> = db.with_conn(|c| -> Result<HashMap<i64, i64>> {
        let mut st = c.prepare("SELECT id, CASE WHEN mix_ms>0 THEN mix_ms-cue_in_ms ELSE duration_ms-cue_in_ms END FROM tracks")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    })?;
    let mut by_hour: BTreeMap<String, Vec<PlanItem>> = BTreeMap::new();
    for (h, _, _, it) in items { by_hour.entry(h).or_default().push(it); }
    db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        for (h, mut its) in by_hour {
            its.sort_by_key(|i| i.pos);
            let start = DateTime::parse_from_rfc3339(&h)?.with_timezone(&Utc);
            let mut t = 0i64;
            for it in its {
                if let Some(m) = it.slot.at_min { t = t.max(m as i64 * 60_000); }
                let overlay = it.slot.kind == "imaging" && it.slot.position.as_deref().map(|p| p != "independent").unwrap_or(false);
                let len = if overlay { 0 } else { it.track_id.and_then(|id| lens.get(&id).copied()).unwrap_or(it.length_ms) };
                tx.execute("UPDATE sel_items SET est_start=?2, length_ms=?3 WHERE id=?1", params![it.id, (start + Duration::milliseconds(t)).to_rfc3339(), len])?;
                t += len;
            }
        }
        tx.commit()?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------
// Historie pro generátor

/// Skutečná hraní (as-played, hlavní výstup) v intervalu.
pub fn history(db: &Db, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Placement>> {
    let rows: Vec<(i64, String)> = db.with_conn(|c| -> Result<Vec<(i64, String)>> {
        let mut st = c.prepare("SELECT track_id, started_at FROM play_log WHERE track_id IS NOT NULL AND channel='main' AND started_at >= ?1 AND started_at < ?2")?;
        Ok(st.query_map([from.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(), to.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    Ok(rows.into_iter().filter_map(|(id, s)| {
        let t = DateTime::parse_from_rfc3339(&s).ok()?.with_timezone(&Utc);
        let l = t.with_timezone(&Local);
        Some(Placement { track_id: id, at: t.timestamp_millis(), date: l.date_naive(), hour: l.hour() })
    }).collect())
}

/// Plánované položky jiných dnů (nejnovější revize), které navazují na generovaný interval.
pub fn planned(db: &Db, from_day: &str, to_day: &str, exclude: &[String]) -> Result<Vec<Placement>> {
    let mut v = vec![];
    for l in list_logs(db, from_day, to_day)? {
        if exclude.contains(&l.day) || l.status == "superseded" { continue; }
        if latest(db, &l.day)?.map(|x| x.id) != Some(l.id) { continue; }
        for (_, _, _, it) in load_items(db, l.id)? {
            if let (Some(id), Some(t)) = (it.track_id, it.est_start) {
                if !(it.slot.kind == "music" || it.slot.kind == "fixed") { continue; }
                let lt = t.with_timezone(&Local);
                v.push(Placement { track_id: id, at: t.timestamp_millis(), date: lt.date_naive(), hour: lt.hour() });
            }
        }
    }
    Ok(v)
}

// ---------------------------------------------------------------------------------------------
// Generování intervalu na pozadí

#[derive(Debug, Clone, Serialize, Default)]
pub struct GenStatus { pub running: bool, pub from: String, pub to: String, pub done_days: usize, pub total_days: usize, pub log_ids: Vec<i64>, pub message: String, pub error: Option<String>, pub elapsed_ms: u64 }

fn status_cell() -> &'static (Mutex<GenStatus>, AtomicBool) {
    static S: OnceLock<(Mutex<GenStatus>, AtomicBool)> = OnceLock::new();
    S.get_or_init(|| (Mutex::new(GenStatus::default()), AtomicBool::new(false)))
}
pub fn gen_status() -> GenStatus { status_cell().0.lock().clone() }
pub fn cancel_gen() { status_cell().1.store(true, Ordering::Relaxed); }

/// Vygeneruje plány pro dny [from, to] v paměti (bez zápisu) – pro simulaci i pro skutečné generování.
pub fn plan_days(db: &Db, cfg: &SelectorConfig, tracks: &[SelTrack], from: NaiveDate, to: NaiveDate, keep_existing: bool, cancel: Option<&AtomicBool>, mut progress: impl FnMut(usize, &DayPlan)) -> Result<Vec<DayPlan>> {
    let start_utc = Local.from_local_datetime(&from.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|d| d.with_timezone(&Utc)).ok_or_else(|| anyhow!("neplatné datum"))?;
    let lookback = (cfg.rules.same_hour_days as i64).max(8);
    let days: Vec<String> = (0..=(to - from).num_days()).map(|i| (from + Duration::days(i)).format("%Y-%m-%d").to_string()).collect();
    let mut ctx = Ctx::new(cfg, tracks);
    let now = Utc::now();
    for p in history(db, start_utc - Duration::days(lookback), now.min(start_utc + Duration::days(days.len() as i64)))? { ctx.add_history(&p); }
    let pfrom = (from - Duration::days(lookback)).format("%Y-%m-%d").to_string();
    let pto = (to + Duration::days(2)).format("%Y-%m-%d").to_string();
    let today = Local::now().date_naive();
    for p in planned(db, &pfrom, &pto, &days)? { if p.date >= today { ctx.add_history(&p); } }
    let mut out = vec![];
    for (i, d) in days.iter().enumerate() {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) { break; }
        let date = NaiveDate::parse_from_str(d, "%Y-%m-%d")?;
        let mut keep = HashMap::new();
        if keep_existing {
            if let Some(l) = latest(db, d)? {
                let consumed = consumed_set(db, d)?;
                for (hu, _, _, it) in load_items(db, l.id)? {
                    let utc = DateTime::parse_from_rfc3339(&hu)?.with_timezone(&Utc);
                    let aired = consumed.contains(&(hu.clone(), it.pos as i64));
                    let protected = it.est_start.map(|t| t < now + Duration::minutes(cfg.protect_min as i64)).unwrap_or(false);
                    if it.locked || aired || protected || it.origin == "manual" { let mut k = it.clone(); if aired { k.locked = true; k.note = "odvysíláno".into(); } keep.insert((utc, it.pos), k); }
                }
            }
        }
        let seed = cfg.seed ^ (date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as u64);
        let plan = generator::generate_day(&mut ctx, DayInput { date, hours: generator::day_hours(&Local, date), keep, seed, cancel });
        progress(i, &plan);
        out.push(plan);
    }
    Ok(out)
}

/// Spustí generování na pozadí; výsledkem jsou koncepty (revize) pro každý den.
pub fn start_generate(db: Arc<Db>, from: NaiveDate, to: NaiveDate, user: String) -> Result<GenStatus> {
    {
        let st = status_cell().0.lock();
        if st.running { return Err(anyhow!("generování už probíhá")); }
    }
    if to < from || (to - from).num_days() > 62 { return Err(anyhow!("zvolte nejvýše 63 dní")); }
    let cfg = load_config(&db)?;
    if cfg.data.clocks.is_empty() { return Err(anyhow!("Selector nemá žádná schémata – nejdřív nastavte formát (průvodce).")); }
    status_cell().1.store(false, Ordering::Relaxed);
    *status_cell().0.lock() = GenStatus { running: true, from: from.to_string(), to: to.to_string(), total_days: (to - from).num_days() as usize + 1, message: "načítám knihovnu…".into(), ..Default::default() };
    std::thread::Builder::new().name("orca-selector".into()).spawn(move || {
        let t0 = std::time::Instant::now();
        let res: Result<()> = (|| {
            let tracks = load_tracks(&db, &all_kinds(&cfg.data))?;
            let db2 = db.clone();
            let plans = plan_days(&db, &cfg.data, &tracks, from, to, true, Some(&status_cell().1), |i, p| {
                let mut st = status_cell().0.lock();
                st.done_days = i + 1;
                st.message = format!("{}: {} pozic, {} neobsazeno", p.day, p.stats.slots, p.stats.unfilled);
                let _ = &db2;
            })?;
            for p in &plans {
                let id = save_draft(&db, p, cfg.version, &user, "vygenerováno")?;
                status_cell().0.lock().log_ids.push(id);
            }
            Ok(())
        })();
        let mut st = status_cell().0.lock();
        st.running = false;
        st.elapsed_ms = t0.elapsed().as_millis() as u64;
        if let Err(e) = res { st.error = Some(e.to_string()); }
        else if status_cell().1.load(Ordering::Relaxed) { st.message = "zrušeno".into(); }
        else { st.message = format!("hotovo za {:.1} s", st.elapsed_ms as f64 / 1000.0); }
    })?;
    Ok(gen_status())
}

pub fn all_kinds(cfg: &SelectorConfig) -> Vec<crate::model::TrackKind> {
    let mut k: Vec<_> = cfg.categories.iter().map(|c| c.kind).collect();
    for c in &cfg.clocks { for s in &c.slots { if let Some(t) = s.track_kind { k.push(t); } } }
    k.extend(cfg.music_kinds.iter().copied());
    k.push(crate::model::TrackKind::Jingle);
    k.push(crate::model::TrackKind::Sweeper);
    k.sort_by_key(|x| x.as_str());
    k.dedup();
    k
}

/// Přegeneruje vybrané hodiny konceptu (ostatní položky jsou pevný kontext).
pub fn regenerate_hours(db: &Db, log_id: i64, token: i64, hours: &[String], seed_bump: u64, user: &str) -> Result<(i64, i64)> {
    let draft = ensure_draft(db, log_id, user)?;
    let info = get_log(db, draft)?.ok_or_else(|| anyhow!("plán nenalezen"))?;
    if draft == log_id && info.token != token { return Err(anyhow!("CONFLICT: plán mezitím upravil jiný uživatel")); }
    let cfg = load_config(db)?;
    let tracks = load_tracks(db, &all_kinds(&cfg.data))?;
    let plan = load_plan(db, draft)?;
    let date = NaiveDate::parse_from_str(&plan.day, "%Y-%m-%d")?;
    let hours: Vec<DateTime<Utc>> = hours.iter().filter_map(|h| DateTime::parse_from_rfc3339(h).ok().map(|d| d.with_timezone(&Utc))).collect();
    if hours.is_empty() { return Err(anyhow!("vyberte hodiny k přegenerování")); }
    let mut keep = HashMap::new();
    for h in &plan.hours {
        let regen = hours.contains(&h.spec.hour_utc);
        for it in &h.items {
            if !regen || it.locked || it.origin == "manual" || it.origin == "fixed" { keep.insert((h.spec.hour_utc, it.pos), it.clone()); }
        }
    }
    let mut c2 = cfg.data.clone();
    c2.seed = c2.seed.wrapping_add(seed_bump);
    let mut ctx = Ctx::new(&c2, &tracks);
    for h in plan.hours.iter().filter(|h| hours.contains(&h.spec.hour_utc)) { for it in &h.items { if let Some(t) = it.track_id { ctx.avoid.insert(t); } } }
    let start_utc = plan.hours.first().map(|h| h.spec.hour_utc).unwrap_or_else(Utc::now);
    for p in history(db, start_utc - Duration::days(8), Utc::now().min(start_utc))? { ctx.add_history(&p); }
    let seed = c2.seed ^ (date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as u64);
    let new = generator::generate_day(&mut ctx, DayInput { date, hours: generator::day_hours(&Local, date), keep, seed, cancel: None });
    let id = save_draft(db, &new, cfg.version, user, "přegenerovány hodiny")?;
    let tok = get_log(db, id)?.map(|l| l.token).unwrap_or(1);
    Ok((id, tok))
}

// ---------------------------------------------------------------------------------------------
// Odbavení automatikou

pub fn consumed_set(db: &Db, day: &str) -> Result<std::collections::HashSet<(String, i64)>> {
    db.with_conn(|c| -> Result<std::collections::HashSet<(String, i64)>> {
        let mut st = c.prepare("SELECT hour_utc, pos FROM sel_consumed WHERE day=?1")?;
        Ok(st.query_map([day], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    })
}

pub enum Next { Track { item_id: i64, track_id: i64, slot: Slot, reason: String }, Stitch { kind: String, reason: String } }

/// Další položka publikovaného plánu pro čas `t` (místní). Položky dřívějších hodin, které nestihly odvysílat, se vypouští;
/// pevné body chrání hodina. None = plán neexistuje nebo je hodina vyčerpaná (automatika doplní z jednoduchých clocků).
pub fn take_next(db: &Db, t: DateTime<Local>) -> Result<Option<Next>> {
    let day = t.date_naive().format("%Y-%m-%d").to_string();
    let Some(log) = published(db, &day)? else { return Ok(None) };
    let tu = t.with_timezone(&Utc);
    let items = load_items(db, log.id)?;
    let consumed = consumed_set(db, &day)?;
    // aktuální hodina = poslední hour_utc <= t
    let mut hours: Vec<String> = items.iter().map(|x| x.0.clone()).collect();
    hours.dedup();
    let cur = hours.iter().rev().find(|h| DateTime::parse_from_rfc3339(h).map(|d| d.with_timezone(&Utc) <= tu).unwrap_or(false)).cloned();
    let Some(cur) = cur else { return Ok(None) };
    let cur_items: Vec<_> = items.iter().filter(|x| x.0 == cur).collect();
    let any_consumed = cur_items.iter().any(|(h, _, _, it)| consumed.contains(&(h.clone(), it.pos as i64)));
    for (k, (h, label, _, it)) in cur_items.iter().enumerate() {
        if consumed.contains(&(h.clone(), it.pos as i64)) { continue; }
        // start uprostřed hodiny (spuštění automatiky): položky, jejichž čas dávno minul, se vypustí – hodina pokračuje podle plánu
        if !any_consumed && k + 1 < cur_items.len() && it.est_start.map(|e| e + Duration::minutes(10) < tu).unwrap_or(false) { continue; }
        let id = it.id.unwrap_or(0);
        db.with_conn(|c| c.execute("INSERT OR IGNORE INTO sel_consumed(day,hour_utc,pos,at) VALUES(?1,?2,?3,?4)", params![day, h, it.pos as i64, now_utc()]))?;
        let reason = format!("Selector {} {} #{} ({})", day, label, it.pos + 1, match it.origin.as_str() { "manual" => "ruční výběr", "fallback" => "náhradní kategorie", "fixed" => "pevný titul", _ => "plán" });
        match it.slot.kind.as_str() {
            "music" | "fixed" | "imaging" => if let Some(tid) = it.track_id { return Ok(Some(Next::Track { item_id: id, track_id: tid, slot: it.slot.clone(), reason })); },
            "news" | "stitch" => if let Some(k) = it.slot.stitch.clone().or(Some("news".into()).filter(|_| it.slot.kind == "news")) { return Ok(Some(Next::Stitch { kind: k, reason })); },
            _ => {}
        }
    }
    Ok(None)
}

/// Plán vs. skutečnost pro den: položky plánu se stavem odvysílání.
#[derive(Debug, Clone, Serialize)]
pub struct Reconcile { pub item_id: i64, pub hour: String, pub pos: usize, pub planned_track: Option<i64>, pub aired_track: Option<i64>, pub status: String, pub started_at: Option<String>, pub played_ms: Option<i64> }

pub fn reconcile(db: &Db, day: &str) -> Result<Vec<Reconcile>> {
    let Some(log) = published(db, day)? else { return Ok(vec![]) };
    let aired: HashMap<i64, (Option<i64>, String, Option<i64>)> = db.with_conn(|c| -> Result<HashMap<i64, (Option<i64>, String, Option<i64>)>> {
        let mut st = c.prepare("SELECT plan_item_id, track_id, started_at, played_ms FROM play_log WHERE plan_item_id IS NOT NULL AND started_at >= ?1 AND started_at < ?2")?;
        let from = format!("{}T00:00:00", (NaiveDate::parse_from_str(day, "%Y-%m-%d")? - Duration::days(1)).format("%Y-%m-%d"));
        let to = format!("{}T00:00:00", (NaiveDate::parse_from_str(day, "%Y-%m-%d")? + Duration::days(2)).format("%Y-%m-%d"));
        Ok(st.query_map([from, to], |r| Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.collect::<rusqlite::Result<_>>()?)
    })?;
    let consumed = consumed_set(db, day)?;
    let now = Utc::now();
    Ok(load_items(db, log.id)?.into_iter().filter(|(_, _, _, it)| matches!(it.slot.kind.as_str(), "music" | "fixed" | "imaging")).map(|(h, label, _, it)| {
        let id = it.id.unwrap_or(0);
        let a = aired.get(&id);
        let status = match a {
            Some((tid, _, _)) if *tid == it.track_id => "odvysíláno",
            Some(_) => "jiná skladba",
            None if consumed.contains(&(h.clone(), it.pos as i64)) => "zařazeno, nepotvrzeno",
            None if it.est_start.map(|t| t < now).unwrap_or(false) => "vypuštěno",
            None => "čeká",
        };
        Reconcile { item_id: id, hour: label, pos: it.pos, planned_track: it.track_id, aired_track: a.and_then(|x| x.0), status: status.into(), started_at: a.map(|x| x.1.clone()), played_ms: a.and_then(|x| x.2) }
    }).collect())
}
