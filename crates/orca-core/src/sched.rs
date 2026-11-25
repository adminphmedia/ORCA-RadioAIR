//! Plánování vysílání (Jazler: Playlists, Timetable, Events, Main Auto – SOHO plán):
//! moderátoři, pořady a jejich epizody, playlisty s rozvrhem, programová mřížka (EPG),
//! časované události a vygenerovaný plán vysílání po hodinách.

use crate::config::Config;
use crate::db::{now_utc, row_track, Db, TRACK_COLS};
use crate::model::{QueueItem, Track, TrackKind};
use crate::rotation::{clock_for, select, Exclusions};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone, Timelike};
use rand::seq::SliceRandom;
use rusqlite::{params, OptionalExtension};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashSet;

// ---------------------------------------------------------------------------------------------
// Datové typy

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Presenter {
    pub id: Option<i64>,
    pub name: String,
    /// Role (moderátor, redaktor, DJ …)
    pub role: String,
    pub bio: String,
    /// Fotka (cesta k souboru) – jde do DAB slideshow a EPG.
    pub photo_path: String,
    pub email: String,
    pub phone: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Show {
    pub id: Option<i64>,
    pub name: String,
    /// Krátký název (max 8 znaků – RDS PS / DAB krátký label).
    pub short_name: String,
    pub description: String,
    /// Žánr pro EPG (text, např. „Hudba“, „Zpravodajství“).
    pub genre: String,
    /// RDS PTY (0–31) a DAB PTy v době pořadu, None = výchozí stanice.
    pub pty: Option<u8>,
    pub presenter_ids: Vec<i64>,
    /// Obrázek pořadu (slideshow, EPG).
    pub image_path: String,
    pub color: String,
    pub url: String,
    /// Odkud se berou vstupy pro epizody (typ knihovny a kategorie).
    pub inputs_kind: String,
    pub inputs_category: String,
    /// Jak vybírat epizodu, když není určeno jinak: next | newest | random
    pub episode_order: String,
}

impl Default for Show {
    fn default() -> Self {
        Self {
            id: None, name: String::new(), short_name: String::new(), description: String::new(), genre: String::new(), pty: None,
            presenter_ids: vec![], image_path: String::new(), color: "#8b5cf6".into(), url: String::new(),
            inputs_kind: "voicetrack".into(), inputs_category: String::new(), episode_order: "next".into(),
        }
    }
}

/// Položka playlistu / části epizody / položka plánu.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlItem {
    pub id: String,
    /// track | select | episode | stitch | relay | bed | note
    /// bed: mode "on" (track_id nebo náhodně z kategorie Podkresů) | "off"
    #[serde(rename = "type")]
    pub typ: String,
    pub track_id: Option<i64>,
    /// select: typ knihovny a kategorie
    pub kind: Option<TrackKind>,
    pub category: String,
    /// select: rotation | random | newest | oldest | least_played
    /// episode: next | newest | random | specific
    pub mode: String,
    /// newest/oldest: kolikátý (1 = poslední nahraný, 2 = předposlední …)
    pub nth: u32,
    pub show_id: Option<i64>,
    pub episode_id: Option<i64>,
    /// stitch: hooks | time | news
    pub stitch: Option<String>,
    /// relay: id zdroje rebroadcastu
    pub relay_id: Option<String>,
    /// relay: délka převzetí v sekundách (0 = do konce pořadu/ručního ukončení)
    pub duration_s: u32,
    /// Sweeper / voice track přes skladbu
    pub position: Option<String>,
    /// Popisek (pro poznámky a přehled)
    pub label: String,
}

impl Default for PlItem {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(), typ: "track".into(), track_id: None, kind: None, category: String::new(), mode: String::new(), nth: 1,
            show_id: None, episode_id: None, stitch: None, relay_id: None, duration_s: 0, position: None, label: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Schedule {
    /// once | weekly
    #[serde(rename = "type")]
    pub typ: String,
    /// once: YYYY-MM-DD
    pub date: String,
    /// weekly: Po … Ne
    pub days: [bool; 7],
    /// HH:MM nebo HH:MM:SS
    pub time: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
}

impl Default for Schedule {
    fn default() -> Self { Self { typ: "weekly".into(), date: String::new(), days: [true; 7], time: "12:00".into(), valid_from: None, valid_to: None } }
}

impl Schedule {
    /// Čas spuštění v daný den (pokud v ten den platí).
    pub fn time_on(&self, date: NaiveDate) -> Option<NaiveTime> {
        let t = parse_time(&self.time)?;
        let ds = date.format("%Y-%m-%d").to_string();
        if let Some(f) = self.valid_from.as_deref().filter(|s| !s.is_empty()) { if ds.as_str() < f { return None; } }
        if let Some(to) = self.valid_to.as_deref().filter(|s| !s.is_empty()) { if ds.as_str() > to { return None; } }
        match self.typ.as_str() {
            "once" => (self.date == ds).then_some(t),
            _ => self.days[date.weekday().num_days_from_monday() as usize].then_some(t),
        }
    }
}

pub fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S").or_else(|_| NaiveTime::parse_from_str(s, "%H:%M")).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Playlist {
    pub id: Option<i64>,
    pub name: String,
    pub color: String,
    pub description: String,
    pub items: Vec<PlItem>,
    pub schedules: Vec<Schedule>,
    /// insert = vloží se mezi ostatní; override = nahradí automatiku (zruší další auto položky ve frontě)
    pub mode: String,
    /// V daný čas stáhne hrající skladbu a začne přesně.
    pub start_immediately: bool,
    /// Zamíchat pořadí položek.
    pub shuffle: bool,
    pub enabled: bool,
    /// Patří k pořadu (pro přehled a EPG).
    pub show_id: Option<i64>,
}

