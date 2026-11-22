//! Generátor hudebního plánu (logu).
//!
//! 1. Rozpis clocků na hodiny dne (UTC, místní čas stanice – přechody času: 23/25 hodin, dvojí 02:00).
//! 2. Pevné a uzamčené položky se zafixují, historie = skutečné hraní (as-played) + navazující plány.
//! 3. Pozice se plní v pořadí průchodů (kritické kategorie dřív). Pro každou pozici: kandidáti → tvrdé filtry
//!    (s počítadly vyřazení) → skóre z normalizovaných složek s uloženým rozkladem → deterministický výběr (seed).
//! 4. Hodiny s neobsazenými pozicemi se zkusí v omezeném počtu variant (jiné pořadí, jiný seed).
//! 5. Náhradní kategorie jen podle konfigurace (zaznamenáno jako „náhrada“). Neobsazené pozice mají důvody a návrh úpravy.

use super::attrs::{matches, SelTrack};
use super::{SelectorConfig, Slot};
use crate::model::TrackKind;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------------------------
// Hodiny dne

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HourSpec {
    pub hour_utc: DateTime<Utc>,
    pub date: NaiveDate,
    pub hour: u32,
    /// 1 = pondělí
    pub weekday: u32,
    /// "02:00" nebo "02:00 (2.)" při dvojí hodině
    pub label: String,
    pub clock: String,
    pub clock_source: String,
    /// Délka hodiny (při přechodu času může být zkrácená – vždy 60 min v UTC).
    pub len_ms: i64,
}

