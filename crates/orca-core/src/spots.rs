//! Spots/Commercials: kampaně, reklamní bloky, priority, programování (týdenní, detailní, playlister),
//! sestavení bloku, import plánů z traffic softwaru a reporty.

use crate::config::{BreakDef, Config};
use crate::db::{now_utc, row_track, Db, TRACK_COLS};
use crate::model::{Track, TrackKind};
use anyhow::{anyhow, Result};
use chrono::{Datelike, NaiveDate};
use rand::seq::SliceRandom;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CampaignSpot {
    pub track_id: i64,
    pub ord: i64,
    #[serde(skip_deserializing)]
    pub track: Option<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Slot {
    /// Den v týdnu 1 = Po … 7 = Ne (týdenní programování)
    pub weekday: Option<i64>,
    /// "YYYY-MM-DD" (detailní programování)
    pub date: Option<String>,
    /// "HH:MM" – čas reklamního bloku
    pub time: String,
    /// Konkrétní spot ("in specific order"); None = podle systému kampaně
    pub spot_track_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Campaign {
    pub id: i64,
    pub name: String,
    pub client: String,
    pub category: String,
    /// Obchodní reklama (rozhoduje o přehrání "optional" priorit a o přehledu reklam).
    pub is_commercial: bool,
    /// "YYYY-MM-DD HH:MM" nebo None = od teď
    pub start_at: Option<String>,
    /// "YYYY-MM-DD HH:MM" nebo None = nikdy (trvalá kampaň – station ID, upoutávky)
    pub expires_at: Option<String>,
    /// weekly | detailed | playlister
    pub mode: String,
    /// specific | order | random
    pub system: String,
    pub priority: i64,
    pub enabled: bool,
    pub notes: String,
    pub spots: Vec<CampaignSpot>,
    pub slots: Vec<Slot>,
    #[serde(skip_deserializing)]
    pub plays: i64,
    #[serde(skip_deserializing)]
    pub created_at: String,
    #[serde(skip_deserializing)]
    pub updated_at: String,
}

impl Default for Campaign {
    fn default() -> Self {
        Self { id: 0, name: String::new(), client: String::new(), category: "Reklama".into(), is_commercial: true, start_at: None, expires_at: None,
            mode: "weekly".into(), system: "order".into(), priority: 4, enabled: true, notes: String::new(), spots: vec![], slots: vec![], plays: 0,
            created_at: String::new(), updated_at: String::new() }
    }
}

const CAMP_COLS: &str = "id,name,client,category,is_commercial,start_at,expires_at,mode,system,priority,enabled,notes,created_at,updated_at";

fn row_campaign(r: &rusqlite::Row) -> rusqlite::Result<Campaign> {
    Ok(Campaign {
        id: r.get(0)?, name: r.get(1)?, client: r.get(2)?, category: r.get(3)?, is_commercial: r.get::<_, i64>(4)? != 0,
        start_at: r.get(5)?, expires_at: r.get(6)?, mode: r.get(7)?, system: r.get(8)?, priority: r.get(9)?, enabled: r.get::<_, i64>(10)? != 0,
        notes: r.get(11)?, created_at: r.get(12)?, updated_at: r.get(13)?, spots: vec![], slots: vec![], plays: 0,
    })
}

fn load_details(c: &rusqlite::Connection, camp: &mut Campaign) -> rusqlite::Result<()> {
    let mut st = c.prepare("SELECT track_id, ord FROM campaign_spots WHERE campaign_id=?1 ORDER BY ord, track_id")?;
    let spots = st.query_map([camp.id], |r| Ok(CampaignSpot { track_id: r.get(0)?, ord: r.get(1)?, track: None }))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tst = c.prepare(&format!("SELECT {TRACK_COLS} FROM tracks WHERE id=?1"))?;
    camp.spots = spots.into_iter().map(|mut s| { s.track = tst.query_row([s.track_id], row_track).optional().ok().flatten(); s }).collect();
    let mut st = c.prepare("SELECT weekday, date, break_time, spot_track_id FROM campaign_slots WHERE campaign_id=?1 ORDER BY date, weekday, break_time")?;
    camp.slots = st.query_map([camp.id], |r| Ok(Slot { weekday: r.get(0)?, date: r.get(1)?, time: r.get(2)?, spot_track_id: r.get(3)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let ids: Vec<String> = camp.spots.iter().map(|s| s.track_id.to_string()).collect();
    camp.plays = if ids.is_empty() { 0 } else {
        c.query_row(&format!("SELECT COUNT(*) FROM play_log WHERE track_id IN ({}) AND started_at >= ?1", ids.join(",")), [&camp.created_at], |r| r.get(0))?
    };
    Ok(())
}

/// `status`: None = vše, "active" = platné teď, "expired", "future"
pub fn list_campaigns(db: &Db, status: Option<&str>, now_local: &str) -> Result<Vec<Campaign>> {
    db.with_conn(|c| {
        let mut st = c.prepare(&format!("SELECT {CAMP_COLS} FROM campaigns ORDER BY enabled DESC, name COLLATE NOCASE"))?;
        let mut v = st.query_map([], row_campaign)?.collect::<rusqlite::Result<Vec<_>>>()?;
        v.retain(|cp| match status {
            Some("active") => cp.enabled && cp.start_at.as_deref().map(|s| s <= now_local).unwrap_or(true) && cp.expires_at.as_deref().map(|e| e > now_local).unwrap_or(true),
            Some("expired") => cp.expires_at.as_deref().map(|e| e <= now_local).unwrap_or(false),
            Some("future") => cp.start_at.as_deref().map(|s| s > now_local).unwrap_or(false),
            _ => true,
        });
        for cp in v.iter_mut() { load_details(c, cp)?; }
        Ok(v)
    })
}

pub fn get_campaign(db: &Db, id: i64) -> Result<Option<Campaign>> {
    db.with_conn(|c| {
        let Some(mut cp) = c.query_row(&format!("SELECT {CAMP_COLS} FROM campaigns WHERE id=?1"), [id], row_campaign).optional()? else { return Ok(None) };
        load_details(c, &mut cp)?;
        Ok(Some(cp))
    })
}

fn valid_time(t: &str) -> bool {
    let b = t.as_bytes();
    t.len() == 5 && b[2] == b':' && t[..2].parse::<u32>().map(|h| h < 24).unwrap_or(false) && t[3..].parse::<u32>().map(|m| m < 60).unwrap_or(false)
}

pub fn save_campaign(db: &Db, cp: &Campaign) -> Result<Campaign> {
    if cp.name.trim().is_empty() { return Err(anyhow!("kampaň musí mít název")); }
    if !["weekly", "detailed", "playlister"].contains(&cp.mode.as_str()) { return Err(anyhow!("mode: weekly | detailed | playlister")); }
    if !["specific", "order", "random"].contains(&cp.system.as_str()) { return Err(anyhow!("system: specific | order | random")); }
    for s in &cp.slots {
        if !valid_time(&s.time) { return Err(anyhow!("neplatný čas bloku '{}'", s.time)); }
    }
    let now = now_utc();
    let id = db.with_conn(|c| -> Result<i64> {
        let tx = c.unchecked_transaction()?;
        let id = if cp.id > 0 {
            tx.execute("UPDATE campaigns SET name=?2, client=?3, category=?4, is_commercial=?5, start_at=?6, expires_at=?7, mode=?8, system=?9, priority=?10, enabled=?11, notes=?12, updated_at=?13 WHERE id=?1",
                params![cp.id, cp.name.trim(), cp.client, cp.category, cp.is_commercial as i64, cp.start_at, cp.expires_at, cp.mode, cp.system, cp.priority, cp.enabled as i64, cp.notes, now])?;
            cp.id
        } else {
            tx.execute("INSERT INTO campaigns(name,client,category,is_commercial,start_at,expires_at,mode,system,priority,enabled,notes,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
                params![cp.name.trim(), cp.client, cp.category, cp.is_commercial as i64, cp.start_at, cp.expires_at, cp.mode, cp.system, cp.priority, cp.enabled as i64, cp.notes, now])?;
            tx.last_insert_rowid()
        };
        tx.execute("DELETE FROM campaign_spots WHERE campaign_id=?1", [id])?;
        for (i, s) in cp.spots.iter().enumerate() {
            tx.execute("INSERT OR IGNORE INTO campaign_spots(campaign_id,track_id,ord) VALUES(?1,?2,?3)", params![id, s.track_id, i as i64])?;
        }
        tx.execute("DELETE FROM campaign_slots WHERE campaign_id=?1", [id])?;
        for s in &cp.slots {
            tx.execute("INSERT INTO campaign_slots(campaign_id,weekday,date,break_time,spot_track_id) VALUES(?1,?2,?3,?4,?5)", params![id, s.weekday, s.date, s.time, s.spot_track_id])?;
        }
        tx.commit()?;
        Ok(id)
    })?;
    get_campaign(db, id)?.ok_or_else(|| anyhow!("kampaň zmizela"))
}

pub fn delete_campaign(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| {
        c.execute("DELETE FROM campaign_slots WHERE campaign_id=?1", [id])?;
        c.execute("DELETE FROM campaign_spots WHERE campaign_id=?1", [id])?;
        c.execute("DELETE FROM campaigns WHERE id=?1", [id])?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------
// Reklamní blok

#[derive(Debug, Clone, Serialize)]
pub struct BreakItem {
    pub track: Track,
    pub campaign_id: Option<i64>,
    pub campaign: String,
    pub priority: i64,
    pub priority_name: String,
    pub optional: bool,
    pub is_commercial: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakBlock {
    pub date: String,
    pub time: String,
    pub name: String,
    pub position: String,
    pub start_immediately: bool,
    pub items: Vec<BreakItem>,
    pub duration_ms: i64,
    pub commercial_ms: i64,
    pub max_duration_s: u32,
    pub exceeds: bool,
    pub inserted: bool,
    /// Posun položky s prioritou "ohlášení času" od začátku bloku (ms).
    pub time_offset_ms: i64,
}

fn spot_len(t: &Track) -> i64 { let (ci, _, co) = t.effective_cues(); (co - ci).max(0) }

pub fn break_key(date: &str, time: &str) -> String { format!("{date} {time}") }

/// Sestaví reklamní blok pro datum a čas. `commit` = posunout rotaci kampaní (při skutečném vložení).
pub fn build_block(db: &Db, cfg: &Config, date: NaiveDate, def: &BreakDef, commit: bool) -> Result<BreakBlock> {
    let date_s = date.format("%Y-%m-%d").to_string();
    let dt = format!("{date_s} {}", def.time);
    let weekday = date.weekday().number_from_monday() as i64;
    let sp = &cfg.spots;
    let mut items: Vec<BreakItem> = vec![];
    db.with_conn(|c| -> Result<()> {
        // týdenní a detailní programování
        let mut st = c.prepare(&format!(
            "SELECT DISTINCT c.{} , s.spot_track_id FROM campaigns c JOIN campaign_slots s ON s.campaign_id=c.id
             WHERE c.enabled=1 AND s.break_time=?1
               AND ((c.mode='weekly' AND s.weekday=?2) OR (c.mode='detailed' AND s.date=?3))
               AND (c.start_at IS NULL OR c.start_at <= ?4) AND (c.expires_at IS NULL OR c.expires_at > ?4)",
            CAMP_COLS.replace(',', ",c.")))?;
        let rows = st.query_map(params![def.time, weekday, date_s, dt], |r| Ok((row_campaign(r)?, r.get::<_, Option<i64>>(14)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tst = c.prepare(&format!("SELECT {TRACK_COLS} FROM tracks WHERE id=?1 AND enabled=1 AND missing=0"))?;
        let mut seen = std::collections::HashSet::new();
        for (camp, specific) in rows {
            if !seen.insert((camp.id, specific)) { continue; }
            let mut sst = c.prepare("SELECT track_id FROM campaign_spots WHERE campaign_id=?1 ORDER BY ord, track_id")?;
            let spot_ids: Vec<i64> = sst.query_map([camp.id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let pick = match (specific, camp.system.as_str()) {
                (Some(t), _) => Some(t),
                (None, "random") => spot_ids.choose(&mut rand::thread_rng()).copied(),
                (None, "order") if !spot_ids.is_empty() => {
                    let rr: i64 = c.query_row("SELECT rr_index FROM campaigns WHERE id=?1", [camp.id], |r| r.get(0))?;
                    if commit { c.execute("UPDATE campaigns SET rr_index=rr_index+1 WHERE id=?1", [camp.id])?; }
                    Some(spot_ids[(rr.rem_euclid(spot_ids.len() as i64)) as usize])
                }
                _ => spot_ids.first().copied(),
            };
            let Some(tid) = pick else { continue };
            let Some(track) = tst.query_row([tid], row_track).optional()? else { continue };
            let pdef = sp.priority(camp.priority as u32);
            items.push(BreakItem {
                track, campaign_id: Some(camp.id), campaign: camp.name.clone(), priority: camp.priority,
                priority_name: pdef.map(|p| p.name.clone()).unwrap_or_default(),
                optional: pdef.map(|p| p.special == "optional").unwrap_or(false), is_commercial: camp.is_commercial,
            });
        }
        // playlister (traffic) – plán z TXT
        let pl_prio = sp.special_priority("playlister").unwrap_or(5) as i64;
        let pl_name = sp.priority(pl_prio as u32).map(|p| p.name.clone()).unwrap_or_default();
        let mut st = c.prepare("SELECT track_id, title FROM break_playlist WHERE date=?1 AND break_time=?2 ORDER BY seq")?;
        let pl = st.query_map(params![date_s, def.time], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (tid, _title) in pl {
            let Some(tid) = tid else { continue };
            let Some(track) = tst.query_row([tid], row_track).optional()? else { continue };
            let camp: Option<(i64, String, i64)> = c.query_row(
                "SELECT c.id, c.name, c.is_commercial FROM campaigns c JOIN campaign_spots s ON s.campaign_id=c.id WHERE s.track_id=?1 AND c.enabled=1 LIMIT 1",
                [tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            items.push(BreakItem {
                track, campaign_id: camp.as_ref().map(|c| c.0), campaign: camp.as_ref().map(|c| c.1.clone()).unwrap_or_else(|| "Traffic".into()),
                priority: pl_prio, priority_name: pl_name.clone(), optional: false, is_commercial: camp.map(|c| c.2 != 0).unwrap_or(true),
            });
        }
        Ok(())
    })?;
    // stabilní pořadí: priorita, pak pořadí vložení
    items.sort_by_key(|i| i.priority);
    // "optional" priority hrají jen tehdy, když v bloku je obchodní reklama
    let has_commercial = items.iter().any(|i| i.is_commercial && !i.optional);
    if !has_commercial { items.retain(|i| !i.optional); }
    let duration_ms: i64 = items.iter().map(|i| spot_len(&i.track)).sum();
    let commercial_ms: i64 = items.iter().filter(|i| i.is_commercial).map(|i| spot_len(&i.track)).sum();
    let time_prio = sp.special_priority("time").map(|p| p as i64);
    let time_offset_ms = match time_prio {
        Some(tp) => items.iter().take_while(|i| i.priority != tp).map(|i| spot_len(&i.track)).sum(),
        None => 0,
    };
    let inserted = db.with_conn(|c| c.query_row("SELECT 1 FROM breaks_inserted WHERE date=?1 AND break_time=?2", params![date_s, def.time], |_| Ok(())).optional())?.is_some();
    let wd = (weekday - 1) as usize;
    Ok(BreakBlock {
        date: date_s, time: def.time.clone(), name: def.name.clone(), position: def.position.clone(),
        start_immediately: def.start_immediately.get(wd).copied().unwrap_or(false),
        exceeds: def.max_duration_s > 0 && duration_ms > def.max_duration_s as i64 * 1000,
        max_duration_s: def.max_duration_s, items, duration_ms, commercial_ms, inserted, time_offset_ms,
    })
}

/// Všechny bloky dne (přehled "breaks view").
pub fn day_blocks(db: &Db, cfg: &Config, date: NaiveDate, only_nonempty: bool) -> Result<Vec<BreakBlock>> {
    let mut breaks = cfg.spots.breaks.clone();
    breaks.sort_by(|a, b| a.time.cmp(&b.time));
    let mut v = vec![];
    for b in breaks.iter().filter(|b| valid_time(&b.time)) {
        let blk = build_block(db, cfg, date, b, false)?;
        if !only_nonempty || !blk.items.is_empty() { v.push(blk); }
    }
    Ok(v)
}

pub fn mark_inserted(db: &Db, date: &str, time: &str) -> Result<()> {
    db.with_conn(|c| { c.execute("INSERT OR IGNORE INTO breaks_inserted(date,break_time,inserted_at) VALUES(?1,?2,?3)", params![date, time, now_utc()])?; Ok(()) })
}

pub fn unmark_inserted(db: &Db, date: &str, time: &str) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM breaks_inserted WHERE date=?1 AND break_time=?2", params![date, time])?; Ok(()) })
}

// ---------------------------------------------------------------------------------------------
// Playlister / traffic: TXT "TIME,PLAYLISTER CODE,TITLE"

#[derive(Debug, Default, Serialize)]
pub struct PlaylisterImport {
    pub date: String,
    pub imported: usize,
    pub errors: Vec<String>,
    pub aborted: bool,
}

/// Datum z názvu souboru YYYYMMDD.txt / .asc
pub fn date_from_filename(name: &str) -> Option<NaiveDate> {
    let stem: String = name.chars().filter(|c| c.is_ascii_digit()).take(8).collect();
    NaiveDate::parse_from_str(&stem, "%Y%m%d").ok()
}

pub fn import_playlister(db: &Db, cfg: &Config, date: NaiveDate, text: &str, verify: bool) -> Result<PlaylisterImport> {
    let date_s = date.format("%Y-%m-%d").to_string();
    let times: std::collections::HashSet<&str> = cfg.spots.breaks.iter().map(|b| b.time.as_str()).collect();
    let mut rows = vec![];
    let mut rep = PlaylisterImport { date: date_s.clone(), ..Default::default() };
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') { continue; }
        let mut parts = line.splitn(3, ',');
        let time_raw = parts.next().unwrap_or("").trim();
        let code = parts.next().unwrap_or("").trim().to_string();
        let title = parts.next().unwrap_or("").trim().to_string();
        let time = if time_raw.len() >= 5 { format!("{:0>5}", &time_raw[..time_raw.len().min(5)]) } else { format!("{:0>5}", time_raw) };
        if !times.contains(time.as_str()) { rep.errors.push(format!("řádek {}: čas {time} není reklamní blok", ln + 1)); continue; }
        if code.is_empty() || code.contains('*') || code.contains('%') { rep.errors.push(format!("řádek {}: neplatný kód '{code}'", ln + 1)); continue; }
        let track = db.track_by_code(&code, &cfg.library.prefixes)?;
        if track.is_none() { rep.errors.push(format!("řádek {}: spot s kódem {code} není v knihovně", ln + 1)); }
        rows.push((time, track.map(|t| t.id), code, title));
    }
    if verify && !rep.errors.is_empty() { rep.aborted = true; return Ok(rep); }
    db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        tx.execute("DELETE FROM break_playlist WHERE date=?1", [&date_s])?; // nový import téhož dne nahradí předchozí
        for (i, (time, tid, code, title)) in rows.iter().enumerate() {
            tx.execute("INSERT INTO break_playlist(date,break_time,seq,track_id,code,title) VALUES(?1,?2,?3,?4,?5,?6)", params![date_s, time, i as i64, tid, code, title])?;
        }
        tx.commit()?;
        Ok(())
    })?;
    rep.imported = rows.len();
    Ok(rep)
}

/// Export plánu bloků dne ve formátu TIME,CODE,TITLE.
pub fn export_breaks_playlist(db: &Db, cfg: &Config, date: NaiveDate) -> Result<String> {
    let mut out = String::new();
    for b in day_blocks(db, cfg, date, true)? {
        for i in b.items { out += &format!("{},{},{}\r\n", b.time, crate::library::playlister_code(cfg, &i.track), i.track.display()); }
    }
    Ok(out)
}

/// Export délek bloků: TIME,SEKUNDY
pub fn export_breaks_duration(db: &Db, cfg: &Config, date: NaiveDate) -> Result<String> {
    let mut out = String::new();
    for b in day_blocks(db, cfg, date, false)? { out += &format!("{},{}\r\n", b.time, (b.duration_ms as f64 / 1000.0).round() as i64); }
    Ok(out)
}

/// Vypne playlister spoty, které nebyly naplánované posledních N dní (Jazler: Clean up old spots).
pub fn cleanup_old_playlister_spots(db: &Db, days: i64) -> Result<usize> {
    let since = (chrono::Local::now().date_naive() - chrono::Duration::days(days)).format("%Y-%m-%d").to_string();
    db.with_conn(|c| {
        Ok(c.execute(
            "UPDATE tracks SET enabled=0 WHERE kind=?1 AND id IN (SELECT DISTINCT track_id FROM break_playlist WHERE track_id IS NOT NULL)
             AND id NOT IN (SELECT DISTINCT track_id FROM break_playlist WHERE date >= ?2 AND track_id IS NOT NULL)",
            params![TrackKind::Spot.as_str(), since])?)
    })
}

// ---------------------------------------------------------------------------------------------
// Presety programování

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Preset { pub id: i64, pub name: String, pub slots: Vec<Slot> }

pub fn list_presets(db: &Db) -> Result<Vec<Preset>> {
    db.with_conn(|c| {
        let mut st = c.prepare("SELECT id,name,slots FROM spot_presets ORDER BY name")?;
        let v = st.query_map([], |r| Ok(Preset { id: r.get(0)?, name: r.get(1)?, slots: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default() }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

pub fn save_preset(db: &Db, p: &Preset) -> Result<i64> {
    let slots = serde_json::to_string(&p.slots)?;
    db.with_conn(|c| {
        if p.id > 0 { c.execute("UPDATE spot_presets SET name=?2, slots=?3 WHERE id=?1", params![p.id, p.name, slots])?; Ok(p.id) }
        else { c.execute("INSERT INTO spot_presets(name,slots) VALUES(?1,?2)", params![p.name, slots])?; Ok(c.last_insert_rowid()) }
    })
}

pub fn delete_preset(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM spot_presets WHERE id=?1", [id])?; Ok(()) })
}

// ---------------------------------------------------------------------------------------------
// Reporty (CSV, UTF-8 s BOM, středník)

fn csv(fields: &[String]) -> String {
    fields.iter().map(|f| if f.contains(';') || f.contains('"') { format!("\"{}\"", f.replace('"', "\"\"")) } else { f.clone() }).collect::<Vec<_>>().join(";") + "\r\n"
}

fn local(iso: &str) -> (String, String) {
    chrono::DateTime::parse_from_rfc3339(iso).map(|d| { let l = d.with_timezone(&chrono::Local); (l.format("%d.%m.%Y").to_string(), l.format("%H:%M:%S").to_string()) })
        .unwrap_or_default()
}

/// Skutečně odvysílané spoty kampaně.
pub fn report_campaign_plays(db: &Db, id: i64) -> Result<String> {
    let cp = get_campaign(db, id)?.ok_or_else(|| anyhow!("kampaň nenalezena"))?;
    let mut out = format!("\u{feff}Kampaň;{}\r\nKlient;{}\r\n\r\n", cp.name, cp.client);
    out += "Datum;Čas;Spot;Odvysíláno (s)\r\n";
    let ids: Vec<String> = cp.spots.iter().map(|s| s.track_id.to_string()).collect();
    if ids.is_empty() { return Ok(out); }
    db.with_conn(|c| -> Result<()> {
        let mut st = c.prepare(&format!("SELECT started_at, artist, title, played_ms FROM play_log WHERE track_id IN ({}) AND started_at >= ?1 ORDER BY started_at", ids.join(",")))?;
        let rows = st.query_map([&cp.created_at], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?)))?;
        let mut n = 0;
        for r in rows {
            let (at, a, t, p) = r?;
            let (d, tm) = local(&at);
            out += &csv(&[d, tm, if a.is_empty() { t } else { format!("{a} - {t}") }, p.map(|x| ((x as f64) / 1000.0).round().to_string()).unwrap_or_default()]);
            n += 1;
        }
        out += &format!("\r\nCelkem;{n}\r\n");
        Ok(())
    })?;
    Ok(out)
}

/// Plán (programování) kampaně.
pub fn report_campaign_schedule(db: &Db, id: i64) -> Result<String> {
    let cp = get_campaign(db, id)?.ok_or_else(|| anyhow!("kampaň nenalezena"))?;
    let days = ["", "Po", "Út", "St", "Čt", "Pá", "So", "Ne"];
    let mut out = format!("\u{feff}Kampaň;{}\r\nKlient;{}\r\nPlatnost;{} – {}\r\nRežim;{}\r\n\r\nDen/Datum;Blok;Spot\r\n",
        cp.name, cp.client, cp.start_at.clone().unwrap_or_else(|| "ihned".into()), cp.expires_at.clone().unwrap_or_else(|| "bez omezení".into()), cp.mode);
    for s in &cp.slots {
        let day = s.date.clone().unwrap_or_else(|| s.weekday.map(|w| days[w.clamp(1, 7) as usize].to_string()).unwrap_or_default());
        let spot = s.spot_track_id.and_then(|t| cp.spots.iter().find(|x| x.track_id == t)).and_then(|x| x.track.as_ref()).map(|t| t.display()).unwrap_or_else(|| "(podle systému kampaně)".into());
        out += &csv(&[day, s.time.clone(), spot]);
    }
    Ok(out)
}

/// Kampaně běžící mezi daty.
pub fn report_running(db: &Db, from: &str, to: &str) -> Result<String> {
    let mut out = String::from("\u{feff}Kampaň;Klient;Kategorie;Od;Do;Spoty;Reklama\r\n");
    for cp in list_campaigns(db, None, "")? {
        let starts_before_end = cp.start_at.as_deref().map(|s| s <= to).unwrap_or(true);
        let ends_after_start = cp.expires_at.as_deref().map(|e| e >= from).unwrap_or(true);
        if !(starts_before_end && ends_after_start) { continue; }
        let spots = cp.spots.iter().filter_map(|s| s.track.as_ref().map(|t| t.display())).collect::<Vec<_>>().join(" | ");
        out += &csv(&[cp.name, cp.client, cp.category, cp.start_at.unwrap_or_default(), cp.expires_at.unwrap_or_else(|| "bez omezení".into()), spots, if cp.is_commercial { "ano".into() } else { "ne".into() }]);
    }
    Ok(out)
}

/// Denní program reklamních bloků.
pub fn report_daily(db: &Db, cfg: &Config, date: NaiveDate) -> Result<String> {
    let mut out = format!("\u{feff}Den;{}\r\n\r\nBlok;Priorita;Kampaň;Spot;Délka (s)\r\n", date.format("%d.%m.%Y"));
    for b in day_blocks(db, cfg, date, true)? {
        for i in &b.items {
            out += &csv(&[b.time.clone(), format!("{} {}", i.priority, i.priority_name), i.campaign.clone(), i.track.display(), ((spot_len(&i.track) as f64) / 1000.0).round().to_string()]);
        }
        out += &csv(&[format!("{} celkem", b.time), String::new(), String::new(), String::new(), ((b.duration_ms as f64) / 1000.0).round().to_string()]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ScannedFile;

    fn spot(db: &Db, name: &str, dur: i64) -> i64 {
        let f = ScannedFile { kind: TrackKind::Spot, path: format!("/tmp/{name}.mp3"), filename: format!("{name}.mp3"), title: name.into(), duration_ms: dur, file_mtime: 1, file_size: 1, ..Default::default() };
        match db.upsert_scanned(&f).unwrap() { crate::db::UpsertResult::Inserted(id) => id, _ => panic!() }
    }

    #[test]
    fn block_order_priorities_optional_and_rotation() {
        let db = Db::open_in_memory().unwrap();
        let cfg = Config::default();
        let (id_a, id_b, id_intro, id_sid) = (spot(&db, "a", 30_000), spot(&db, "b", 20_000), spot(&db, "intro", 5_000), spot(&db, "sid", 4_000));
        let date = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(); // pondělí
        let wk = |t: &str| Slot { weekday: Some(1), date: None, time: t.into(), spot_track_id: None };
        // reklama se 2 spoty v pořadí, priorita 4
        save_campaign(&db, &Campaign { name: "Autoservis".into(), spots: vec![CampaignSpot { track_id: id_a, ..Default::default() }, CampaignSpot { track_id: id_b, ..Default::default() }], slots: vec![wk("10:00"), wk("10:30")], ..Default::default() }).unwrap();
        // úvod reklam – optional priorita 2, není reklama
        save_campaign(&db, &Campaign { name: "Začátek reklam".into(), is_commercial: false, priority: 2, spots: vec![CampaignSpot { track_id: id_intro, ..Default::default() }], slots: vec![wk("10:00"), wk("11:00")], ..Default::default() }).unwrap();
        // station ID – priorita 1, trvalá
        save_campaign(&db, &Campaign { name: "Station ID".into(), is_commercial: false, priority: 1, spots: vec![CampaignSpot { track_id: id_sid, ..Default::default() }], slots: vec![wk("10:00"), wk("11:00")], ..Default::default() }).unwrap();
        let def = |t: &str| BreakDef { time: t.into(), ..Default::default() };

        let b = build_block(&db, &cfg, date, &def("10:00"), true).unwrap();
        let names: Vec<_> = b.items.iter().map(|i| i.track.title.clone()).collect();
        assert_eq!(names, vec!["sid", "intro", "a"], "pořadí podle priorit");
        assert_eq!(b.duration_ms, 39_000);
        // další blok s kampaní → druhý spot (in order)
        let b2 = build_block(&db, &cfg, date, &def("10:30"), true).unwrap();
        assert_eq!(b2.items[0].track.title, "b");
        // v 11:00 není reklama → "Začátek reklam" (optional) nehraje, station ID ano
        let b3 = build_block(&db, &cfg, date, &def("11:00"), false).unwrap();
        let names: Vec<_> = b3.items.iter().map(|i| i.track.title.clone()).collect();
        assert_eq!(names, vec!["sid"]);
        // jiný den v týdnu → nic
        let tue = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        assert!(build_block(&db, &cfg, tue, &def("10:00"), false).unwrap().items.is_empty());
        // expirace
        let mut c = list_campaigns(&db, None, "").unwrap().into_iter().find(|c| c.name == "Autoservis").unwrap();
        c.expires_at = Some("2026-11-23 10:15".into());
        save_campaign(&db, &c).unwrap();
        assert!(build_block(&db, &cfg, date, &def("10:30"), false).unwrap().items.is_empty(), "po expiraci nehraje");
    }

    #[test]
    fn playlister_import_and_verify() {
        let db = Db::open_in_memory().unwrap();
        let cfg = Config::default();
        let id = spot(&db, "mcd", 30_000);
        db.patch_track(id, &crate::db::TrackPatch { playlister_code: Some("MCD01".into()), ..Default::default() }).unwrap();
        let date = date_from_filename("20261123.txt").unwrap();
        let bad = import_playlister(&db, &cfg, date, "09:00,MCD01,Apple pie\n00:05,MCD01,x\n", true).unwrap();
        assert!(bad.aborted && bad.errors.len() == 1, "{bad:?}");
        let ok = import_playlister(&db, &cfg, date, "09:00,MCD01,Apple pie\n17:00,C00001,auto kod\n", true).unwrap();
        assert_eq!(ok.imported, 2, "{ok:?}");
        let b = build_block(&db, &cfg, date, &BreakDef { time: "09:00".into(), ..Default::default() }, false).unwrap();
        assert_eq!(b.items.len(), 1);
        assert_eq!(b.items[0].priority, 5);
        assert!(export_breaks_playlist(&db, &cfg, date).unwrap().contains("09:00,MCD01"));
    }
}