impl Default for Playlist {
    fn default() -> Self {
        Self { id: None, name: String::new(), color: "#0ea5e9".into(), description: String::new(), items: vec![], schedules: vec![], mode: "insert".into(), start_immediately: false, shuffle: false, enabled: true, show_id: None }
    }
}

/// Položka programové mřížky (týdenní program → EPG, DLS „právě vysíláme“, foto moderátora).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GridEntry {
    pub id: Option<i64>,
    pub show_id: Option<i64>,
    pub days: [bool; 7],
    pub start: String,
    pub end: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    /// music (jen označení, hraje automatika) | episode | playlist | relay | live
    pub content: String,
    pub playlist_id: Option<i64>,
    pub relay_id: Option<String>,
    /// episode: next | newest | random
    pub episode_mode: String,
    pub nth: u32,
    pub start_immediately: bool,
    /// Moderátoři jen pro tento čas (prázdné = podle pořadu)
    pub presenter_ids: Vec<i64>,
    /// Popis tohoto vysílání v EPG (prázdné = popis pořadu)
    pub description: String,
    /// Zvukový preset processingu pro tento blok (prázdné = hlavní nastavení)
    pub proc_preset: Option<String>,
}

impl Default for GridEntry {
    fn default() -> Self {
        Self { id: None, show_id: None, days: [true, true, true, true, true, false, false], start: "06:00".into(), end: "09:00".into(), valid_from: None, valid_to: None,
            content: "music".into(), playlist_id: None, relay_id: None, episode_mode: "next".into(), nth: 1, start_immediately: false, presenter_ids: vec![], description: String::new(), proc_preset: None }
    }
}

/// Časovaná událost (Jazler Events).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedEvent {
    pub id: Option<i64>,
    pub name: String,
    pub enabled: bool,
    pub schedule: Schedule,
    /// playlist | episode | relay_start | relay_stop | track | break | stitch | mode_auto | mode_manual | stop_after | clear_queue
    pub action: String,
    pub playlist_id: Option<i64>,
    pub show_id: Option<i64>,
    pub relay_id: Option<String>,
    pub duration_s: u32,
    pub track_id: Option<i64>,
    pub stitch: Option<String>,
    pub start_immediately: bool,
}