/// Hodiny místního dne [00:00, 24:00) v časovém pásmu stanice, jako UTC začátky.
pub fn day_hours<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> Vec<(DateTime<Utc>, u32, String)> {
    let start = tz.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|d| d.with_timezone(&Utc));
    let next = date.succ_opt().unwrap();
    let end = tz.from_local_datetime(&next.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|d| d.with_timezone(&Utc));
    let (Some(mut t), Some(end)) = (start, end) else { return vec![] };
    let mut v: Vec<(DateTime<Utc>, u32, String)> = vec![];
    while t < end {
        let local = t.with_timezone(tz);
        let h = local.hour();
        let dup = v.iter().filter(|(_, hh, _)| *hh == h).count();
        let label = if dup > 0 { format!("{h:02}:00 ({}.)", dup + 1) } else { format!("{h:02}:00") };
        if dup > 0 { if let Some(first) = v.iter_mut().find(|(_, hh, l)| *hh == h && !l.contains('(')) { first.2 = format!("{h:02}:00 (1.)"); } }
        v.push((t, h, label));
        t += Duration::hours(1);
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Plán

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PlanItem {
    pub pos: usize,
    pub slot: Slot,
    pub track_id: Option<i64>,
    /// auto | fixed | fallback | manual | locked | unfilled | marker
    pub origin: String,
    pub locked: bool,
    pub score: Option<f64>,
    pub breakdown: Vec<(String, f64)>,
    /// Důvody neobsazení / poznámka k výjimce
    pub note: String,
    pub est_start: Option<DateTime<Utc>>,
    pub length_ms: i64,
    /// id v databázi (pokud uloženo)
    #[serde(default)]
    pub id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanHour { pub spec: HourSpec, pub items: Vec<PlanItem>, pub total_ms: i64, pub over_ms: i64, pub fixed_point: Option<String> }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GenStats {
    pub slots: usize,
    pub filled: usize,
    pub unfilled: usize,
    pub fallback: usize,
    pub compromises: usize,
    pub avg_score: f64,
    pub elapsed_ms: u64,
    pub variants_tried: usize,
    pub rejected: BTreeMap<String, usize>,
    pub hints: Vec<String>,
    pub hours_over: usize,
    pub hours_under: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayPlan { pub day: String, pub hours: Vec<PlanHour>, pub stats: GenStats, pub seed: u64 }

/// Předchozí nebo navazující hraní (as-played nebo plán).
#[derive(Debug, Clone)]
pub struct Placement { pub track_id: i64, pub at: i64, pub date: NaiveDate, pub hour: u32 }

// ---------------------------------------------------------------------------------------------
// Evidence umístění (rychlé kontroly odstupů)

#[derive(Clone, Default)]
struct Ledger {
    by_track: HashMap<i64, Vec<i64>>,
    by_artist: HashMap<String, Vec<i64>>,
    by_work: HashMap<String, Vec<i64>>,
    by_album: HashMap<String, Vec<i64>>,
    same_hour: HashMap<i64, Vec<(NaiveDate, u32)>>,
    day_track: HashMap<(NaiveDate, i64), u32>,
    day_artist: HashMap<(NaiveDate, String), u32>,
    /// (scope, attr, value) → počet / (scope) → celkem; scope = "d:2026-11-22" | "h:<utc>"
    goal_hits: HashMap<(String, String, String), f64>,
    goal_total: HashMap<String, f64>,
}

fn ins(v: &mut Vec<i64>, t: i64) { let i = v.partition_point(|x| *x < t); v.insert(i, t); }
fn rm(v: &mut Vec<i64>, t: i64) { if let Some(i) = v.iter().position(|x| *x == t) { v.remove(i); } }
/// Nejmenší vzdálenost (ms) od času t, None = žádné hraní.
fn nearest(v: Option<&Vec<i64>>, t: i64) -> Option<i64> {
    let v = v?;
    if v.is_empty() { return None; }
    let i = v.partition_point(|x| *x < t);
    let mut best = i64::MAX;
    if i < v.len() { best = best.min((v[i] - t).abs()); }
    if i > 0 { best = best.min((t - v[i - 1]).abs()); }
    Some(best)
}

impl Ledger {
    fn add(&mut self, tr: &SelTrack, at: i64, date: NaiveDate, hour: u32, goals: &[(String, String)], hour_key: &str, mult: f64) {
        ins(self.by_track.entry(tr.id).or_default(), at);
        for a in &tr.artists { ins(self.by_artist.entry(a.clone()).or_default(), at); }
        ins(self.by_work.entry(tr.work.clone()).or_default(), at);
        if !tr.album.is_empty() { ins(self.by_album.entry(tr.album.to_lowercase()).or_default(), at); }
        self.same_hour.entry(tr.id).or_default().push((date, hour));
        *self.day_track.entry((date, tr.id)).or_default() += 1;
        for a in &tr.artists { *self.day_artist.entry((date, a.clone())).or_default() += 1; }
        self.goal_add(tr, date, hour_key, goals, mult);
    }
    fn goal_add(&mut self, tr: &SelTrack, date: NaiveDate, hour_key: &str, goals: &[(String, String)], mult: f64) {
        for scope in [format!("d:{date}"), format!("h:{hour_key}")] {
            *self.goal_total.entry(scope.clone()).or_default() += mult;
            for (attr, _) in goals {
                if let Some(v) = tr.attr(attr) { *self.goal_hits.entry((scope.clone(), attr.clone(), v.to_lowercase())).or_default() += mult; }
            }
        }
    }
    fn remove(&mut self, tr: &SelTrack, at: i64, date: NaiveDate, hour: u32, goals: &[(String, String)], hour_key: &str, mult: f64) {
        if let Some(v) = self.by_track.get_mut(&tr.id) { rm(v, at); }
        for a in &tr.artists { if let Some(v) = self.by_artist.get_mut(a) { rm(v, at); } }
        if let Some(v) = self.by_work.get_mut(&tr.work) { rm(v, at); }
        if !tr.album.is_empty() { if let Some(v) = self.by_album.get_mut(&tr.album.to_lowercase()) { rm(v, at); } }
        if let Some(v) = self.same_hour.get_mut(&tr.id) { if let Some(i) = v.iter().position(|x| *x == (date, hour)) { v.remove(i); } }
        if let Some(c) = self.day_track.get_mut(&(date, tr.id)) { *c = c.saturating_sub(1); }
        for a in &tr.artists { if let Some(c) = self.day_artist.get_mut(&(date, a.clone())) { *c = c.saturating_sub(1); } }
        for scope in [format!("d:{date}"), format!("h:{hour_key}")] {
            if let Some(t) = self.goal_total.get_mut(&scope) { *t -= mult; }
            for (attr, _) in goals { if let Some(v) = tr.attr(attr) { if let Some(h) = self.goal_hits.get_mut(&(scope.clone(), attr.clone(), v.to_lowercase())) { *h -= mult; } } }
        }
    }
}

fn mix(seed: u64, a: u64, b: u64) -> u64 {
    let mut z = seed ^ a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------------------------
// Kontext generování

pub struct Ctx<'a> {
    pub cfg: &'a SelectorConfig,
    pub tracks: HashMap<i64, &'a SelTrack>,
    /// kategorie → způsobilé skladby
    pub pools: HashMap<(TrackKind, String), Vec<&'a SelTrack>>,
    /// kategorie → (pozic za hodinu, průměrná délka)
    pub cat_rate: HashMap<String, (f64, i64)>,
    goals: Vec<(String, String)>,
    ledger: Ledger,
    /// Skladby, kterým se při přegenerování vyhnout (jiná varianta) – poloviční skóre.
    pub avoid: std::collections::HashSet<i64>,
    /// Twofer: druhá skladba musí sdílet interpreta s první.
    pub require_artist: Option<Vec<String>>,
}

pub const R_SONG: &str = "Odstup skladby";
pub const R_ARTIST: &str = "Odstup interpreta";
pub const R_WORK: &str = "Odstup verzí téže písně";
pub const R_ALBUM: &str = "Odstup alba";
pub const R_SAME_HOUR: &str = "Stejná hodina v předchozích dnech";
pub const R_DAY_SONG: &str = "Max. skladby za den";
pub const R_DAY_ARTIST: &str = "Max. interpreta za den";
pub const R_TRANSITION: &str = "Zakázaný přechod";
pub const R_IN_ROW: &str = "Stejná hodnota za sebou";
pub const R_EXPLICIT: &str = "Explicitní v tomto čase";
pub const R_UNKNOWN: &str = "Neznámý požadovaný atribut";
pub const R_ATTR: &str = "Nevyhovující atribut pozice";
pub const R_DRIVE: &str = "Hraje už v jiné špičce dne";
pub const R_INACTIVE: &str = "Neaktivní / pozastavená / chybí soubor";
pub const R_GOAL: &str = "Závazný cíl zastoupení";
pub const R_INTRO: &str = "Krátké intro pro hlasový sweeper";
pub const R_TWOFER: &str = "Twofer: jiný interpret";

const MIN: i64 = 60_000;

impl<'a> Ctx<'a> {
    pub fn new(cfg: &'a SelectorConfig, tracks: &'a [SelTrack]) -> Self {
        let mut pools: HashMap<(TrackKind, String), Vec<&SelTrack>> = HashMap::new();
        for t in tracks { pools.entry((t.kind, t.category.clone())).or_default().push(t); }
        for v in pools.values_mut() { v.sort_by_key(|t| t.id); }
        // poptávka po kategoriích z mřížky (týden)
        let mut slots: HashMap<String, f64> = HashMap::new();
        for d in 1..=7u32 { for h in 0..24u32 {
            let date = NaiveDate::from_isoywd_opt(2024, 10, chrono::Weekday::try_from((d - 1) as u8).unwrap()).unwrap();
            if let Some((c, _)) = cfg.clock_for(date, d, h) { for s in &c.slots { if s.kind == "music" { *slots.entry(s.category.clone()).or_default() += 1.0; } } }
        } }
        let cat_rate = slots.into_iter().map(|(c, n)| {
            let avg = pools.iter().filter(|((_, cc), _)| *cc == c).flat_map(|(_, v)| v.iter().map(|t| t.len_ms)).filter(|l| *l > 0).collect::<Vec<_>>();
            let a = if avg.is_empty() { 210_000 } else { avg.iter().sum::<i64>() / avg.len() as i64 };
            (c, (n / 168.0, a))
        }).collect();
        let goals = cfg.goals.iter().map(|g| (g.attr.clone(), g.value.to_lowercase())).collect();
        Self { cfg, tracks: tracks.iter().map(|t| (t.id, t)).collect(), pools, cat_rate, goals, ledger: Ledger::default(), avoid: Default::default(), require_artist: None }
    }

    fn mult(&self, tr: &SelTrack) -> f64 { if self.cfg.goals.iter().any(|g| g.unit == "minutes") { (tr.len_ms as f64 / 60_000.0).max(0.1) } else { 1.0 } }

    pub fn add_history(&mut self, p: &Placement) {
        if let Some(tr) = self.tracks.get(&p.track_id).copied() {
            let hk = format!("{}", p.at / 3_600_000);
            let m = self.mult(tr);
            self.ledger.add(tr, p.at, p.date, p.hour, &self.goals, &hk, m);
        }
    }

    fn turnover_ms(&self, cat: &str, eligible: usize) -> i64 {
        let rate = self.cat_rate.get(cat).map(|r| r.0).unwrap_or(1.0).max(0.01);
        let speed = match self.cfg.category(cat).map(|c| c.rotation.as_str()) { Some("fast") => 0.8, Some("slow") => 1.25, _ => 1.0 };
        ((eligible as f64 / rate) * 3_600_000.0 * speed) as i64
    }

    fn avg_len(&self, slot: &Slot) -> i64 {
        if let Some(e) = slot.est_s { if !matches!(slot.kind.as_str(), "music" | "fixed") { return e as i64 * 1000; } }
        match slot.kind.as_str() {
            "music" => self.cat_rate.get(&slot.category).map(|r| r.1).unwrap_or(210_000),
            "fixed" => slot.track_id.and_then(|id| self.tracks.get(&id)).map(|t| t.len_ms).unwrap_or(210_000),
            "imaging" => 8_000,
            "news" | "stitch" => 120_000,
            "vt" => 15_000,
            _ => 0,
        }
    }
}

/// Výsledek kontroly kandidáta.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate { pub track_id: i64, pub artist: String, pub title: String, pub category: String, pub score: f64, pub breakdown: Vec<(String, f64)>, pub violations: Vec<String>, pub len_ms: i64 }

struct Neigh<'a> { prev: Option<&'a SelTrack>, next: Option<&'a SelTrack>, row_prev: Vec<&'a SelTrack>, row_next: Vec<&'a SelTrack>, prev_positions: Vec<&'a SelTrack> }

impl<'a> Ctx<'a> {
    /// Tvrdé kontroly: vrací seznam porušení (prázdný = způsobilý).
    fn hard(&self, tr: &SelTrack, slot: &Slot, at: i64, date: NaiveDate, hour: u32, nb: &Neigh, day: &str, music: bool) -> Vec<&'static str> {
        let r = &self.cfg.rules;
        let l = &self.ledger;
        let mut v = vec![];
        if !tr.active_on(day) { v.push(R_INACTIVE); return v; }
        for (k, req) in &slot.attrs {
            match matches(tr.attr(k).as_deref(), req) { None => v.push(R_UNKNOWN), Some(false) => v.push(R_ATTR), _ => {} }
        }
        if !music { return v; }
        if tr.attrs.explicit == Some(true) && r.explicit_forbidden.iter().any(|(f, t)| hour >= *f && hour < *t) { v.push(R_EXPLICIT); }
        let sep = |w: u32, x: Option<i64>| w > 0 && x.map(|d| d < w as i64 * MIN).unwrap_or(false);
        if sep(r.song_sep_min, nearest(l.by_track.get(&tr.id), at)) { v.push(R_SONG); }
        if r.protect_versions && sep(r.work_sep_min, nearest(l.by_work.get(&tr.work), at)) && !v.contains(&R_SONG) { v.push(R_WORK); }
        if tr.artists.iter().any(|a| sep(r.artist_sep_min, nearest(l.by_artist.get(a), at))) { v.push(R_ARTIST); }
        if r.artist_sep_positions > 0 && nb.prev_positions.iter().take(r.artist_sep_positions as usize).any(|p| p.artists.iter().any(|a| tr.artists.contains(a))) && !v.contains(&R_ARTIST) { v.push(R_ARTIST); }
        if !tr.album.is_empty() && sep(r.album_sep_min, nearest(l.by_album.get(&tr.album.to_lowercase()), at)) { v.push(R_ALBUM); }
        if r.same_hour_hard && r.same_hour_days > 0 {
            if let Some(h) = l.same_hour.get(&tr.id) {
                if h.iter().any(|(d, hh)| *hh == hour && *d < date && (date - *d).num_days() <= r.same_hour_days as i64) { v.push(R_SAME_HOUR); }
            }
        }
        if r.max_song_per_day > 0 && l.day_track.get(&(date, tr.id)).copied().unwrap_or(0) >= r.max_song_per_day { v.push(R_DAY_SONG); }
        if r.max_artist_per_day > 0 && tr.artists.iter().any(|a| l.day_artist.get(&(date, a.clone())).copied().unwrap_or(0) >= r.max_artist_per_day) { v.push(R_DAY_ARTIST); }
        if r.protect_drive_times {
            let drive = |h: u32| (6..10).contains(&h) || (15..19).contains(&h);
            if drive(hour) { if let Some(h) = l.same_hour.get(&tr.id) { if h.iter().any(|(d, hh)| *d == date && drive(*hh) && (*hh < 12) != (hour < 12)) { v.push(R_DRIVE); } } }
        }
        let themed = !slot.theme.is_empty();
        for t in self.cfg.transitions.iter().filter(|t| t.level == "forbidden" && !themed) {
            let pair = |a: &SelTrack, b: &SelTrack| matches(a.attr(&t.attr).as_deref(), &t.from) == Some(true) && matches(b.attr(&t.attr).as_deref(), &t.to) == Some(true);
            if nb.prev.map(|p| pair(p, tr)).unwrap_or(false) || nb.next.map(|n| pair(tr, n)).unwrap_or(false) { v.push(R_TRANSITION); break; }
        }
        for (attr, max) in r.max_in_row.iter().filter(|_| !themed) {
            let Some(val) = tr.attr(attr) else { continue };
            let same = |x: &&&SelTrack| x.attr(attr).as_deref() == Some(val.as_str());
            let run = 1 + nb.row_prev.iter().take_while(same).count() + nb.row_next.iter().take_while(same).count();
            if run as u32 > *max { v.push(R_IN_ROW); break; }
        }
        for g in self.cfg.goals.iter().filter(|g| g.severity == "hard" && (g.daypart.is_empty() || self.cfg.daypart_of(hour).map(|d| d.name == g.daypart).unwrap_or(false))) {
            if tr.attr(&g.attr).map(|x| x.to_lowercase()) != Some(g.value.to_lowercase()) { continue; }
            let scope = if g.scope == "hour" { format!("h:{}", at / 3_600_000) } else { format!("d:{date}") };
            let total = l.goal_total.get(&scope).copied().unwrap_or(0.0) + 1.0;
            let hits = l.goal_hits.get(&(scope, g.attr.clone(), g.value.to_lowercase())).copied().unwrap_or(0.0) + 1.0;
            if total >= 4.0 && hits / total * 100.0 > g.target_pct + g.tolerance_pct { v.push(R_GOAL); break; }
        }
        v
    }

    /// Měkké skóre 0–1 s rozkladem.
    fn soft(&self, tr: &SelTrack, slot: &Slot, at: i64, date: NaiveDate, hour: u32, nb: &Neigh, eligible: usize, hour_over: i64, seed: u64, key: u64) -> (f64, Vec<(String, f64)>) {
        let r = &self.cfg.rules;
        let w = &self.cfg.weights;
        let l = &self.ledger;
        let mut parts: Vec<(String, f64, f64)> = vec![];
        let turnover = self.turnover_ms(&slot.category, eligible).max(MIN);
        let rest = match nearest(l.by_track.get(&tr.id), at) { None => 1.0, Some(g) => ((g as f64 / turnover as f64).min(1.5)) / 1.5 };
        parts.push(("odpočinek".into(), rest, w.rest));
        let rate = self.cat_rate.get(&slot.category).map(|x| x.0).unwrap_or(1.0);
        let expected = (rate * 24.0 * 7.0 / eligible.max(1) as f64).max(0.01);
        let week = l.by_track.get(&tr.id).map(|v| v.iter().filter(|x| (at - **x).abs() < 7 * 24 * 60 * MIN / 2 * 2).count()).unwrap_or(0) as f64;
        parts.push(("opomíjená".into(), 1.0 / (1.0 + week / expected), w.underplayed));
        let soft_sep = |target: u32, d: Option<i64>| if target == 0 { 1.0 } else { d.map(|x| (x as f64 / (target as f64 * MIN as f64)).min(1.0)).unwrap_or(1.0) };
        let s1 = soft_sep(r.song_target_min, nearest(l.by_track.get(&tr.id), at));
        let s2 = tr.artists.iter().map(|a| soft_sep(r.artist_target_min, nearest(l.by_artist.get(a), at))).fold(1.0, f64::min);
        parts.push(("odstupy".into(), (s1 + s2) / 2.0, w.separation));
        if r.same_hour_days > 0 && !r.same_hour_hard {
            let k = l.same_hour.get(&tr.id).map(|h| h.iter().filter(|(d, hh)| *hh == hour && *d < date && (date - *d).num_days() <= r.same_hour_days as i64).count()).unwrap_or(0);
            parts.push(("stejná hodina".into(), 1.0 - (k as f64 / r.same_hour_days as f64).min(1.0), w.same_hour));
        }
        let soft_goals: Vec<_> = self.cfg.goals.iter().filter(|g| g.severity != "hard" && (g.daypart.is_empty() || self.cfg.daypart_of(hour).map(|d| d.name == g.daypart).unwrap_or(false))).collect();
        if !soft_goals.is_empty() {
            let mut sum = 0.0; let mut ws = 0.0;
            for g in soft_goals {
                let scope = if g.scope == "hour" { format!("h:{}", at / 3_600_000) } else { format!("d:{date}") };
                let total = l.goal_total.get(&scope).copied().unwrap_or(0.0);
                let hits = l.goal_hits.get(&(scope, g.attr.clone(), g.value.to_lowercase())).copied().unwrap_or(0.0);
                let share = if total > 0.0 { hits / total * 100.0 } else { g.target_pct };
                let val = tr.attr(&g.attr);
                let c = match val.as_deref().map(|x| x.eq_ignore_ascii_case(&g.value)) {
                    None => 0.5,
                    Some(true) => if share < g.target_pct - g.tolerance_pct { 1.0 } else if share > g.target_pct + g.tolerance_pct { 0.0 } else if share < g.target_pct { 0.8 } else { 0.4 },
                    Some(false) => if share < g.target_pct - g.tolerance_pct { 0.1 } else if share > g.target_pct + g.tolerance_pct { 1.0 } else { 0.6 },
                };
                sum += c * g.weight; ws += g.weight;
            }
            parts.push(("cíle formátu".into(), if ws > 0.0 { sum / ws } else { 0.5 }, w.goals));
        }
        let und: Vec<_> = self.cfg.transitions.iter().filter(|t| t.level == "undesirable").collect();
        if !und.is_empty() && slot.theme.is_empty() {
            let bad = und.iter().any(|t| {
                let pair = |a: &SelTrack, b: &SelTrack| matches(a.attr(&t.attr).as_deref(), &t.from) == Some(true) && matches(b.attr(&t.attr).as_deref(), &t.to) == Some(true);
                nb.prev.map(|p| pair(p, tr)).unwrap_or(false) || nb.next.map(|n| pair(tr, n)).unwrap_or(false)
            });
            parts.push(("přechody".into(), if bad { 0.0 } else { 1.0 }, w.transitions));
        }
        if r.energy_max_jump > 0 {
            let e = tr.attrs.energy;
            let jump = |o: Option<&SelTrack>| match (e, o.and_then(|x| x.attrs.energy)) { (Some(a), Some(b)) => if (a - b).unsigned_abs() as u32 > r.energy_max_jump { 0.0 } else { 1.0 }, _ => 0.7 };
            parts.push(("energie".into(), (jump(nb.prev) + jump(nb.next)) / 2.0, w.energy_flow));
        }
        let avg = self.cat_rate.get(&slot.category).map(|x| x.1).unwrap_or(210_000).max(1);
        let lc = if hour_over == 0 { 0.5 } else { (0.5 - hour_over.signum() as f64 * (tr.len_ms - avg) as f64 / avg as f64).clamp(0.0, 1.0) };
        parts.push(("délka".into(), lc, w.length));
        let ws: f64 = parts.iter().map(|p| p.2).sum::<f64>().max(1e-9);
        let base: f64 = parts.iter().map(|p| p.1 * p.2).sum::<f64>() / ws;
        let tie = (mix(seed, key, tr.id as u64) % 1000) as f64 / 1_000_000.0;
        let avoid = if self.avoid.contains(&tr.id) { 0.5 } else { 1.0 };
        let score = (base * tr.attrs.weight.max(0.0) * avoid).min(1.0) + tie;
        (score, parts.into_iter().map(|(n, c, _)| (n, (c * 1000.0).round() / 1000.0)).collect())
    }
}

// ---------------------------------------------------------------------------------------------
// Generování dne

pub struct DayInput<'a> {
    pub date: NaiveDate,
    pub hours: Vec<(DateTime<Utc>, u32, String)>,
    /// Uzamčené / odvysílané položky z předchozí revize: (hour_utc, pos) → položka
    pub keep: HashMap<(DateTime<Utc>, usize), PlanItem>,
    pub seed: u64,
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
}

