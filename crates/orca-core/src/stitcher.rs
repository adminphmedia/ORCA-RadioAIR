//! The Stitcher: skládá více souborů do jednoho vysílaného celku v okamžiku přehrání.
//! - hooks: ohlášení dalších skladeb jejich hooky (úvod, hook, oddělovač, hook…, závěr)
//! - time:  ohlášení přesného času z nahrávek hodin a minut (úvod + podkres, hodina, minuta, závěr)
//! - news:  zprávy – úvod + podkres, hlas zpráv, závěr; podkres hraje jen po dobu hlasu

use crate::config::Config;
use crate::db::{row_track, Db, TRACK_COLS};
use crate::model::{QueueItem, Track, TrackKind};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Timelike};
use rusqlite::params;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub path: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub label: String,
}

impl Segment {
    pub fn len_ms(&self) -> i64 { (self.end_ms - self.start_ms).max(0) }
    fn from_track(t: &Track, label: &str) -> Segment {
        let (ci, _, co) = t.effective_cues();
        Segment { path: t.path.clone(), start_ms: ci, end_ms: co, label: label.to_string() }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StitchPlan {
    pub kind: String,
    pub title: String,
    pub segments: Vec<Segment>,
    /// Podkres: segment, hlasitost (dB), délka (ms) – hraje od začátku po zadanou dobu a na konci se stáhne.
    pub bed: Option<(Segment, f32, i64)>,
    /// Použit náhradní soubor (stitch se nepodařilo složit).
    pub fallback: bool,
}

impl StitchPlan {
    pub fn total_ms(&self) -> i64 { self.segments.iter().map(|s| s.len_ms()).sum() }
}

pub fn title_for(kind: &str) -> &'static str {
    match kind { "hooks" => "Stitcher: Další skladby", "time" => "Stitcher: Ohlášení času", "news" => "Stitcher: Zprávy", _ => "Stitcher" }
}

fn track(db: &Db, id: Option<i64>) -> Option<Track> {
    let id = id?;
    db.get_track(id).ok().flatten().filter(|t| !t.missing && std::path::Path::new(&t.path).exists())
}

/// Čísla v názvu souboru ("HOD_16" → 16, "minuta 04" → 4) – bere se poslední číslo.
fn number_in_name(t: &Track) -> Option<u32> {
    let stem = std::path::Path::new(&t.filename).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut nums = vec![];
    let mut cur = String::new();
    for ch in stem.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() { cur.push(ch); } else if !cur.is_empty() { nums.push(cur.clone()); cur.clear(); }
    }
    nums.last().and_then(|n| n.parse().ok())
}

fn numbered(db: &Db, kind: Option<TrackKind>, category: &str, n: u32) -> Option<Track> {
    let kind = kind.unwrap_or(TrackKind::Asset);
    let tracks: Vec<Track> = db.with_conn(|c| -> rusqlite::Result<Vec<Track>> {
        let mut st = c.prepare(&format!("SELECT {TRACK_COLS} FROM tracks WHERE kind=?1 AND category=?2 AND missing=0 AND enabled=1"))?;
        let v = st.query_map(params![kind.as_str(), category], row_track)?.collect();
        v
    }).ok()?;
    tracks.into_iter().find(|t| number_in_name(t) == Some(n))
}

fn newest(db: &Db, kind: Option<TrackKind>, category: &str) -> Option<Track> {
    let kind = kind.unwrap_or(TrackKind::Asset);
    db.with_conn(|c| {
        c.query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE kind=?1 AND category=?2 AND missing=0 AND enabled=1 ORDER BY updated_at DESC, added_at DESC LIMIT 1"),
            params![kind.as_str(), category], row_track).ok()
    })
}

fn fallback(db: &Db, kind: &str, default_id: Option<i64>, why: String) -> Result<StitchPlan> {
    match track(db, default_id) {
        Some(t) => Ok(StitchPlan { kind: kind.into(), title: format!("{} (náhradní)", title_for(kind)), segments: vec![Segment::from_track(&t, "náhradní soubor")], bed: None, fallback: true }),
        None => Err(anyhow!("{why} a není nastavený náhradní soubor")),
    }
}