impl Default for SchedEvent {
    fn default() -> Self {
        Self { id: None, name: String::new(), enabled: true, schedule: Schedule::default(), action: "playlist".into(), playlist_id: None, show_id: None, relay_id: None, duration_s: 0, track_id: None, stitch: None, start_immediately: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Episode {
    pub id: Option<i64>,
    pub show_id: i64,
    pub num: i64,
    pub title: String,
    pub description: String,
    pub parts: Vec<PlItem>,
    /// draft | ready | archived
    pub status: String,
    pub air_date: Option<String>,
    pub created_at: String,
    pub play_count: i64,
    pub last_aired: Option<String>,
    /// Odhad délky (jen pro čtení)
    pub duration_ms: i64,
}

impl Default for Episode {
    fn default() -> Self {
        Self { id: None, show_id: 0, num: 0, title: String::new(), description: String::new(), parts: vec![], status: "ready".into(), air_date: None, created_at: String::new(), play_count: 0, last_aired: None, duration_ms: 0 }
    }
}

// ---------------------------------------------------------------------------------------------
// Dokumenty v DB (moderátoři, pořady, playlisty, mřížka, události)

pub trait Doc: Serialize + DeserializeOwned + Clone {
    const KIND: &'static str;
    fn id(&self) -> Option<i64>;
    fn set_id(&mut self, id: i64);
    fn name(&self) -> String;
}

macro_rules! doc {
    ($t:ty, $k:expr, $name:expr) => {
        impl Doc for $t {
            const KIND: &'static str = $k;
            fn id(&self) -> Option<i64> { self.id }
            fn set_id(&mut self, id: i64) { self.id = Some(id); }
            fn name(&self) -> String { let f: fn(&$t) -> String = $name; f(self) }
        }
    };
}
doc!(Presenter, "presenter", |x| x.name.clone());
doc!(Show, "show", |x| x.name.clone());
doc!(Playlist, "playlist", |x| x.name.clone());
doc!(GridEntry, "grid", |x| format!("{}-{}", x.start, x.end));
doc!(SchedEvent, "event", |x| x.name.clone());

pub fn list<T: Doc>(db: &Db) -> Result<Vec<T>> {
    db.with_conn(|c| {
        let mut st = c.prepare("SELECT id, data FROM sched_docs WHERE kind=?1 ORDER BY name COLLATE NOCASE, id")?;
        let rows = st.query_map([T::KIND], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = vec![];
        for r in rows {
            let (id, data) = r?;
            match serde_json::from_str::<T>(&data) { Ok(mut d) => { d.set_id(id); out.push(d); } Err(e) => log::warn!("{} {id}: {e}", T::KIND) }
        }
        Ok(out)
    })
}

pub fn get<T: Doc>(db: &Db, id: i64) -> Result<Option<T>> {
    db.with_conn(|c| {
        let data: Option<String> = c.query_row("SELECT data FROM sched_docs WHERE kind=?1 AND id=?2", params![T::KIND, id], |r| r.get(0)).optional()?;
        Ok(match data { Some(d) => { let mut x: T = serde_json::from_str(&d)?; x.set_id(id); Some(x) } None => None })
    })
}

pub fn save<T: Doc>(db: &Db, doc: &T) -> Result<T> {
    let mut d = doc.clone();
    let json = serde_json::to_string(&d)?;
    db.with_conn(|c| -> Result<()> {
        match d.id() {
            Some(id) => {
                let n = c.execute("UPDATE sched_docs SET name=?1, data=?2, updated_at=?3 WHERE id=?4 AND kind=?5", params![d.name(), json, now_utc(), id, T::KIND])?;
                if n == 0 { return Err(anyhow!("záznam nenalezen")); }
            }
            None => {
                c.execute("INSERT INTO sched_docs(kind, name, data, updated_at) VALUES(?1,?2,?3,?4)", params![T::KIND, d.name(), json, now_utc()])?;
                d.set_id(c.last_insert_rowid());
            }
        }
        Ok(())
    })?;
    Ok(d)
}

pub fn delete<T: Doc>(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM sched_docs WHERE kind=?1 AND id=?2", params![T::KIND, id])?; Ok(()) })
}

pub fn delete_show(db: &Db, id: i64) -> Result<()> {
    delete::<Show>(db, id)?;
    db.with_conn(|c| { c.execute("DELETE FROM episodes WHERE show_id=?1", [id])?; Ok(()) })
}

// ---------------------------------------------------------------------------------------------
// Epizody

fn row_episode(r: &rusqlite::Row) -> rusqlite::Result<Episode> {
    let parts: String = r.get(5)?;
    Ok(Episode {
        id: Some(r.get(0)?), show_id: r.get(1)?, num: r.get(2)?, title: r.get(3)?, description: r.get(4)?,
        parts: serde_json::from_str(&parts).unwrap_or_default(), status: r.get(6)?, air_date: r.get(7)?, created_at: r.get(8)?,
        play_count: r.get(9)?, last_aired: r.get(10)?, duration_ms: 0,
    })
}
const EP_COLS: &str = "id, show_id, num, title, description, parts, status, air_date, created_at, play_count, last_aired";

pub fn episodes(db: &Db, show_id: i64) -> Result<Vec<Episode>> {
    let mut v: Vec<Episode> = db.with_conn(|c| -> Result<Vec<Episode>> {
        let mut st = c.prepare(&format!("SELECT {EP_COLS} FROM episodes WHERE show_id=?1 ORDER BY num DESC, id DESC"))?;
        Ok(st.query_map([show_id], row_episode)?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    for e in v.iter_mut() { e.duration_ms = estimate_items(db, &e.parts); }
    Ok(v)
}

pub fn get_episode(db: &Db, id: i64) -> Result<Option<Episode>> {
    db.with_conn(|c| Ok(c.query_row(&format!("SELECT {EP_COLS} FROM episodes WHERE id=?1"), [id], row_episode).optional()?))
}

pub fn save_episode(db: &Db, e: &Episode) -> Result<Episode> {
    let mut e = e.clone();
    let parts = serde_json::to_string(&e.parts)?;
    db.with_conn(|c| -> Result<()> {
        if e.num <= 0 {
            e.num = c.query_row("SELECT COALESCE(MAX(num),0)+1 FROM episodes WHERE show_id=?1", [e.show_id], |r| r.get(0))?;
        }
        match e.id {
            Some(id) => { c.execute("UPDATE episodes SET num=?1, title=?2, description=?3, parts=?4, status=?5, air_date=?6 WHERE id=?7", params![e.num, e.title, e.description, parts, e.status, e.air_date, id])?; }
            None => {
                e.created_at = now_utc();
                c.execute("INSERT INTO episodes(show_id, num, title, description, parts, status, air_date, created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![e.show_id, e.num, e.title, e.description, parts, e.status, e.air_date, e.created_at])?;
                e.id = Some(c.last_insert_rowid());
            }
        }
        Ok(())
    })?;
    e.duration_ms = estimate_items(db, &e.parts);
    Ok(e)
}

pub fn delete_episode(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM episodes WHERE id=?1", [id])?; Ok(()) })
}

pub fn mark_episode_aired(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| { c.execute("UPDATE episodes SET play_count=play_count+1, last_aired=?1 WHERE id=?2", params![now_utc(), id])?; Ok(()) })
}

/// Výběr epizody pořadu: next = nejstarší dosud neodvysílaná (pak nejméně hraná), newest = n-tá nejnovější,
/// random = náhodná připravená, specific = konkrétní id.
pub fn pick_episode(db: &Db, show_id: i64, mode: &str, nth: u32, specific: Option<i64>) -> Result<Option<Episode>> {
    if let Some(id) = specific { return get_episode(db, id); }
    let order = match mode {
        "newest" => "num DESC, id DESC",
        "random" => "RANDOM()",
        _ => "play_count ASC, num ASC, id ASC",
    };
    let off = if mode == "newest" { nth.max(1) as i64 - 1 } else { 0 };
    db.with_conn(|c| Ok(c.query_row(&format!("SELECT {EP_COLS} FROM episodes WHERE show_id=?1 AND status='ready' ORDER BY {order} LIMIT 1 OFFSET ?2"), params![show_id, off], row_episode).optional()?))
}

// ---------------------------------------------------------------------------------------------
// Výběr souborů

/// Vybere soubor podle selektoru: rotation (nejdéle nehraný), random, newest/oldest (n-tý podle data přidání),
/// least_played (nejméně hraný).
pub fn select_track(db: &Db, cfg: &Config, it: &PlItem, excl: &HashSet<i64>) -> Result<Option<(Track, String)>> {
    let kind = it.kind.unwrap_or(TrackKind::Song);
    let cat = it.category.clone();
    let cat_l = if cat.is_empty() { "*".to_string() } else { cat.clone() };
    let nth = it.nth.max(1) as i64;
    let q = |order: &str, offset: i64| -> Result<Option<Track>> {
        db.with_conn(|c| {
            let sql = format!("SELECT {TRACK_COLS} FROM tracks WHERE kind=?1 AND (?2='' OR category=?2) AND enabled=1 AND missing=0 ORDER BY {order} LIMIT 1 OFFSET ?3");
            Ok(c.query_row(&sql, params![kind.as_str(), cat, offset], row_track).optional()?)
        })
    };
    let r = match it.mode.as_str() {
        "newest" => q("added_at DESC, file_mtime DESC, id DESC", nth - 1)?.map(|t| (t, format!("{} / {cat_l}: {}. nejnovější", kind.as_str(), nth))),
        "oldest" => q("added_at ASC, id ASC", nth - 1)?.map(|t| (t, format!("{} / {cat_l}: {}. nejstarší", kind.as_str(), nth))),
        "least_played" => q("play_count ASC, last_played_at ASC, id ASC", 0)?.map(|t| (t, format!("{} / {cat_l}: nejméně hraný", kind.as_str()))),
        "random" => {
            let all: Vec<Track> = db.with_conn(|c| -> Result<Vec<Track>> {
                let mut st = c.prepare(&format!("SELECT {TRACK_COLS} FROM tracks WHERE kind=?1 AND (?2='' OR category=?2) AND enabled=1 AND missing=0 AND duration_ms>0"))?;
                Ok(st.query_map(params![kind.as_str(), cat], row_track)?.collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            let ok: Vec<&Track> = all.iter().filter(|t| !excl.contains(&t.id)).collect();
            let pool = if ok.is_empty() { all.iter().collect() } else { ok };
            pool.choose(&mut rand::thread_rng()).map(|t| ((*t).clone(), format!("{} / {cat_l}: náhodně", kind.as_str())))
        }
        _ => {
            let step = crate::config::ClockStep { kind, category: cat.clone(), position: None, stitch: None, track_id: None };
            let ex = Exclusions { track_ids: excl.clone(), artists: HashSet::new(), at: None };
            select(db, &cfg.auto, &step, "", &ex)?
        }
    };
    Ok(r)
}

/// Odhad délky položek (ms).
pub fn estimate_items(db: &Db, items: &[PlItem]) -> i64 {
    let mut total = 0;
    for it in items {
        match it.typ.as_str() {
            "track" => if let Some(Ok(Some(t))) = it.track_id.map(|id| db.get_track(id)) {
                if !(t.kind().is_overlay_capable() && it.position.as_deref().map(|p| p != "independent").unwrap_or(false)) {
                    let (ci, mix, _) = t.effective_cues(); total += (mix - ci).max(0);
                }
            },
            "relay" => total += it.duration_s as i64 * 1000,
            "select" => total += if it.kind.unwrap_or(TrackKind::Song) == TrackKind::Song { 210_000 } else { 15_000 },
            "stitch" => total += 20_000,
            "live" => total += it.duration_s as i64 * 1000,
            "bed" => total += if it.mode == "off" { 3000 } else { it.track_id.and_then(|id| db.get_track(id).ok().flatten()).map(|t| t.bed_intro_ms).unwrap_or(3000) },
            "episode" => {
                if let Ok(Some(e)) = it.show_id.map(|s| pick_episode(db, s, &it.mode, it.nth, it.episode_id)).transpose().map(|x| x.flatten()) {
                    total += estimate_items(db, &e.parts);
                }
            }
            _ => {}
        }
    }
    total
}

/// Rozvinutí položek na položky fronty. `episodes_aired` – epizody, které se tím odvysílají.
pub fn expand(db: &Db, cfg: &Config, items: &[PlItem], source: &str, reason: &str, depth: u8) -> Result<(Vec<QueueItem>, Vec<i64>, Vec<String>)> {
    let mut out = vec![];
    let mut aired = vec![];
    let mut warnings = vec![];
    let mut excl: HashSet<i64> = HashSet::new();
    for it in items {
        let mk = |t: Track, why: String| {
            let mut q = QueueItem::new(t, source, format!("{reason} • {why}"));
            q.position = it.position.clone();
            q
        };
        match it.typ.as_str() {
            "track" => match it.track_id.map(|id| db.get_track(id)).transpose()?.flatten() {
                Some(t) if t.enabled && !t.missing => { excl.insert(t.id); out.push(mk(t, "pevně zvolený soubor".into())); }
                Some(t) => warnings.push(format!("soubor {} je vypnutý nebo chybí", t.display())),
                None => warnings.push(format!("soubor #{} neexistuje", it.track_id.unwrap_or(0))),
            },
            "select" => match select_track(db, cfg, it, &excl)? {
                Some((t, why)) => { excl.insert(t.id); out.push(mk(t, why)); }
                None => warnings.push(format!("pro výběr {} / {} nejsou soubory", it.kind.map(|k| k.as_str()).unwrap_or("song"), it.category)),
            },
            "stitch" => if let Some(k) = &it.stitch {
                out.push(crate::stitcher::queue_item(k, 20_000, &format!("{reason} • Stitcher")));
            },
            "live" => out.push(QueueItem::live_break(&it.label, it.duration_s as i64 * 1000, it.mode == "auto", source, format!("{reason} • živý vstup"))),
            "bed" => {
                if it.mode == "off" { out.push(QueueItem::bed_off(source, format!("{reason} • podkres OFF"))); continue; }
                let picked = match it.track_id {
                    Some(id) => db.get_track(id)?.filter(|t| t.enabled && !t.missing).map(|t| (t, "pevně zvolený podkres".to_string())),
                    None => { let mut sel = it.clone(); sel.kind = Some(TrackKind::Bed); sel.mode = "rotation".into(); select_track(db, cfg, &sel, &excl)? }
                };
                match picked {
                    Some((t, why)) => out.push(QueueItem::bed_on(t, source, format!("{reason} • podkres ON • {why}"))),
                    None => warnings.push("pro podkres není k dispozici soubor (knihovna Podkresy)".into()),
                }
            }
            "relay" => if let Some(rid) = &it.relay_id {
                match crate::relay::queue_item(cfg, rid, it.duration_s as i64 * 1000, &format!("{reason} • rebroadcast")) {
                    Some(q) => out.push(q),
                    None => warnings.push(format!("zdroj rebroadcastu '{rid}' neexistuje")),
                }
            },
            "episode" => {
                if depth > 2 { continue; }
                let Some(show_id) = it.show_id else { continue };
                match pick_episode(db, show_id, if it.mode.is_empty() { "next" } else { &it.mode }, it.nth, it.episode_id)? {
                    Some(e) => {
                        let show = get::<Show>(db, show_id)?.map(|s| s.name).unwrap_or_default();
                        let r = format!("{reason} • {show} – díl {} {}", e.num, e.title);
                        let (mut q, _, w) = expand(db, cfg, &e.parts, source, &r, depth + 1)?;
                        out.append(&mut q); warnings.extend(w);
                        if let Some(id) = e.id { aired.push(id); }
                    }
                    None => warnings.push("pořad nemá připravenou epizodu".into()),
                }
            }
            _ => {}
        }
    }
    Ok((out, aired, warnings))
}

// ---------------------------------------------------------------------------------------------
// Rozvrh: výskyty v daném dni

#[derive(Debug, Clone, Serialize)]
pub struct Occurrence {
    /// playlist | grid | event
    pub source: String,
    pub id: i64,
    pub name: String,
    pub color: String,
    pub date: String,
    pub time: String,
    /// konec (mřížka) – HH:MM
    pub end: Option<String>,
    pub action: String,
    pub start_immediately: bool,
    pub duration_ms: i64,
    pub fired: bool,
    pub key: String,
}

pub fn fired_keys(db: &Db, prefix: &str) -> Result<HashSet<String>> {
    db.with_conn(|c| {
        let mut st = c.prepare("SELECT key FROM sched_fired WHERE key LIKE ?1")?;
        let v = st.query_map([format!("%{prefix}%")], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(v)
    })
}

pub fn mark_fired(db: &Db, key: &str) -> Result<()> {
    db.with_conn(|c| { c.execute("INSERT OR REPLACE INTO sched_fired(key, fired_at) VALUES(?1, ?2)", params![key, now_utc()])?; Ok(()) })
}

pub fn unmark_fired(db: &Db, key: &str) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM sched_fired WHERE key=?1", [key])?; Ok(()) })
}

/// Všechny naplánované výskyty v daný den (Timetable).
pub fn day_occurrences(db: &Db, date: NaiveDate) -> Result<Vec<Occurrence>> {
    let ds = date.format("%Y-%m-%d").to_string();
    let fired = fired_keys(db, &ds)?;
    let mut out = vec![];
    for p in list::<Playlist>(db)?.into_iter().filter(|p| p.enabled) {
        let dur = estimate_items(db, &p.items);
        for s in &p.schedules {
            if let Some(t) = s.time_on(date) {
                let time = t.format("%H:%M:%S").to_string();
                let key = format!("playlist:{}:{ds} {time}", p.id.unwrap_or(0));
                out.push(Occurrence { source: "playlist".into(), id: p.id.unwrap_or(0), name: p.name.clone(), color: p.color.clone(), date: ds.clone(), time, end: None,
                    action: p.mode.clone(), start_immediately: p.start_immediately, duration_ms: dur, fired: fired.contains(&key), key });
            }
        }
    }
    let shows = list::<Show>(db)?;
    for g in list::<GridEntry>(db)? {
        let Some(t) = grid_on(&g, date) else { continue };
        let show = shows.iter().find(|s| s.id == g.show_id);
        let time = t.format("%H:%M:%S").to_string();
        let key = format!("grid:{}:{ds} {time}", g.id.unwrap_or(0));
        out.push(Occurrence { source: "grid".into(), id: g.id.unwrap_or(0), name: show.map(|s| s.name.clone()).unwrap_or_default(), color: show.map(|s| s.color.clone()).unwrap_or_default(),
            date: ds.clone(), time, end: Some(g.end.clone()), action: g.content.clone(), start_immediately: g.start_immediately, duration_ms: grid_len_ms(&g), fired: fired.contains(&key), key });
    }
    for e in list::<SchedEvent>(db)?.into_iter().filter(|e| e.enabled) {
        if let Some(t) = e.schedule.time_on(date) {
            let time = t.format("%H:%M:%S").to_string();
            let key = format!("event:{}:{ds} {time}", e.id.unwrap_or(0));
            out.push(Occurrence { source: "event".into(), id: e.id.unwrap_or(0), name: e.name.clone(), color: String::new(), date: ds.clone(), time, end: None,
                action: e.action.clone(), start_immediately: e.start_immediately, duration_ms: e.duration_s as i64 * 1000, fired: fired.contains(&key), key });
        }
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

fn grid_on(g: &GridEntry, date: NaiveDate) -> Option<NaiveTime> {
    let ds = date.format("%Y-%m-%d").to_string();
    if let Some(f) = g.valid_from.as_deref().filter(|s| !s.is_empty()) { if ds.as_str() < f { return None; } }
    if let Some(t) = g.valid_to.as_deref().filter(|s| !s.is_empty()) { if ds.as_str() > t { return None; } }
    if !g.days[date.weekday().num_days_from_monday() as usize] { return None; }
    parse_time(&g.start)
}

fn grid_len_ms(g: &GridEntry) -> i64 {
    match (parse_time(&g.start), parse_time(&g.end)) {
        (Some(a), Some(b)) => { let mut d = (b - a).num_milliseconds(); if d <= 0 { d += 86_400_000; } d }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Program (EPG): co se vysílá v daný den

#[derive(Debug, Clone, Serialize)]
pub struct ProgramSlot {
    pub grid_id: i64,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub show: Show,
    pub presenters: Vec<Presenter>,
    pub description: String,
    pub content: String,
}

pub fn program_for_day(db: &Db, date: NaiveDate) -> Result<Vec<ProgramSlot>> {
    let shows = list::<Show>(db)?;
    let presenters = list::<Presenter>(db)?;
    let mut out = vec![];
    for g in list::<GridEntry>(db)? {
        let Some(t) = grid_on(&g, date) else { continue };
        let Some(show) = shows.iter().find(|s| s.id == g.show_id).cloned() else { continue };
        let Some(start) = Local.from_local_datetime(&date.and_time(t)).single() else { continue };
        let end = start + chrono::Duration::milliseconds(grid_len_ms(&g));
        let ids = if g.presenter_ids.is_empty() { &show.presenter_ids } else { &g.presenter_ids };
        let pres = ids.iter().filter_map(|id| presenters.iter().find(|p| p.id == Some(*id)).cloned()).collect();
        let description = if g.description.is_empty() { show.description.clone() } else { g.description.clone() };
        out.push(ProgramSlot { grid_id: g.id.unwrap_or(0), start, end, show, presenters: pres, description, content: g.content.clone() });
    }
    out.sort_by_key(|s| s.start);
    Ok(out)
}

/// Právě vysílaný a následující pořad.
pub fn now_next(db: &Db, now: DateTime<Local>) -> Result<(Option<ProgramSlot>, Option<ProgramSlot>)> {
    let d = now.date_naive();
    let mut slots = program_for_day(db, d.pred_opt().unwrap_or(d))?;
    slots.extend(program_for_day(db, d)?);
    slots.extend(program_for_day(db, d.succ_opt().unwrap_or(d))?);
    let cur = slots.iter().filter(|s| s.start <= now && now < s.end).max_by_key(|s| s.start).cloned();
    let next = slots.into_iter().filter(|s| s.start > now).min_by_key(|s| s.start);
    Ok((cur, next))
}

// ---------------------------------------------------------------------------------------------
// Plán vysílání (Jazler SOHO Main Auto: playlist vygenerovaný z clocků na hodiny dopředu)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRow {
    pub id: i64,
    pub date: String,
    pub hour: u32,
    pub pos: i64,
    pub item: PlItem,
    pub locked: bool,
    /// planned | queued | skipped
    pub state: String,
    /// rozvinuté info pro zobrazení
    pub track: Option<Track>,
    pub eta: Option<String>,
}

pub fn plan_rows(db: &Db, date: &str, hour: Option<u32>) -> Result<Vec<PlanRow>> {
    let rows: Vec<(i64, String, u32, i64, String, bool, String)> = db.with_conn(|c| -> Result<_> {
        let mut st = c.prepare("SELECT id, date, hour, pos, item, locked, state FROM plan_items WHERE date=?1 AND (?2 IS NULL OR hour=?2) ORDER BY hour, pos, id")?;
        Ok(st.query_map(params![date, hour], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? != 0, r.get(6)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut out = vec![];
    let mut t_by_hour: std::collections::HashMap<u32, DateTime<Local>> = Default::default();
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok();
    for (id, date, hour, pos, item, locked, state) in rows {
        let item: PlItem = serde_json::from_str(&item).unwrap_or_default();
        let track = item.track_id.and_then(|id| db.get_track(id).ok().flatten());
        let t = t_by_hour.entry(hour).or_insert_with(|| d.and_then(|d| Local.from_local_datetime(&d.and_hms_opt(hour, 0, 0).unwrap()).single()).unwrap_or_else(Local::now));
        let eta = Some(t.to_rfc3339());
        let len = match (&track, item.typ.as_str()) {
            (Some(tr), _) if !(tr.kind().is_overlay_capable() && item.position.as_deref().map(|p| p != "independent").unwrap_or(false)) => { let (ci, mix, _) = tr.effective_cues(); (mix - ci).max(0) }
            (_, "relay") => item.duration_s as i64 * 1000,
            (_, "stitch") => 20_000,
            _ => 0,
        };
        *t += chrono::Duration::milliseconds(len);
        out.push(PlanRow { id, date, hour, pos, item, locked, state, track, eta });
    }
    Ok(out)
}

/// Vygeneruje plán pro dané hodiny z clocků (zamčené položky zůstanou).
pub fn generate_plan(db: &Db, cfg: &Config, date: NaiveDate, hours: &[u32], overwrite: bool) -> Result<usize> {
    let ds = date.format("%Y-%m-%d").to_string();
    let mut used: HashSet<i64> = HashSet::new();
    let mut artist_times: Vec<(String, i64)> = vec![];
    let mut created = 0;
    for &h in hours {
        if h > 23 { continue; }
        let existing = plan_rows(db, &ds, Some(h))?;
        if !existing.is_empty() && !overwrite { for r in &existing { if let Some(id) = r.item.track_id { used.insert(id); } } continue; }
        db.with_conn(|c| c.execute("DELETE FROM plan_items WHERE date=?1 AND hour=?2 AND locked=0", params![ds, h]))?;
        let locked: Vec<PlanRow> = plan_rows(db, &ds, Some(h))?;
        let Some(start) = Local.from_local_datetime(&date.and_hms_opt(h, 0, 0).unwrap()).single() else { continue };
        let Some(clock) = clock_for(cfg, &start).cloned() else { continue };
        if clock.steps.is_empty() { continue; }
        let mut t_ms: i64 = locked.iter().map(|r| r.track.as_ref().map(|t| { let (a, b, _) = t.effective_cues(); (b - a).max(0) }).unwrap_or(0)).sum();
        let mut pos = locked.iter().map(|r| r.pos).max().unwrap_or(0) + 1;
        let mut step_i = 0usize;
        let mut guard = 0;
        while t_ms < 3_600_000 && guard < 400 {
            guard += 1;
            let step = &clock.steps[step_i % clock.steps.len()];
            step_i += 1;
            let item = if let Some(k) = &step.stitch {
                t_ms += 20_000;
                PlItem { typ: "stitch".into(), stitch: Some(k.clone()), label: format!("Stitcher {k}"), ..Default::default() }
            } else {
                let now_ms = h as i64 * 3_600_000 + t_ms;
                let artists: HashSet<String> = artist_times.iter().filter(|(_, at)| now_ms - at < cfg.auto.artist_separation_min as i64 * 60_000).map(|(a, _)| a.clone()).collect();
                let ex = Exclusions { track_ids: used.clone(), artists, at: None };
                let picked = match select(db, &cfg.auto, step, &clock.backup_category, &ex)? {
                    Some(x) => Some(x),
                    // málo souborů – povolit opakování (jako automatika při uvolnění separace)
                    None => select(db, &cfg.auto, step, &clock.backup_category, &Exclusions { track_ids: HashSet::new(), artists: HashSet::new(), at: None })?,
                };
                let Some((t, _why)) = picked else { continue };
                used.insert(t.id);
                let overlay = t.kind().is_overlay_capable() && step.position.as_deref().map(|p| p != "independent").unwrap_or(false);
                if !overlay { let (a, b, _) = t.effective_cues(); t_ms += (b - a).max(1000); }
                if t.kind() == TrackKind::Song && !t.artist.is_empty() { artist_times.push((t.artist.to_lowercase(), now_ms)); }
                PlItem { typ: "track".into(), track_id: Some(t.id), position: step.position.clone(), label: t.display(), ..Default::default() }
            };
            let json = serde_json::to_string(&item)?;
            db.with_conn(|c| c.execute("INSERT INTO plan_items(date, hour, pos, item) VALUES(?1,?2,?3,?4)", params![ds, h, pos, json]))?;
            pos += 1;
            created += 1;
        }
    }
    Ok(created)
}

pub fn plan_clear(db: &Db, date: &str, hour: Option<u32>, keep_locked: bool) -> Result<usize> {
    db.with_conn(|c| Ok(c.execute("DELETE FROM plan_items WHERE date=?1 AND (?2 IS NULL OR hour=?2) AND (?3=0 OR locked=0)", params![date, hour, keep_locked as i64])?))
}

/// Uložení upravené hodiny plánu (pořadí, položky, zámky).
pub fn plan_save_hour(db: &Db, date: &str, hour: u32, rows: &[(PlItem, bool)]) -> Result<()> {
    db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        tx.execute("DELETE FROM plan_items WHERE date=?1 AND hour=?2 AND state='planned'", params![date, hour])?;
        let base: i64 = tx.query_row("SELECT COALESCE(MAX(pos),0) FROM plan_items WHERE date=?1 AND hour=?2", params![date, hour], |r| r.get(0))?;
        for (i, (it, locked)) in rows.iter().enumerate() {
            tx.execute("INSERT INTO plan_items(date, hour, pos, item, locked) VALUES(?1,?2,?3,?4,?5)", params![date, hour, base + 1 + i as i64, serde_json::to_string(it)?, *locked as i64])?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// Další položka plánu pro danou hodinu (označí ji jako zařazenou). Starší nezařazené hodiny se přeskočí.
pub fn plan_take(db: &Db, at: &DateTime<Local>) -> Result<Option<PlItem>> {
    let ds = at.format("%Y-%m-%d").to_string();
    let h = at.hour();
    db.with_conn(|c| -> Result<Option<PlItem>> {
        c.execute("UPDATE plan_items SET state='skipped' WHERE state='planned' AND (date<?1 OR (date=?1 AND hour<?2))", params![ds, h])?;
        let r: Option<(i64, String)> = c.query_row("SELECT id, item FROM plan_items WHERE date=?1 AND hour=?2 AND state='planned' ORDER BY pos, id LIMIT 1", params![ds, h], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((id, item)) = r else { return Ok(None) };
        c.execute("UPDATE plan_items SET state='queued' WHERE id=?1", [id])?;
        Ok(serde_json::from_str(&item).ok())
    })
}

pub fn plan_hours_with_items(db: &Db, date: &str) -> Result<Vec<(u32, i64, i64)>> {
    db.with_conn(|c| {
        let mut st = c.prepare("SELECT hour, COUNT(*), SUM(state='planned') FROM plan_items WHERE date=?1 GROUP BY hour ORDER BY hour")?;
        let v = st.query_map([date], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

// ---------------------------------------------------------------------------------------------
// Import playlistů (Jazler: Filenames filter, Prodigy filter)

#[derive(Debug, Clone, Serialize, Default)]
pub struct ImportResult {
    pub items: Vec<PlItem>,
    pub errors: Vec<String>,
}

/// Řádky se jmény/cestami souborů (M3U, TXT). Prodigy značky: SW1–SW4 (sweeper), COMBREAK (reklamní blok – přeskočí, bloky se vkládají samy), END.
pub fn import_text(db: &Db, text: &str) -> Result<ImportResult> {
    let mut res = ImportResult::default();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') { continue; }
        let up = line.to_uppercase();
        if up == "END" { break; }
        if up == "COMBREAK" { res.items.push(PlItem { typ: "note".into(), label: "COMBREAK".into(), ..Default::default() }); continue; }
        if let Some(num) = up.strip_prefix("SW").and_then(|x| x.parse::<u32>().ok()) {
            res.items.push(PlItem { typ: "select".into(), kind: Some(TrackKind::Sweeper), mode: "rotation".into(), position: Some("start".into()), label: format!("Sweeper {num}"), ..Default::default() });
            continue;
        }
        // Prodigy: "HH:MM:SS<tab>soubor" nebo "kód,název"
        let cand = line.split('\t').last().unwrap_or(line).trim().trim_matches('"');
        let fname = cand.rsplit(['/', '\\']).next().unwrap_or(cand);
        let found: Option<Track> = db.with_conn(|c| -> Result<Option<Track>> {
            let t = c.query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE path=?1 AND missing=0"), [cand], row_track).optional()?;
            if t.is_some() { return Ok(t); }
            Ok(c.query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE filename=?1 COLLATE NOCASE AND missing=0 ORDER BY id DESC LIMIT 1"), [fname], row_track).optional()?)
        })?;
        match found {
            Some(t) => res.items.push(PlItem { typ: "track".into(), track_id: Some(t.id), label: t.display(), ..Default::default() }),
            None => res.errors.push(format!("řádek {}: soubor '{}' není v knihovně", n + 1, fname)),
        }
    }
    Ok(res)
}

/// Automatický import playlistů ze složky: soubor `YYYYMMDD-HHMM[ název].txt|m3u` → playlist naplánovaný na ten čas.
/// Zpracované soubory se přesunou do `Imported/`, chyby se zapíšou do `_Errors.txt`.
pub fn auto_import_dir(db: &Db, dir: &std::path::Path) -> Result<Vec<String>> {
    let mut msgs = vec![];
    if !dir.is_dir() { return Ok(msgs); }
    let imported = dir.join("Imported");
    for e in std::fs::read_dir(dir)?.flatten() {
        let path = e.path();
        if !path.is_file() { continue; }
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        if !["txt", "m3u", "m3u8"].contains(&ext.as_str()) { continue; }
        let stem = path.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
        if stem.starts_with('_') { continue; }
        let (date, time) = match (stem.get(0..8).and_then(|d| NaiveDate::parse_from_str(d, "%Y%m%d").ok()), stem.get(9..13).and_then(|t| NaiveTime::parse_from_str(t, "%H%M").ok())) {
            (Some(d), Some(t)) => (d, t),
            _ => continue,
        };
        let text = std::fs::read(&path).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
        let res = import_text(db, &text)?;
        let name = stem.get(13..).map(|x| x.trim_matches([' ', '-', '_'])).filter(|x| !x.is_empty()).map(String::from).unwrap_or_else(|| format!("Import {stem}"));
        if !res.errors.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("_Errors.txt")) {
                let _ = writeln!(f, "[{}] {}:", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), path.file_name().unwrap_or_default().to_string_lossy());
                for er in &res.errors { let _ = writeln!(f, "  {er}"); }
            }
        }
        if !res.items.is_empty() {
            let p = Playlist { name: name.clone(), items: res.items.clone(), schedules: vec![Schedule { typ: "once".into(), date: date.format("%Y-%m-%d").to_string(), time: time.format("%H:%M").to_string(), ..Default::default() }], ..Default::default() };
            save(db, &p)?;
        }
        std::fs::create_dir_all(&imported)?;
        let dest = crate::library::unique_path(&imported, &path.file_name().unwrap_or_default().to_string_lossy(), "rename").unwrap_or_else(|| imported.join(path.file_name().unwrap_or_default()));
        std::fs::rename(&path, &dest).or_else(|_| std::fs::copy(&path, &dest).and_then(|_| std::fs::remove_file(&path)))?;
        msgs.push(format!("import playlistu {}: {} položek, {} chyb", name, res.items.len(), res.errors.len()));
    }
    Ok(msgs)
}

pub fn shuffle(items: &mut [PlItem]) { items.shuffle(&mut rand::thread_rng()); }

/// Pomocná funkce pro automatiku: cílový čas výskytu.
pub fn occurrence_dt(date: NaiveDate, time: &str) -> Option<DateTime<Local>> {
    let t = parse_time(time)?;
    Local.from_local_datetime(&date.and_time(t)).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_days() {
        let s = Schedule { typ: "weekly".into(), days: [true, false, false, false, false, false, false], time: "10:30".into(), ..Default::default() };
        let mon = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        assert!(s.time_on(mon).is_some());
        assert!(s.time_on(mon.succ_opt().unwrap()).is_none());
        let o = Schedule { typ: "once".into(), date: "2026-11-24".into(), time: "08:00".into(), ..Default::default() };
        assert!(o.time_on(mon.succ_opt().unwrap()).is_some());
    }

    #[test]
    fn docs_and_episodes() {
        let db = Db::open_in_memory().unwrap();
        let s = save(&db, &Show { name: "Ranní show".into(), ..Default::default() }).unwrap();
        let sid = s.id.unwrap();
        for i in 0..3 { save_episode(&db, &Episode { show_id: sid, title: format!("Díl {i}"), ..Default::default() }).unwrap(); }
        let e = pick_episode(&db, sid, "next", 1, None).unwrap().unwrap();
        assert_eq!(e.num, 1);
        mark_episode_aired(&db, e.id.unwrap()).unwrap();
        assert_eq!(pick_episode(&db, sid, "next", 1, None).unwrap().unwrap().num, 2);
        assert_eq!(pick_episode(&db, sid, "newest", 1, None).unwrap().unwrap().num, 3);
        assert_eq!(pick_episode(&db, sid, "newest", 2, None).unwrap().unwrap().num, 2);
        assert_eq!(list::<Show>(&db).unwrap().len(), 1);
    }
}