fn is_music(slot: &Slot) -> bool { slot.kind == "music" || slot.kind == "fixed" }

fn neighbours<'b>(ctx: &'b Ctx, items: &[PlanItem], idx: usize) -> Neigh<'b> {
    let tr = |i: &PlanItem| -> Option<&'b SelTrack> { if is_music(&i.slot) { i.track_id.and_then(|id| ctx.tracks.get(&id).copied()) } else { None } };
    let before: Vec<&PlanItem> = items[..idx].iter().rev().filter(|i| is_music(&i.slot)).collect();
    let after: Vec<&PlanItem> = items[idx + 1..].iter().filter(|i| is_music(&i.slot)).collect();
    let row_prev: Vec<&SelTrack> = before.iter().map_while(|i| tr(i)).collect();
    let row_next: Vec<&SelTrack> = after.iter().map_while(|i| tr(i)).collect();
    Neigh { prev: before.first().and_then(|i| tr(i)), next: after.first().and_then(|i| tr(i)), prev_positions: row_prev.clone(), row_prev, row_next }
}

/// Kandidáti pro pozici seřazení podle skóre (způsobilí první). `limit` = kolik vrátit.
pub fn candidates(ctx: &Ctx, hour: &PlanHour, idx: usize, day: &str, seed: u64, include_violations: bool, limit: usize, cat_override: Option<&str>) -> (Vec<Candidate>, BTreeMap<String, usize>) {
    let item = &hour.items[idx];
    let slot = &item.slot;
    let music = slot.kind == "music";
    let cat = match cat_override { Some(c) => c.to_string(), None if music => ctx.cfg.effective_category(&slot.category, hour.spec.hour), None => slot.category.clone() };
    let kind = if music { ctx.cfg.category(&cat).map(|c| c.kind).unwrap_or(TrackKind::Song) } else { slot.track_kind.unwrap_or(TrackKind::Jingle) };
    let pool: Vec<&SelTrack> = if slot.kind == "imaging" && cat.is_empty() {
        ctx.pools.iter().filter(|((k, _), _)| *k == kind).flat_map(|(_, v)| v.iter().copied()).collect()
    } else { ctx.pools.get(&(kind, cat.clone())).cloned().unwrap_or_default() };
    let at = item.est_start.map(|t| t.timestamp_millis()).unwrap_or(hour.spec.hour_utc.timestamp_millis());
    let nb = neighbours(ctx, &hour.items, idx);
    let eligible = pool.iter().filter(|t| t.active_on(day)).count();
    let mut rejected: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = vec![];
    let key = mix(at as u64, idx as u64, 7);
    // imaging: rotace podle nejdéle nehraného, u hlasového sweeperu přes intro kontrola délky
    let next_intro = if slot.kind == "imaging" && slot.position.as_deref().map(|p| p == "start" || p == "before_intro").unwrap_or(false) {
        nb.next.map(|t| t.intro_ms).filter(|i| *i > 0)
    } else { None };
    for t in pool.iter().take(ctx.cfg.max_candidates.max(10) * 20) {
        let mut viol: Vec<String> = ctx.hard(t, slot, at, hour.spec.date, hour.spec.hour, &nb, day, music).into_iter().map(String::from).collect();
        if let Some(intro) = next_intro { if t.len_ms > intro + 500 { viol.push(R_INTRO.into()); } }
        if let Some(req) = &ctx.require_artist { if !t.artists.iter().any(|a| req.contains(a)) { viol.push(R_TWOFER.into()); } }
        if !music && slot.kind == "imaging" {
            let min_gap = 30 * MIN;
            if nearest(ctx.ledger.by_track.get(&t.id), at).map(|d| d < min_gap).unwrap_or(false) { viol.push(R_SONG.into()); }
        }
        if !viol.is_empty() {
            for r in &viol { *rejected.entry(r.clone()).or_default() += 1; }
            if !include_violations { continue; }
        }
        let (score, breakdown) = ctx.soft(t, slot, at, hour.spec.date, hour.spec.hour, &nb, eligible, hour.over_ms, seed, key);
        out.push(Candidate { track_id: t.id, artist: t.artist.clone(), title: t.title.clone(), category: t.category.clone(), score, breakdown, violations: viol, len_ms: t.len_ms });
    }
    out.sort_by(|a, b| a.violations.len().cmp(&b.violations.len()).then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)));
    out.truncate(limit);
    (out, rejected)
}