/// Sestaví plán. `upcoming` = položky ve frontě za stitchem (pro hooky).
pub fn resolve(db: &Db, cfg: &Config, kind: &str, upcoming: &[QueueItem], now: DateTime<Local>) -> Result<StitchPlan> {
    let sc = &cfg.stitcher;
    match kind {
        "hooks" => {
            let h = &sc.hooks;
            // aktuální data z knihovny (hooky mohly být nastavené až po zařazení do fronty)
            let hooks: Vec<Segment> = upcoming.iter()
                .filter(|q| q.track.kind() == TrackKind::Song && q.stitch.is_none())
                .map(|q| db.get_track(q.track.id).ok().flatten().unwrap_or_else(|| q.track.clone()))
                .filter_map(|t| match (t.hook_in_ms, t.hook_out_ms) {
                    (Some(a), Some(b)) if b > a => Some(Segment { path: t.path.clone(), start_ms: a, end_ms: b, label: t.display() }),
                    _ => None,
                })
                .take(h.count.max(1) as usize).collect();
            if hooks.is_empty() { return fallback(db, kind, h.default_track_id, "další skladby nemají nastavené hooky".into()); }
            let mut segs = vec![];
            if let Some(t) = track(db, h.opening_track_id) { segs.push(Segment::from_track(&t, "úvod")); }
            let sep = track(db, h.separator_track_id);
            let n = hooks.len();
            for (i, hk) in hooks.into_iter().enumerate() {
                segs.push(hk);
                if i + 1 < n { if let Some(s) = &sep { segs.push(Segment::from_track(s, "oddělovač")); } }
            }
            if let Some(t) = track(db, h.closing_track_id) { segs.push(Segment::from_track(&t, "závěr")); }
            Ok(StitchPlan { kind: kind.into(), title: title_for(kind).into(), segments: segs, bed: None, fallback: false })
        }
        "time" => {
            let t = &sc.time;
            let (hh, mm) = (now.hour(), now.minute());
            let Some(hour) = numbered(db, t.hours_kind, &t.hours_category, hh) else {
                return fallback(db, kind, t.default_track_id, format!("chybí nahrávka hodiny {hh} v kategorii '{}'", t.hours_category));
            };
            let minute = numbered(db, t.minutes_kind, &t.minutes_category, mm);
            if minute.is_none() && mm != 0 {
                return fallback(db, kind, t.default_track_id, format!("chybí nahrávka minuty {mm} v kategorii '{}'", t.minutes_category));
            }
            let mut segs = vec![];
            if let Some(o) = track(db, t.opening_track_id) { segs.push(Segment::from_track(&o, "úvod")); }
            segs.push(Segment::from_track(&hour, &format!("hodina {hh}")));
            if let Some(m) = minute { segs.push(Segment::from_track(&m, &format!("minuta {mm}"))); }
            let bed_len: i64 = segs.iter().map(|s| s.len_ms()).sum();
            if let Some(c) = track(db, t.closing_track_id) { segs.push(Segment::from_track(&c, "závěr")); }
            let bed = track(db, t.bed_track_id).map(|b| (Segment::from_track(&b, "podkres"), -10.0f32, bed_len));
            Ok(StitchPlan { kind: kind.into(), title: format!("{} {hh:02}:{mm:02}", title_for(kind)), segments: segs, bed, fallback: false })
        }
        "news" => {
            let n = &sc.news;
            let Some(news) = newest(db, n.news_kind, &n.news_category) else {
                return fallback(db, kind, n.default_track_id, format!("v kategorii '{}' nejsou žádné zprávy", n.news_category));
            };
            let mut segs = vec![];
            if let Some(o) = track(db, n.opening_track_id) { segs.push(Segment::from_track(&o, "úvod")); }
            segs.push(Segment::from_track(&news, &news.display()));
            let bed_len: i64 = segs.iter().map(|s| s.len_ms()).sum();
            if let Some(c) = track(db, n.closing_track_id) { segs.push(Segment::from_track(&c, "závěr")); }
            let bed = track(db, n.bed_track_id).map(|b| (Segment::from_track(&b, "podkres"), n.bed_db, bed_len));
            Ok(StitchPlan { kind: kind.into(), title: title_for(kind).into(), segments: segs, bed, fallback: false })
        }
        _ => Err(anyhow!("neznámý typ stitcheru '{kind}' (hooks | time | news)")),
    }
}

/// Položka fronty pro Stitcher (syntetická skladba; obsah se složí až při přehrání).
pub fn queue_item(kind: &str, estimate_ms: i64, reason: &str) -> QueueItem {
    let t = Track {
        id: 0, kind: Some(TrackKind::Asset), title: title_for(kind).into(), filename: format!("stitcher-{kind}"), path: String::new(),
        duration_ms: estimate_ms, cue_out_ms: estimate_ms, mix_ms: estimate_ms, enabled: true, analyzed: true, ..Default::default()
    };
    let mut q = QueueItem::new(t, "stitcher", reason);
    q.stitch = Some(kind.to_string());
    q
}