fn retime(ctx: &Ctx, h: &mut PlanHour) {
    let start = h.spec.hour_utc;
    let mut t = 0i64;
    let mut fixed: Option<String> = None;
    for it in h.items.iter_mut() {
        if let Some(m) = it.slot.at_min { let f = m as i64 * MIN; if f > t { t = f; } if fixed.is_none() { fixed = Some(format!(":{m:02}")); } }
        it.est_start = Some(start + Duration::milliseconds(t));
        let len = match it.track_id.and_then(|id| ctx.tracks.get(&id)) {
            Some(tr) => if it.slot.kind == "imaging" && it.slot.position.as_deref().map(|p| p != "independent").unwrap_or(false) { 0 } else { tr.len_ms },
            None => if it.origin == "unfilled" { ctx.avg_len(&it.slot) } else { ctx.avg_len(&it.slot) },
        };
        it.length_ms = len;
        t += len;
    }
    h.total_ms = t;
    h.over_ms = t - h.spec.len_ms;
    h.fixed_point = fixed;
}

fn place(ctx: &mut Ctx, h: &mut PlanHour, idx: usize, day: &str, seed: u64, stats: &mut GenStats) -> bool {
    let (c, rej) = candidates(ctx, h, idx, day, seed, false, 1, None);
    let mut chosen = c.into_iter().next().map(|c| (c, "auto"));
    let mut rej_all = rej.clone();
    if chosen.is_none() && h.items[idx].slot.kind == "music" {
        if let Some(fb) = ctx.cfg.category(&h.items[idx].slot.category).map(|c| c.fallback.clone()).filter(|f| !f.is_empty()) {
            let (c2, rej2) = candidates(ctx, h, idx, day, seed, false, 1, Some(&fb));
            for (k, v) in rej2 { *rej_all.entry(format!("{k} (náhrada)")).or_default() += v; }
            chosen = c2.into_iter().next().map(|c| (c, "fallback"));
        }
    }
    let it = &mut h.items[idx];
    match chosen {
        Some((c, origin)) => {
            it.track_id = Some(c.track_id);
            it.origin = origin.into();
            it.score = Some((c.score * 1000.0).round() / 1000.0);
            it.breakdown = c.breakdown;
            it.note = if origin == "fallback" { "náhradní kategorie podle konfigurace".into() } else { String::new() };
            if let Some(tr) = ctx.tracks.get(&c.track_id).copied() {
                let at = it.est_start.map(|t| t.timestamp_millis()).unwrap_or(0);
                if is_music(&it.slot) { let m = ctx.mult(tr); let hk = format!("{}", at / 3_600_000); ctx.ledger.add(tr, at, h.spec.date, h.spec.hour, &ctx.goals.clone(), &hk, m); }
                else { ins(ctx.ledger.by_track.entry(tr.id).or_default(), at); }
            }
            if h.items[idx].slot.twofer && ctx.require_artist.is_none() { place_twofer(ctx, h, idx, day, seed); }
            true
        }
        None => {
            it.track_id = None;
            it.origin = "unfilled".into();
            it.score = None;
            let total: usize = ctx.pools.get(&(ctx.cfg.category(&it.slot.category).map(|c| c.kind).unwrap_or(TrackKind::Song), it.slot.category.clone())).map(|v| v.len()).unwrap_or(0);
            let mut top: Vec<_> = rej_all.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            it.note = if total == 0 && it.slot.kind == "music" { format!("Kategorie „{}“ nemá žádné skladby.", it.slot.category) }
                else { top.iter().take(4).map(|(k, v)| format!("{k}: vyřadil {v} z {total}")).collect::<Vec<_>>().join("; ") };
            for (k, v) in rej_all { *stats.rejected.entry(k).or_default() += v; }
            false
        }
    }
}

/// Twofer: k právě umístěné skladbě dá na další hudební pozici skladbu stejného interpreta.
/// Odstup interpreta se pro tuto dvojici neuplatní (první skladba se na chvíli vyjme z evidence).
fn place_twofer(ctx: &mut Ctx, h: &mut PlanHour, idx: usize, day: &str, seed: u64) {
    let Some(partner) = (idx + 1..h.items.len()).find(|&j| h.items[j].slot.kind == "music") else { return };
    if !matches!(h.items[partner].origin.as_str(), "pending" | "unfilled") { return; }
    let Some(first) = h.items[idx].track_id.and_then(|id| ctx.tracks.get(&id).copied()) else { return };
    let at = h.items[idx].est_start.map(|t| t.timestamp_millis()).unwrap_or(0);
    let hk = format!("{}", at / 3_600_000);
    let m = ctx.mult(first);
    let goals = ctx.goals.clone();
    ctx.ledger.remove(first, at, h.spec.date, h.spec.hour, &goals, &hk, m);
    ctx.require_artist = Some(first.artists.clone());
    let mut dummy = GenStats::default();
    let ok = place(ctx, h, partner, day, seed, &mut dummy);
    ctx.require_artist = None;
    ctx.ledger.add(first, at, h.spec.date, h.spec.hour, &goals, &hk, m);
    if ok { h.items[partner].origin = "twofer".into(); h.items[partner].note = format!("twofer s „{} – {}“", first.artist, first.title); }
    else { h.items[partner].origin = "pending".into(); h.items[partner].note = "Twofer: žádná další skladba interpreta – doplněno běžně".into(); }
}

fn unplace(ctx: &mut Ctx, h: &mut PlanHour, idx: usize) {
    let it = &mut h.items[idx];
    if let Some(tr) = it.track_id.and_then(|id| ctx.tracks.get(&id).copied()) {
        let at = it.est_start.map(|t| t.timestamp_millis()).unwrap_or(0);
        if is_music(&it.slot) { let m = ctx.mult(tr); let hk = format!("{}", at / 3_600_000); ctx.ledger.remove(tr, at, h.spec.date, h.spec.hour, &ctx.goals.clone(), &hk, m); }
        else if let Some(v) = ctx.ledger.by_track.get_mut(&tr.id) { rm(v, at); }
    }
    it.track_id = None;
}

/// Vygeneruje plán jednoho dne. Kontext obsahuje historii; umístěné položky do něj přibudou (pro následující dny).
pub fn generate_day(ctx: &mut Ctx, inp: DayInput) -> DayPlan {
    let started = std::time::Instant::now();
    let cfg = ctx.cfg;
    let day = inp.date.format("%Y-%m-%d").to_string();
    let mut stats = GenStats::default();
    let mut hours: Vec<PlanHour> = inp.hours.iter().map(|(utc, h, label)| {
        let wd = inp.date.weekday().number_from_monday();
        let (clock, src) = cfg.clock_for(inp.date, wd, *h).map(|(c, s)| (Some(c), s)).unwrap_or((None, "bez schématu".into()));
        let mut slots: Vec<Slot> = vec![];
        for s in clock.map(|c| c.slots.clone()).unwrap_or_default() {
            if s.kind == "set" {
                if let Some(set) = cfg.sets.iter().find(|x| x.name == s.set) {
                    let mut ids = set.track_ids.clone();
                    if !set.keep_order { ids.sort_by_key(|id| mix(inp.seed, *id as u64, *h as u64)); }
                    for id in ids { slots.push(Slot { kind: "fixed".into(), track_id: Some(id), label: set.name.clone(), theme: set.name.clone(), set: set.name.clone(), ..Default::default() }); }
                    continue;
                }
            }
            slots.push(s);
        }
        let items = slots.into_iter().enumerate().map(|(i, s)| PlanItem { pos: i, origin: if is_music(&s) || s.kind == "imaging" { "pending".into() } else { "marker".into() }, slot: s, ..Default::default() }).collect();
        PlanHour { spec: HourSpec { hour_utc: *utc, date: inp.date, hour: *h, weekday: wd, label: label.clone(), clock: clock.map(|c| c.name.clone()).unwrap_or_default(), clock_source: src, len_ms: 3_600_000 }, items, total_ms: 0, over_ms: 0, fixed_point: None }
    }).collect();
    // zafixovat pevné a uzamčené
    for h in hours.iter_mut() {
        retime(ctx, h);
        for i in 0..h.items.len() {
            if let Some(k) = inp.keep.get(&(h.spec.hour_utc, i)) { if k.slot == h.items[i].slot || k.locked {
                let mut k = k.clone(); k.pos = i; k.est_start = h.items[i].est_start; h.items[i] = k;
            } }
            let it = &mut h.items[i];
            if it.slot.kind == "fixed" && it.track_id.is_none() { it.track_id = it.slot.track_id; it.origin = if it.slot.set.is_empty() { "fixed".into() } else { "set".into() }; }
            if it.track_id.is_some() && it.origin != "pending" {
                if let Some(tr) = it.track_id.and_then(|id| ctx.tracks.get(&id).copied()) {
                    let at = it.est_start.map(|t| t.timestamp_millis()).unwrap_or(0);
                    if is_music(&it.slot) { let m = ctx.mult(tr); let hk = format!("{}", at / 3_600_000); ctx.ledger.add(tr, at, h.spec.date, h.spec.hour, &ctx.goals.clone(), &hk, m); }
                }
            }
        }
    }
    // pořadí průchodů: hudba podle pass kategorie, pak imaging
    let mut order: Vec<(u32, usize, usize, usize)> = vec![];
    let n = hours.len().max(1);
    let day_ord = inp.date.num_days_from_ce() as usize;
    for (hi, h) in hours.iter().enumerate() { for (ii, it) in h.items.iter().enumerate() {
        if it.origin != "pending" { continue; }
        let pass = if it.slot.kind == "music" { cfg.category(&it.slot.category).map(|c| c.pass).unwrap_or(50) } else { 1000 };
        // reverzní / posunuté rozložení: mění, která hodina dostane „nejodpočatější“ skladby → skladby se nevrací do stejných hodin
        let rank = match cfg.rotation_pattern.as_str() {
            "reverse" if day_ord % 2 == 1 => n - 1 - hi,
            "shift" => (hi + n - (day_ord * cfg.shift_hours.max(1) as usize) % n) % n,
            _ => hi,
        };
        order.push((pass, rank, hi, ii));
    } }
    order.sort();
    for (_, _, hi, ii) in &order {
        if inp.cancel.map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(false) { break; }
        if hours[*hi].items[*ii].origin != "pending" { continue; }
        place(ctx, &mut hours[*hi], *ii, &day, inp.seed, &mut stats);
    }
    // varianty pro obtížné hodiny
    for hi in 0..hours.len() {
        let unfilled = |h: &PlanHour| h.items.iter().filter(|i| i.origin == "unfilled").count();
        if unfilled(&hours[hi]) == 0 { continue; }
        let auto_idx: Vec<usize> = hours[hi].items.iter().enumerate().filter(|(_, i)| matches!(i.origin.as_str(), "auto" | "fallback" | "unfilled" | "twofer")).map(|(i, _)| i).collect();
        let score_of = |h: &PlanHour| (unfilled(h), -(h.items.iter().filter_map(|i| i.score).sum::<f64>() * 1000.0) as i64);
        let mut best = hours[hi].clone();
        let mut best_key = score_of(&best);
        for v in 1..=cfg.variants {
            for &i in &auto_idx { unplace(ctx, &mut hours[hi], i); hours[hi].items[i].origin = "pending".into(); }
            let mut ord = auto_idx.clone();
            ord.sort_by_key(|i| mix(inp.seed + v as u64, *i as u64, hi as u64));
            let mut st2 = GenStats::default();
            for &i in &ord { place(ctx, &mut hours[hi], i, &day, inp.seed.wrapping_add(v as u64 * 7919), &mut st2); }
            stats.variants_tried += 1;
            let k = score_of(&hours[hi]);
            if k < best_key { best_key = k; best = hours[hi].clone(); }
            if best_key.0 == 0 { break; }
        }
        // obnovit nejlepší variantu v evidenci
        for &i in &auto_idx { unplace(ctx, &mut hours[hi], i); }
        hours[hi] = best;
        for &i in &auto_idx {
            let h = &hours[hi];
            if let Some(tr) = h.items[i].track_id.and_then(|id| ctx.tracks.get(&id).copied()) {
                let at = h.items[i].est_start.map(|t| t.timestamp_millis()).unwrap_or(0);
                if is_music(&h.items[i].slot) { let m = ctx.mult(tr); let hk = format!("{}", at / 3_600_000); ctx.ledger.add(tr, at, h.spec.date, h.spec.hour, &ctx.goals.clone(), &hk, m); }
                else { ins(ctx.ledger.by_track.entry(tr.id).or_default(), at); }
            }
        }
    }
    if cfg.linkers.iter().any(|l| l.enabled) {
        for h in hours.iter_mut() { retime(ctx, h); apply_linkers(ctx, h, inp.seed); }
    }
    let mut scores = vec![];
    for h in hours.iter_mut() {
        retime(ctx, h);
        for it in &h.items {
            if !(is_music(&it.slot) || it.slot.kind == "imaging") { continue; }
            stats.slots += 1;
            match it.origin.as_str() { "unfilled" => stats.unfilled += 1, "fallback" => { stats.fallback += 1; stats.filled += 1 } _ => stats.filled += 1 }
            if let Some(s) = it.score { scores.push(s); if s < 0.45 { stats.compromises += 1; } }
        }
        if h.over_ms > 120_000 { stats.hours_over += 1; } else if h.over_ms < -300_000 { stats.hours_under += 1; }
    }
    stats.avg_score = if scores.is_empty() { 0.0 } else { (scores.iter().sum::<f64>() / scores.len() as f64 * 1000.0).round() / 1000.0 };
    stats.hints = hints(&stats, cfg);
    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    DayPlan { day, hours, stats, seed: inp.seed }
}

/// Linkery: cílené vložení imagingu podle vlastností skladby (před / za skladbu / přes intro). Starší linker položky hodiny se nahradí.
pub fn apply_linkers(ctx: &mut Ctx, h: &mut PlanHour, seed: u64) {
    let linkers: Vec<super::Linker> = ctx.cfg.linkers.iter().filter(|l| l.enabled).cloned().collect();
    h.items.retain(|i| i.origin != "linker");
    if linkers.is_empty() { return; }
    let mut count = vec![0u32; linkers.len()];
    let old = std::mem::take(&mut h.items);
    let mut out: Vec<PlanItem> = vec![];
    let make = |ctx: &mut Ctx, l: &super::Linker, at: i64, max_len: Option<i64>| -> Option<PlanItem> {
        let pool: Vec<&SelTrack> = if l.category.is_empty() { ctx.pools.iter().filter(|((k, _), _)| *k == l.track_kind).flat_map(|(_, v)| v.iter().copied()).collect() }
            else { ctx.pools.get(&(l.track_kind, l.category.clone())).cloned().unwrap_or_default() };
        let gap = l.min_gap_min as i64 * MIN;
        let best = pool.into_iter().filter(|t| t.enabled && !t.attrs.paused && max_len.map(|m| t.len_ms <= m).unwrap_or(true))
            .filter_map(|t| { let d = nearest(ctx.ledger.by_track.get(&t.id), at); if d.map(|x| x < gap).unwrap_or(false) { None } else { Some((d.unwrap_or(i64::MAX), mix(seed, t.id as u64, at as u64) % 1000, t)) } })
            .max_by_key(|x| (x.0, x.1)).map(|x| x.2)?;
        ins(ctx.ledger.by_track.entry(best.id).or_default(), at);
        Some(PlanItem { slot: Slot { kind: "imaging".into(), category: l.category.clone(), track_kind: Some(l.track_kind), label: l.name.clone(), position: if l.position == "over_intro" { Some("start".into()) } else { None }, ..Default::default() },
            track_id: Some(best.id), origin: "linker".into(), note: format!("linker „{}“", l.name), est_start: None, length_ms: best.len_ms, ..Default::default() })
    };
    // dvojice (twofer) a sady se jinglem nerozdělují
    let glued = |it: &PlanItem| it.origin == "twofer" || it.origin == "set";
    let firsts: Vec<bool> = old.iter().enumerate().map(|(i, it)| it.origin == "set" && (i == 0 || old[i - 1].origin != "set")).collect();
    let next_glued: Vec<bool> = (0..old.len()).map(|i| old.get(i + 1).map(|n| n.origin == "twofer" || (n.origin == "set" && !firsts[i + 1])).unwrap_or(false)).collect();
    for (idx, it) in old.into_iter().enumerate() {
        let no_before = glued(&it) && !firsts[idx];
        let no_after = next_glued[idx];
        let tr = if is_music(&it.slot) { it.track_id.and_then(|id| ctx.tracks.get(&id).copied()) } else { None };
        let at = it.est_start.map(|t| t.timestamp_millis()).unwrap_or(0);
        let fits = |l: &super::Linker, tr: &SelTrack| l.when.iter().all(|(k, req)| matches(tr.attr(k).as_deref(), req) == Some(true));
        let mut after: Option<PlanItem> = None;
        if let Some(tr) = tr {
            for (li, l) in linkers.iter().enumerate() {
                if count[li] >= l.max_per_hour || !fits(l, tr) { continue; }
                if (l.position == "after" && no_after) || (l.position != "after" && no_before) { continue; }
                let prev_img = out.last().map(|p| p.slot.kind == "imaging").unwrap_or(false);
                let new = match l.position.as_str() {
                    "after" => if after.is_none() { make(ctx, l, at + tr.len_ms, None).map(|x| { after = Some(x); }) } else { None },
                    "over_intro" => if !prev_img && tr.intro_ms >= l.min_intro_ms { make(ctx, l, at, Some(tr.intro_ms)).map(|x| out.push(x)) } else { None },
                    _ => if !prev_img { make(ctx, l, at, None).map(|x| out.push(x)) } else { None },
                };
                if new.is_some() { count[li] += 1; break; }
            }
        }
        out.push(it);
        if let Some(a) = after { out.push(a); }
    }
    for (i, it) in out.iter_mut().enumerate() { it.pos = i; }
    h.items = out;
}

fn hints(st: &GenStats, cfg: &SelectorConfig) -> Vec<String> {
    let mut v = vec![];
    if st.unfilled == 0 { return v; }
    let mut top: Vec<_> = st.rejected.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    if let Some((k, _)) = top.first() {
        let tip = match k.as_str() {
            R_SONG => format!("Nejvíc pomůže kratší odstup skladby (teď {} min) nebo víc skladeb v úzkých kategoriích.", cfg.rules.song_sep_min),
            R_ARTIST => format!("Nejvíc pomůže kratší odstup interpreta (teď {} min) nebo méně skladeb stejného interpreta v kategorii.", cfg.rules.artist_sep_min),
            R_WORK => "Nejvíc pomůže vypnout společnou ochranu verzí nebo zkrátit její odstup.".into(),
            R_SAME_HOUR => "Ochrana stejné hodiny je tvrdá – přepněte ji na měkkou nebo zkraťte počet dní.".into(),
            R_UNKNOWN => "Skladby nemají vyplněné atributy, které pozice vyžaduje – doplňte je v Atributech skladeb.".into(),
            R_ATTR => "Požadavek atributu pozice splňuje málo skladeb – zmírněte ho nebo doplňte repertoár.".into(),
            R_TRANSITION => "Zakázané přechody vylučují mnoho kandidátů – změňte některé na „nežádoucí“.".into(),
            _ => format!("Nejčastější důvod: {k}."),
        };
        v.push(tip);
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Projekce kapacity

#[derive(Debug, Clone, Serialize)]
pub struct CategoryCapacity {
    pub category: String,
    pub role: String,
    pub tracks: usize,
    pub eligible: usize,
    pub slots_week: usize,
    pub per_hour: f64,
    /// Průměrný obrat (h) = způsobilé tituly / pozic za hodinu.
    pub turnover_h: f64,
    pub spins_per_track_week: f64,
    pub feasible: bool,
    pub message: String,
    pub overloaded_artists: Vec<(String, usize)>,
    pub unknown_attrs: usize,
}

pub fn capacity(cfg: &SelectorConfig, tracks: &[SelTrack], day: &str) -> Vec<CategoryCapacity> {
    let ctx = Ctx::new(cfg, tracks);
    let mut cats: Vec<String> = cfg.categories.iter().filter(|c| c.kind == TrackKind::Song || cfg.music_kinds.contains(&c.kind)).map(|c| c.name.clone()).collect();
    for c in ctx.cat_rate.keys() { if !cats.contains(c) { cats.push(c.clone()); } }
    let req: HashMap<String, Vec<String>> = cfg.clocks.iter().flat_map(|c| c.slots.iter()).filter(|s| s.kind == "music").map(|s| (s.category.clone(), s.attrs.keys().cloned().collect::<Vec<_>>())).collect();
    cats.into_iter().map(|c| {
        let kind = cfg.category(&c).map(|x| x.kind).unwrap_or(TrackKind::Song);
        let pool = ctx.pools.get(&(kind, c.clone())).cloned().unwrap_or_default();
        let eligible = pool.iter().filter(|t| t.active_on(day)).count();
        let per_hour = ctx.cat_rate.get(&c).map(|r| r.0).unwrap_or(0.0);
        let slots_week = (per_hour * 168.0).round() as usize;
        let turnover_h = if per_hour > 0.0 { eligible as f64 / per_hour } else { 0.0 };
        let sep_h = cfg.rules.song_sep_min as f64 / 60.0;
        let feasible = per_hour == 0.0 || turnover_h >= sep_h;
        let mut msg = if per_hour == 0.0 { "Kategorie není v žádném schématu.".to_string() }
            else if eligible == 0 { "Žádné způsobilé skladby – pozice zůstanou neobsazené.".into() }
            else if !feasible { format!("Nesplnitelné: {eligible} titulů a {per_hour:.1} pozic za hodinu dává obrat {turnover_h:.1} h, ale odstup skladby je {sep_h:.1} h. Přidejte aspoň {} titulů, zkraťte odstup nebo uberte pozice.", ((sep_h * per_hour).ceil() as usize).saturating_sub(eligible)) }
            else { format!("Obrat {turnover_h:.1} h, každá skladba asi {:.1}× týdně.", if eligible > 0 { slots_week as f64 / eligible as f64 } else { 0.0 }) };
        // přetížení interpretů: interpret by musel hrát častěji, než dovolí odstup
        let mut by_artist: HashMap<String, usize> = HashMap::new();
        for t in pool.iter().filter(|t| t.active_on(day)) { for a in &t.artists { *by_artist.entry(a.clone()).or_default() += 1; } }
        let art_h = cfg.rules.artist_sep_min as f64 / 60.0;
        let mut over: Vec<(String, usize)> = by_artist.into_iter().filter(|(_, n)| per_hour > 0.0 && eligible > 0 && (*n as f64 / eligible as f64) * per_hour * art_h > 1.0).collect();
        over.sort_by(|a, b| b.1.cmp(&a.1));
        over.truncate(8);
        if !over.is_empty() && feasible { msg.push_str(&format!(" Pozor na interprety s mnoha tituly ({}).", over.iter().take(3).map(|(a, n)| format!("{a} {n}×")).collect::<Vec<_>>().join(", "))); }
        if feasible && turnover_h >= 1.0 && cfg.rotation_pattern == "standard" {
            let r = turnover_h.round();
            if (turnover_h - r).abs() < 0.3 && 24.0 % r == 0.0 { msg.push_str(&format!(" Obrat {r:.0} h dělí 24 h – skladby se budou vracet do stejných hodin; zvažte posunutou nebo reverzní rotaci.")); }
        }
        let unknown = req.get(&c).map(|keys| pool.iter().filter(|t| keys.iter().any(|k| t.attr(k).is_none())).count()).unwrap_or(0);
        CategoryCapacity { role: cfg.category(&c).map(|x| x.role.clone()).unwrap_or_default(), category: c, tracks: pool.len(), eligible, slots_week, per_hour: (per_hour * 100.0).round() / 100.0,
            turnover_h: (turnover_h * 10.0).round() / 10.0, spins_per_track_week: if eligible > 0 { (slots_week as f64 / eligible as f64 * 10.0).round() / 10.0 } else { 0.0 }, feasible, message: msg, overloaded_artists: over, unknown_attrs: unknown }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::attrs::{MusicAttrs, SelTrack};
    use crate::selector::{SelCategory, SelClock};

    fn tr(id: i64, cat: &str, artist: &str) -> SelTrack {
        SelTrack { id, kind: TrackKind::Song, category: cat.into(), artist: artist.into(), title: format!("Song {id}"), album: String::new(), artists: vec![crate::import::norm(artist)],
            work: format!("w{id}"), year: Some(2020), genre: String::new(), len_ms: 200_000, intro_ms: 10_000, enabled: true, attrs: MusicAttrs::new(id) }
    }

    fn cfg_a(per_hour: usize, sep_min: u32) -> SelectorConfig {
        let mut c = SelectorConfig::default();
        c.categories = vec![SelCategory { name: "A".into(), pass: 1, ..Default::default() }];
        c.clocks = vec![SelClock { name: "H".into(), slots: (0..per_hour).map(|_| Slot { category: "A".into(), ..Default::default() }).collect(), ..Default::default() }];
        c.default_clock = "H".into();
        c.rules.song_sep_min = sep_min; c.rules.artist_sep_min = 0; c.rules.work_sep_min = 0; c.rules.same_hour_days = 0;
        c
    }

    #[test]
    fn calibration_12_titles_3_per_hour() {
        // 3 A pozice/h × 24 h = 72 pozic; 12 titulů a tvrdý 6h odstup → nelze splnit všechny pozice
        let tracks: Vec<SelTrack> = (1..=12).map(|i| tr(i, "A", &format!("Artist {i}"))).collect();
        let cfg = cfg_a(3, 360);
        let cap = capacity(&cfg, &tracks, "2026-11-23");
        assert_eq!(cap[0].turnover_h, 4.0);
        assert!(!cap[0].feasible);
        let mut ctx = Ctx::new(&cfg, &tracks);
        let date = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let plan = generate_day(&mut ctx, DayInput { date, hours: day_hours(&Utc, date), keep: HashMap::new(), seed: 1, cancel: None });
        assert_eq!(plan.stats.slots, 72);
        assert!(plan.stats.unfilled > 0, "nesmí vykázat splnění všech pozic");
        assert!(plan.stats.rejected.contains_key(R_SONG));
        // s 4h odstupem je to proveditelné
        let cfg2 = cfg_a(3, 180);
        let mut ctx2 = Ctx::new(&cfg2, &tracks);
        let p2 = generate_day(&mut ctx2, DayInput { date, hours: day_hours(&Utc, date), keep: HashMap::new(), seed: 1, cancel: None });
        assert_eq!(p2.stats.unfilled, 0);
        // každá skladba hraje 6× za den (72 / 12) – rovnoměrná rotace
        let mut cnt: HashMap<i64, usize> = HashMap::new();
        for h in &p2.hours { for i in &h.items { *cnt.entry(i.track_id.unwrap()).or_default() += 1; } }
        assert!(cnt.values().all(|c| *c == 6), "{cnt:?}");
    }

    #[test]
    fn deterministic_and_artist_separation() {
        let mut tracks: Vec<SelTrack> = (1..=40).map(|i| tr(i, "A", &format!("Artist {}", i % 10))).collect();
        tracks.push(SelTrack { artists: vec!["artist1".into(), "guest".into()], ..tr(99, "A", "Artist 1 feat. Guest") });
        let mut cfg = cfg_a(4, 120);
        cfg.rules.artist_sep_min = 60;
        let date = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let run = || { let mut ctx = Ctx::new(&cfg, &tracks); generate_day(&mut ctx, DayInput { date, hours: day_hours(&Utc, date), keep: HashMap::new(), seed: 42, cancel: None }) };
        let a = run(); let b = run();
        let ids = |p: &DayPlan| p.hours.iter().flat_map(|h| h.items.iter().map(|i| i.track_id)).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b), "stejný seed a vstupy = stejný plán");
        // žádný interpret dvakrát během 60 min (podle odhadovaných časů)
        let mut seen: Vec<(i64, Vec<String>)> = vec![];
        for h in &a.hours { for i in &h.items { if let Some(id) = i.track_id { let t = tracks.iter().find(|t| t.id == id).unwrap(); seen.push((i.est_start.unwrap().timestamp_millis(), t.artists.clone())); } } }
        for (x, (t1, a1)) in seen.iter().enumerate() { for (t2, a2) in &seen[x + 1..] { if (t2 - t1).abs() < 60 * MIN { assert!(!a1.iter().any(|a| a2.contains(a)), "interpret se opakuje"); } } }
    }

    #[test]
    fn dst_hours() {
        use chrono_tz::Europe::Prague;
        let fall = day_hours(&Prague, NaiveDate::from_ymd_opt(2026, 10, 25).unwrap());
        assert_eq!(fall.len(), 25);
        assert_eq!(fall.iter().filter(|(_, h, _)| *h == 2).count(), 2);
        assert!(fall.iter().any(|(_, _, l)| l == "02:00 (2.)"));
        let spring = day_hours(&Prague, NaiveDate::from_ymd_opt(2026, 3, 29).unwrap());
        assert_eq!(spring.len(), 23);
        assert!(!spring.iter().any(|(_, h, _)| *h == 2));
    }

    #[test]
    fn exceptions_priority() {
        let mut c = cfg_a(1, 0);
        c.clocks.push(SelClock { name: "X".into(), ..Default::default() });
        c.clocks.push(SelClock { name: "Y".into(), ..Default::default() });
        c.grid.insert("1-8".into(), "H".into());
        c.exceptions = vec![
            crate::selector::GridException { name: "Vánoce".into(), date_from: "2026-12-24".into(), date_to: "2026-12-26".into(), from_hour: 0, to_hour: 24, clock: "X".into(), priority: 1 },
            crate::selector::GridException { name: "Koncert".into(), date_from: "2026-12-25".into(), date_to: "".into(), from_hour: 20, to_hour: 22, clock: "Y".into(), priority: 5 },
        ];
        let d = NaiveDate::from_ymd_opt(2026, 12, 25).unwrap();
        assert_eq!(c.clock_for(d, 5, 10).unwrap().0.name, "X");
        assert_eq!(c.clock_for(d, 5, 21).unwrap().0.name, "Y");
        assert_eq!(c.clock_for(d, 5, 22).unwrap().0.name, "X"); // [od, do)
    }

    #[test]
    fn perf_week_3000_tracks() {
        // reprezentativní velikost: 3000 skladeb, 4 kategorie, 12 hudebních pozic/h, 7 dní (2016 pozic)
        let cats = ["P", "C", "R", "G"];
        let tracks: Vec<SelTrack> = (1..=3000).map(|i| { let mut t = tr(i, cats[(i % 4) as usize], &format!("Artist {}", i % 700)); t.attrs.energy = Some(i % 5 + 1); t }).collect();
        let mut cfg = cfg_a(0, 180);
        cfg.categories = cats.iter().enumerate().map(|(i, c)| SelCategory { name: c.to_string(), pass: i as u32, ..Default::default() }).collect();
        cfg.clocks[0].slots = (0..12).map(|i| Slot { category: cats[i % 4].into(), ..Default::default() }).collect();
        cfg.rules.artist_sep_min = 60;
        cfg.rules.same_hour_days = 3;
        cfg.rules.max_in_row.insert("energy".into(), 3);
        let mut ctx = Ctx::new(&cfg, &tracks);
        let t0 = std::time::Instant::now();
        let mut unfilled = 0;
        for d in 0..7 {
            let date = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap() + Duration::days(d);
            unfilled += generate_day(&mut ctx, DayInput { date, hours: day_hours(&Utc, date), keep: HashMap::new(), seed: 1, cancel: None }).stats.unfilled;
        }
        let el = t0.elapsed();
        eprintln!("PERF: 3000 skladeb, 7 dní, 2016 pozic: {:?}, neobsazeno {}", el, unfilled);
        assert_eq!(unfilled, 0);
        assert!(el.as_secs() < 120);
    }

    fn run(cfg: &SelectorConfig, tracks: &[SelTrack], days: i64) -> Vec<DayPlan> {
        let mut ctx = Ctx::new(cfg, tracks);
        (0..days).map(|d| { let date = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap() + Duration::days(d); generate_day(&mut ctx, DayInput { date, hours: day_hours(&Utc, date), keep: HashMap::new(), seed: 7, cancel: None }) }).collect()
    }

    #[test]
    fn twofer_sets_themes() {
        let mut tracks: Vec<SelTrack> = (1..=40).map(|i| tr(i, "A", &format!("Artist {}", i % 8))).collect();
        tracks.push(tr(500, "S", "Orchestr")); tracks.push(tr(501, "S", "Orchestr")); tracks.push(tr(502, "S", "Orchestr"));
        let mut cfg = cfg_a(4, 60);
        cfg.rules.artist_sep_min = 90;
        cfg.clocks[0].slots[0].twofer = true;
        cfg.sets = vec![crate::selector::TrackSet { name: "Symfonie".into(), track_ids: vec![502, 500, 501], keep_order: true, note: String::new() }];
        cfg.clocks[0].slots.push(Slot { kind: "set".into(), set: "Symfonie".into(), ..Default::default() });
        for i in 100..120 { let mut t = tr(i, "ID", "Radio"); t.kind = TrackKind::Jingle; t.len_ms = 4000; tracks.push(t); }
        cfg.linkers = vec![crate::selector::Linker { name: "vždy".into(), position: "before".into(), track_kind: TrackKind::Jingle, category: "ID".into(), max_per_hour: 20, min_gap_min: 0, ..Default::default() }];
        let p = &run(&cfg, &tracks, 1)[0];
        let mut h = p.hours[5].clone();
        h.items.retain(|i| i.origin != "linker");
        assert!(p.hours[5].items.windows(2).all(|w| !(w[0].origin == "linker" && (w[1].origin == "twofer" || (w[1].origin == "set" && w[1].track_id != Some(502))))), "jingle nerozdělí dvojici ani sadu");
        assert!(p.hours[5].items.iter().any(|i| i.origin == "linker"));
        let a = h.items[0].track_id.unwrap(); let b = h.items[1].track_id.unwrap();
        let ta = tracks.iter().find(|t| t.id == a).unwrap(); let tb = tracks.iter().find(|t| t.id == b).unwrap();
        assert_eq!(h.items[1].origin, "twofer");
        assert_eq!(ta.artists, tb.artists, "twofer = stejný interpret i přes 90min odstup");
        let set: Vec<i64> = h.items.iter().filter(|i| i.origin == "set").map(|i| i.track_id.unwrap()).collect();
        assert_eq!(set, vec![502, 500, 501], "sada v zadaném pořadí");
    }

    #[test]
    fn shifted_rotation_breaks_same_hour_lock() {
        // 12 titulů, 3 pozice/h → obrat 4 h dělí 24 h: standardní rotace vrací skladby do stejných hodin
        let tracks: Vec<SelTrack> = (1..=12).map(|i| tr(i, "A", &format!("Artist {i}"))).collect();
        let same_hour = |cfg: &SelectorConfig| {
            let plans = run(cfg, &tracks, 6);
            let mut seen: std::collections::HashMap<(i64, u32), usize> = Default::default();
            for p in &plans { for h in &p.hours { for i in &h.items { if let Some(t) = i.track_id { *seen.entry((t, h.spec.hour)).or_default() += 1; } } } }
            seen.values().filter(|c| **c > 1).map(|c| c - 1).sum::<usize>()
        };
        let std_cfg = cfg_a(3, 180);
        let mut shift_cfg = cfg_a(3, 180);
        shift_cfg.rotation_pattern = "shift".into();
        shift_cfg.shift_hours = 5;
        let (a, b) = (same_hour(&std_cfg), same_hour(&shift_cfg));
        eprintln!("stejná hodina: standard {a}, posunutá {b}");
        assert!(b < a, "posunutá rotace musí snížit návraty do stejných hodin ({b} vs {a})");
        let cap = capacity(&std_cfg, &tracks, "2026-11-23");
        assert!(cap[0].message.contains("stejných hodin"));
    }

    #[test]
    fn linkers_and_daypart_alt() {
        let mut tracks: Vec<SelTrack> = (1..=30).map(|i| { let mut t = tr(i, "A", &format!("Artist {i}")); t.attrs.language = if i % 2 == 0 { "cs".into() } else { "en".into() }; t }).collect();
        for i in 100..106 { let mut t = tr(i, "ID CZ", "Radio"); t.kind = TrackKind::Jingle; t.len_ms = 5000; tracks.push(t); }
        for i in 200..230 { tracks.push(tr(i, "A night", &format!("Night {i}"))); }
        let mut cfg = cfg_a(4, 60);
        cfg.linkers = vec![crate::selector::Linker { name: "CZ jingle".into(), when: [("language".to_string(), "cs".to_string())].into(), position: "before".into(), track_kind: TrackKind::Jingle, category: "ID CZ".into(), max_per_hour: 1, min_gap_min: 30, ..Default::default() }];
        cfg.categories[0].daypart_alt.insert("Noc".into(), "A night".into());
        let p = &run(&cfg, &tracks, 1)[0];
        let day = &p.hours[12];
        let li: Vec<usize> = day.items.iter().enumerate().filter(|(_, i)| i.origin == "linker").map(|(k, _)| k).collect();
        assert_eq!(li.len(), 1, "max 1 za hodinu");
        let next = day.items[li[0] + 1].track_id.unwrap();
        assert_eq!(tracks.iter().find(|t| t.id == next).unwrap().attrs.language, "cs", "jingle před českou skladbou");
        assert!(p.hours[2].items.iter().all(|i| i.track_id.map(|t| t >= 200).unwrap_or(true)), "v noci alternativní kategorie");
    }
}
