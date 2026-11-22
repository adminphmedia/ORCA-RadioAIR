//! Analytika vysílání: as-played (skutečnost), plán a srovnání. Stejné výpočty pro rozhraní, CSV export i simulaci.
//!
//! Definice (verze metrik `METRICS_VERSION`):
//! * **start** = každé spuštění položky na hlavním výstupu (instanty, podkresy a předposlech se nepočítají),
//! * **kvalifikovaný spin** = start, který hrál aspoň `qualify.min_ms` nebo `qualify.min_pct` své délky,
//! * opakovaný start téže položky fronty (restart) je jeden záznam – zápis je idempotentní,
//! * minuty = skutečně odehraná doba (u nepotvrzeného konce odhad z dalšího startu, nejvýše délka),
//!   rozdělená mezi dny podle místního času stanice,
//! * podíly vždy uvádí jmenovatel; neznámá hodnota atributu je samostatná skupina (≠ 0),
//! * týden začíná dnem `week_start` (1 = pondělí) v místním čase.

use super::attrs::{artist_ids, MusicAttrs};
use super::generator::DayPlan;
use super::SelectorConfig;
use crate::db::Db;
use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const METRICS_VERSION: &str = "orca-metrics-1";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Spin {
    pub id: i64,
    pub track_id: Option<i64>,
    pub kind: String,
    pub artist: String,
    pub title: String,
    pub category: String,
    pub started: DateTime<Utc>,
    pub played_ms: i64,
    pub duration_ms: i64,
    pub source: String,
    pub channel: String,
    pub plan_item: Option<i64>,
    pub attrs: BTreeMap<String, String>,
    /// ended | cut | skipped | unconfirmed | planned
    pub status: String,
    pub qualified: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Filter {
    /// YYYY-MM-DD (místní), včetně
    pub from: String,
    /// YYYY-MM-DD (místní), včetně
    pub to: String,
    pub kind: Option<String>,
    pub category: Option<String>,
    pub daypart: Option<String>,
    pub source: Option<String>,
    /// actual | plan
    pub basis: Option<String>,
}

pub fn local_day_start(d: NaiveDate) -> DateTime<Utc> {
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or_else(|| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

fn utc_s(t: DateTime<Utc>) -> String { t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string() }

/// Načte as-played události hlavního výstupu v místním intervalu [from, to] (dny včetně).
pub fn load_spins(db: &Db, cfg: &SelectorConfig, from: NaiveDate, to: NaiveDate) -> Result<Vec<Spin>> {
    let (a, b) = (local_day_start(from), local_day_start(to.succ_opt().unwrap()));
    let rows: Vec<Spin> = db.with_conn(|c| -> Result<Vec<Spin>> {
        let mut st = c.prepare("SELECT id, track_id, kind, artist, title, category, started_at, ended_at, played_ms, duration_ms, source, channel, plan_item_id, attrs FROM play_log
                                WHERE started_at >= ?1 AND started_at < ?2 AND channel='main' ORDER BY started_at, id")?;
        let v = st.query_map([utc_s(a - Duration::hours(3)), utc_s(b)], |r| {
            let started: String = r.get(6)?;
            let ended: Option<String> = r.get(7)?;
            let attrs: String = r.get(13)?;
            Ok((Spin {
                id: r.get(0)?, track_id: r.get(1)?, kind: r.get(2)?, artist: r.get(3)?, title: r.get(4)?, category: r.get(5)?,
                started: DateTime::parse_from_rfc3339(&started).map(|d| d.with_timezone(&Utc)).unwrap_or_default(), played_ms: r.get::<_, Option<i64>>(8)?.unwrap_or(-1),
                duration_ms: r.get(9)?, source: r.get(10)?, channel: r.get(11)?, plan_item: r.get(12)?,
                attrs: serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&attrs).unwrap_or_default().into_iter().map(|(k, v)| (k, v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect(),
                ..Default::default()
            }, ended.is_some()))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out: Vec<Spin> = v.into_iter().map(|(s, _)| s).collect();
        // nepotvrzený konec: odhad z dalšího startu (nejvýše délka)
        for i in 0..out.len() {
            if out[i].played_ms < 0 {
                let next = out.get(i + 1).map(|n| (n.started - out[i].started).num_milliseconds());
                out[i].played_ms = next.map(|n| n.min(out[i].duration_ms.max(0))).unwrap_or(0);
                out[i].status = "unconfirmed".into();
            }
        }
        Ok(out)
    })?;
    Ok(rows.into_iter().map(|mut s| { classify(&mut s, cfg); s }).collect())
}

pub fn classify(s: &mut Spin, cfg: &SelectorConfig) {
    let q = &cfg.qualify;
    s.qualified = s.played_ms >= q.min_ms || (s.duration_ms > 0 && s.played_ms as f64 >= q.min_pct * s.duration_ms as f64);
    if s.status.is_empty() {
        s.status = if s.duration_ms > 0 && s.played_ms >= s.duration_ms - 3_000 { "ended" } else if s.played_ms < 10_000 { "skipped" } else { "cut" }.into();
    }
}

/// Události z plánu (pro srovnání a simulaci).
pub fn plan_spins(plans: &[DayPlan], tracks: &HashMap<i64, super::attrs::SelTrack>) -> Vec<Spin> {
    let mut v = vec![];
    for p in plans { for h in &p.hours { for it in &h.items {
        let (Some(id), Some(t)) = (it.track_id, it.est_start) else { continue };
        let Some(tr) = tracks.get(&id) else { continue };
        let mut attrs = BTreeMap::new();
        for k in ["language", "local", "energy", "bpm", "gender", "mood", "familiarity", "explicit", "sound"] {
            let val = super::attrs::field_value(&tr.attrs, k);
            if !val.is_empty() { attrs.insert(k.to_string(), val); }
        }
        v.push(Spin { id: it.id.unwrap_or(0), track_id: Some(id), kind: tr.kind.as_str().into(), artist: tr.artist.clone(), title: tr.title.clone(), category: tr.category.clone(),
            started: t, played_ms: tr.len_ms, duration_ms: tr.len_ms, source: "plan".into(), channel: "main".into(), plan_item: it.id, attrs, status: "planned".into(), qualified: true });
    } } }
    v.sort_by_key(|s| s.started);
    v
}

/// Doplní atributy, které v historickém snímku chybí, z aktuální knihovny (označeno jako „dohledáno“).
pub fn enrich(spins: &mut [Spin], attrs: &HashMap<i64, MusicAttrs>, years: &HashMap<i64, (Option<i32>, String)>) {
    for s in spins.iter_mut() {
        let Some(id) = s.track_id else { continue };
        if let Some(a) = attrs.get(&id) {
            for k in ["language", "local", "energy", "bpm", "gender", "mood", "familiarity", "explicit", "sound"] {
                if !s.attrs.contains_key(k) { let v = super::attrs::field_value(a, k); if !v.is_empty() { s.attrs.insert(k.into(), v); } }
            }
        }
        if let Some((y, g)) = years.get(&id) {
            if let Some(y) = y { s.attrs.entry("decade".into()).or_insert(format!("{}", y / 10 * 10)); }
            if !g.is_empty() { s.attrs.entry("genre".into()).or_insert(g.to_lowercase()); }
        }
    }
}

pub fn filter(spins: Vec<Spin>, f: &Filter, cfg: &SelectorConfig, from: NaiveDate, to: NaiveDate) -> Vec<Spin> {
    let (a, b) = (local_day_start(from), local_day_start(to.succ_opt().unwrap()));
    spins.into_iter().filter(|s| s.started >= a && s.started < b)
        .filter(|s| f.kind.as_deref().map(|k| k.is_empty() || s.kind == k).unwrap_or(true))
        .filter(|s| f.category.as_deref().map(|c| c.is_empty() || s.category == c).unwrap_or(true))
        .filter(|s| f.source.as_deref().map(|c| c.is_empty() || s.source == c).unwrap_or(true))
        .filter(|s| f.daypart.as_deref().map(|d| d.is_empty() || cfg.daypart_of(s.started.with_timezone(&Local).hour()).map(|x| x.name == d).unwrap_or(false)).unwrap_or(true))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Souhrn stanice

#[derive(Debug, Clone, Serialize, Default)]
pub struct Share { pub key: String, pub count: usize, pub minutes: f64, pub pct_count: f64, pub pct_minutes: f64 }

#[derive(Debug, Clone, Serialize, Default)]
pub struct GoalCheck { pub goal_id: String, pub label: String, pub actual_pct: Option<f64>, pub target_pct: f64, pub tolerance_pct: f64, pub status: String, pub severity: String, pub coverage_pct: f64, pub denominator: String, pub examples: Vec<String>, pub weight: f64, pub score: Option<f64> }

#[derive(Debug, Clone, Serialize, Default)]
pub struct StationReport {
    pub metrics_version: String,
    pub basis: String,
    pub from: String,
    pub to: String,
    pub timezone: String,
    pub qualify: String,
    pub starts: usize,
    pub music_starts: usize,
    pub music_spins: usize,
    pub unique_tracks: usize,
    pub unique_artists: usize,
    pub music_minutes: f64,
    pub total_minutes: f64,
    pub expected_minutes: f64,
    pub coverage_pct: f64,
    pub gaps: Vec<(String, String, i64)>,
    pub kinds: Vec<Share>,
    pub categories: Vec<Share>,
    pub top_tracks: Vec<(i64, String, String, usize)>,
    pub top_artists: Vec<(String, usize, f64)>,
    pub top10_concentration_pct: f64,
    pub unique_per_spin: f64,
    pub jaccard: Vec<(String, f64)>,
    pub jaccard_avg: Option<f64>,
    pub same_hour: Vec<String>,
    pub repeated_pairs: Vec<(String, usize)>,
    pub longest_runs: Vec<(String, String, usize)>,
    pub attrs: BTreeMap<String, Vec<Share>>,
    pub heatmap: Vec<Vec<usize>>,
    pub hourly: Vec<(u32, usize, Option<f64>, Option<f64>)>,
    pub by_day: Vec<(String, usize, usize, f64)>,
    pub goals: Vec<GoalCheck>,
    pub format_score: Option<f64>,
    pub unevaluable_pct: f64,
    pub plan_vs_actual: Option<PlanVsActual>,
    pub statuses: BTreeMap<String, usize>,
    pub zero_spins: Vec<(i64, String, String, String)>,
    pub alerts: Vec<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PlanVsActual { pub planned: usize, pub aired_as_planned: usize, pub replaced: usize, pub dropped: usize, pub unplanned_music: usize, pub match_pct: f64 }

fn minutes_in(s: &Spin, a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    let st = s.started;
    let en = st + Duration::milliseconds(s.played_ms.max(0));
    let lo = st.max(a); let hi = en.min(b);
    if hi > lo { (hi - lo).num_milliseconds() as f64 / 60_000.0 } else { 0.0 }
}

fn r1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }

pub fn is_music(s: &Spin, cfg: &SelectorConfig) -> bool { cfg.music_kinds.iter().any(|k| k.as_str() == s.kind) }

pub fn artist_keys(s: &Spin) -> Vec<String> { artist_ids(&s.artist, &s.title) }

fn shares(map: BTreeMap<String, (usize, f64)>, total_c: usize, total_m: f64) -> Vec<Share> {
    let mut v: Vec<Share> = map.into_iter().map(|(k, (c, m))| Share { key: k, count: c, minutes: r1(m), pct_count: if total_c > 0 { r1(c as f64 * 100.0 / total_c as f64) } else { 0.0 }, pct_minutes: if total_m > 0.0 { r1(m * 100.0 / total_m) } else { 0.0 } }).collect();
    v.sort_by(|a, b| b.count.cmp(&a.count));
    v
}

pub fn station_report(cfg: &SelectorConfig, spins: &[Spin], from: NaiveDate, to: NaiveDate, basis: &str, pva: Option<PlanVsActual>) -> StationReport {
    let t0 = std::time::Instant::now();
    let (a, b) = (local_day_start(from), local_day_start(to.succ_opt().unwrap()));
    let now = Utc::now();
    let mut r = StationReport { metrics_version: METRICS_VERSION.into(), basis: basis.into(), from: from.to_string(), to: to.to_string(), timezone: Local::now().format("%:z").to_string(),
        qualify: format!("spin = hrálo aspoň {} s nebo {:.0} % délky", cfg.qualify.min_ms / 1000, cfg.qualify.min_pct * 100.0), ..Default::default() };
    r.starts = spins.len();
    let music: Vec<&Spin> = spins.iter().filter(|s| is_music(s, cfg)).collect();
    let qual: Vec<&Spin> = music.iter().copied().filter(|s| s.qualified).collect();
    r.music_starts = music.len();
    r.music_spins = qual.len();
    for s in spins { *r.statuses.entry(s.status.clone()).or_default() += 1; }
    r.total_minutes = r1(spins.iter().map(|s| minutes_in(s, a, b)).sum());
    r.music_minutes = r1(qual.iter().map(|s| minutes_in(s, a, b)).sum());
    let end = if basis == "plan" { b } else { b.min(now) };
    r.expected_minutes = r1(((end - a).num_milliseconds().max(0)) as f64 / 60_000.0);
    r.coverage_pct = if r.expected_minutes > 0.0 { r1((r.total_minutes / r.expected_minutes * 100.0).min(100.0)) } else { 0.0 };
    // mezery v datech > 10 min
    let mut last_end = a;
    for s in spins {
        if s.started > last_end + Duration::minutes(10) { r.gaps.push((s.started.with_timezone(&Local).format("%d.%m. %H:%M").to_string(), last_end.with_timezone(&Local).format("%d.%m. %H:%M").to_string(), (s.started - last_end).num_minutes())); }
        last_end = last_end.max(s.started + Duration::milliseconds(s.played_ms.max(0)));
    }
    if end > last_end + Duration::minutes(10) { r.gaps.push((end.with_timezone(&Local).format("%d.%m. %H:%M").to_string(), last_end.with_timezone(&Local).format("%d.%m. %H:%M").to_string(), (end - last_end).num_minutes())); }
    r.gaps.sort_by(|x, y| y.2.cmp(&x.2));
    r.gaps.truncate(10);
    // typy a kategorie (jmenovatel: všechny starty / hudební spiny)
    let mut k: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for s in spins { let e = k.entry(s.kind.clone()).or_default(); e.0 += 1; e.1 += minutes_in(s, a, b); }
    r.kinds = shares(k, r.starts, r.total_minutes);
    let mut c: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for s in &qual { let e = c.entry(if s.category.is_empty() { "(bez kategorie)".into() } else { s.category.clone() }).or_default(); e.0 += 1; e.1 += minutes_in(s, a, b); }
    r.categories = shares(c, r.music_spins, r.music_minutes);
    // opakování
    let mut tc: HashMap<(Option<i64>, String, String), usize> = HashMap::new();
    for s in &qual { *tc.entry((s.track_id, s.artist.clone(), s.title.clone())).or_default() += 1; }
    r.unique_tracks = tc.len();
    let mut tv: Vec<_> = tc.into_iter().collect();
    tv.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.1.cmp(&y.0.1)));
    r.top10_concentration_pct = if r.music_spins > 0 { r1(tv.iter().take(10).map(|x| x.1).sum::<usize>() as f64 * 100.0 / r.music_spins as f64) } else { 0.0 };
    r.top_tracks = tv.iter().take(10).map(|((id, a, t), n)| (id.unwrap_or(0), a.clone(), t.clone(), *n)).collect();
    r.unique_per_spin = if r.music_spins > 0 { (r.unique_tracks as f64 / r.music_spins as f64 * 1000.0).round() / 1000.0 } else { 0.0 };
    let mut ac: HashMap<String, (usize, String)> = HashMap::new();
    for s in &qual { for (i, k) in artist_keys(s).into_iter().enumerate() { let e = ac.entry(k).or_insert((0, String::new())); e.0 += 1; if e.1.is_empty() && i == 0 { e.1 = s.artist.clone(); } } }
    r.unique_artists = ac.len();
    let mut av: Vec<_> = ac.into_iter().map(|(k, (n, name))| (if name.is_empty() { k } else { name }, n)).collect();
    av.sort_by(|x, y| y.1.cmp(&x.1));
    r.top_artists = av.into_iter().take(10).map(|(n, c)| (n, c, if r.music_spins > 0 { r1(c as f64 * 100.0 / r.music_spins as f64) } else { 0.0 })).collect();
    // po dnech + Jaccard sousedních dnů
    let mut days: BTreeMap<NaiveDate, (usize, HashSet<i64>, f64)> = BTreeMap::new();
    let mut d = from;
    while d <= to { days.insert(d, (0, HashSet::new(), 0.0)); d = d.succ_opt().unwrap(); }
    for s in &qual {
        let ld = s.started.with_timezone(&Local).date_naive();
        if let Some(e) = days.get_mut(&ld) { e.0 += 1; if let Some(id) = s.track_id { e.1.insert(id); } }
        // minuty rozdělené přes půlnoc
        let mut dd = ld;
        loop {
            let (da, db_) = (local_day_start(dd), local_day_start(dd.succ_opt().unwrap()));
            let m = minutes_in(s, da, db_);
            if m <= 0.0 { break; }
            if let Some(e) = days.get_mut(&dd) { e.2 += m; }
            dd = dd.succ_opt().unwrap();
        }
    }
    let dv: Vec<_> = days.iter().collect();
    for w in dv.windows(2) {
        let (x, y) = (&w[0].1.1, &w[1].1.1);
        let uni = x.union(y).count();
        if uni > 0 { r.jaccard.push((w[1].0.to_string(), (x.intersection(y).count() as f64 / uni as f64 * 1000.0).round() / 1000.0)); }
    }
    r.jaccard_avg = if r.jaccard.is_empty() { None } else { Some((r.jaccard.iter().map(|x| x.1).sum::<f64>() / r.jaccard.len() as f64 * 1000.0).round() / 1000.0) };
    r.by_day = days.iter().map(|(d, (n, u, m))| (d.to_string(), *n, u.len(), r1(*m))).collect();
    // stejná hodina: všední dny, titul ve stejné hodině ve ≥ 3 dnech nebo ve ≥ 50 % dní
    let mut sh: HashMap<(i64, u32), HashSet<NaiveDate>> = HashMap::new();
    let mut names: HashMap<i64, String> = HashMap::new();
    let weekdays: HashSet<NaiveDate> = days.keys().filter(|d| d.weekday().number_from_monday() <= 5).copied().collect();
    for s in &qual {
        let l = s.started.with_timezone(&Local);
        let Some(id) = s.track_id else { continue };
        if !weekdays.contains(&l.date_naive()) { continue; }
        sh.entry((id, l.hour())).or_default().insert(l.date_naive());
        names.insert(id, format!("{} – {}", s.artist, s.title));
    }
    let mut shv: Vec<_> = sh.into_iter().filter(|(_, ds)| ds.len() >= 3 || (weekdays.len() >= 2 && ds.len() * 2 > weekdays.len() && ds.len() >= 2)).collect();
    shv.sort_by(|x, y| y.1.len().cmp(&x.1.len()));
    r.same_hour = shv.iter().take(15).map(|((id, h), ds)| format!("Ve všední dny se „{}“ v {:02}:00–{:02}:00 objevil {}× z {} dnů.", names.get(id).cloned().unwrap_or_default(), h, (h + 1) % 24, ds.len(), weekdays.len())).collect();
    // opakované dvojice za sebou
    let mut pairs: HashMap<(i64, i64), usize> = HashMap::new();
    for w in qual.windows(2) { if let (Some(x), Some(y)) = (w[0].track_id, w[1].track_id) { *pairs.entry((x, y)).or_default() += 1; } }
    let title = |id: i64| qual.iter().find(|s| s.track_id == Some(id)).map(|s| format!("{} – {}", s.artist, s.title)).unwrap_or_default();
    let mut pv: Vec<_> = pairs.into_iter().filter(|x| x.1 > 1).collect();
    pv.sort_by(|x, y| y.1.cmp(&x.1));
    r.repeated_pairs = pv.into_iter().take(10).map(|((x, y), n)| (format!("{} → {}", title(x), title(y)), n)).collect();
    // atributy
    for key in ["language", "local", "decade", "energy", "gender", "genre", "mood", "sound", "explicit"] {
        let mut m: BTreeMap<String, (usize, f64)> = BTreeMap::new();
        for s in &qual { let e = m.entry(s.attrs.get(key).cloned().unwrap_or_else(|| "?".into())).or_default(); e.0 += 1; e.1 += minutes_in(s, a, b); }
        if m.len() > 1 || m.keys().any(|k| k != "?") { r.attrs.insert(key.into(), shares(m, r.music_spins, r.music_minutes)); }
        // nejdelší série stejné hodnoty
        let mut best = (String::new(), 0usize); let mut cur = (String::new(), 0usize);
        for s in &qual {
            let v = s.attrs.get(key).cloned().unwrap_or_default();
            if !v.is_empty() && v == cur.0 { cur.1 += 1; } else { cur = (v, 1); }
            if !cur.0.is_empty() && cur.1 > best.1 { best = cur.clone(); }
        }
        if best.1 >= 3 { r.longest_runs.push((key.into(), best.0, best.1)); }
    }
    // heatmapa den × hodina, hodinový průběh energie a tempa
    r.heatmap = vec![vec![0; 24]; 7];
    let mut hourly: Vec<(usize, f64, usize, f64, usize)> = vec![(0, 0.0, 0, 0.0, 0); 24];
    for s in &qual {
        let l = s.started.with_timezone(&Local);
        r.heatmap[l.weekday().num_days_from_monday() as usize][l.hour() as usize] += 1;
        let h = &mut hourly[l.hour() as usize];
        h.0 += 1;
        if let Some(e) = s.attrs.get("energy").and_then(|x| x.parse::<f64>().ok()) { h.1 += e; h.2 += 1; }
        if let Some(e) = s.attrs.get("bpm").and_then(|x| x.parse::<f64>().ok()) { h.3 += e; h.4 += 1; }
    }
    r.hourly = hourly.into_iter().enumerate().map(|(h, (n, e, en, b, bn))| (h as u32, n, if en > 0 { Some(r1(e / en as f64)) } else { None }, if bn > 0 { Some(r1(b / bn as f64)) } else { None })).collect();
    // soulad s formátem
    let (checks, score, unev) = format_checks(cfg, &qual, a, b);
    r.goals = checks; r.format_score = score; r.unevaluable_pct = unev;
    r.plan_vs_actual = pva;
    // upozornění
    if r.coverage_pct < 90.0 && basis == "actual" && r.expected_minutes > 60.0 { r.alerts.push(format!("Data pokrývají jen {:.0} % očekávaného vysílacího času – metriky jsou neúplné.", r.coverage_pct)); }
    if r.top10_concentration_pct > 35.0 { r.alerts.push(format!("Top 10 skladeb tvoří {:.0} % hudebních spinů.", r.top10_concentration_pct)); }
    if let Some(j) = r.jaccard_avg { if j > 0.6 { r.alerts.push(format!("Sousední dny sdílí v průměru {:.0} % repertoáru (Jaccard {:.2}).", j * 100.0, j)); } }
    r.alerts.extend(r.same_hour.iter().take(3).cloned());
    for g in r.goals.iter().filter(|g| g.status == "fail") { r.alerts.push(format!("Cíl „{}“: {} (cíl {} ± {} %).", g.label, g.actual_pct.map(|x| format!("{x:.1} %")).unwrap_or("?".into()), g.target_pct, g.tolerance_pct)); }
    r.elapsed_ms = t0.elapsed().as_millis() as u64;
    r
}

fn goal_label(g: &super::Goal) -> String {
    let attr = match g.attr.as_str() { "language" => "jazyk", "local" => "lokální tvorba", "decade" => "dekáda", "energy" => "energie", "gender" => "zpěv", "genre" => "žánr", "category" => "kategorie", x => x };
    let scope = match g.scope.as_str() { "hour" => "hodina", "daypart" => "pásmo", "week" => "týden", _ => "den" };
    format!("{attr} = {} ({}, {}{})", g.value, if g.unit == "minutes" { "minuty" } else { "počet" }, scope, if g.daypart.is_empty() { String::new() } else { format!(", {}", g.daypart) })
}

/// Kontrola cílů formátu: skutečnost vs. cíl ± tolerance, pokrytí dat, závažnost, příklady. Skóre = vážený průměr vyhodnotitelných kontrol;
/// odchylka se normalizuje: v toleranci = 100, mimo = 100 − (přesah / tolerance) × 50, min. 0.
pub fn format_checks(cfg: &SelectorConfig, qual: &[&Spin], a: DateTime<Utc>, b: DateTime<Utc>) -> (Vec<GoalCheck>, Option<f64>, f64) {
    let mut out = vec![];
    for g in &cfg.goals {
        let rel: Vec<&&Spin> = qual.iter().filter(|s| g.daypart.is_empty() || cfg.daypart_of(s.started.with_timezone(&Local).hour()).map(|d| d.name == g.daypart).unwrap_or(false)).collect();
        let val = |s: &Spin| -> Option<String> { if g.attr == "category" { Some(s.category.clone()) } else { s.attrs.get(&g.attr).cloned() } };
        let w = |s: &Spin| if g.unit == "minutes" { minutes_in(s, a, b) } else { 1.0 };
        let known: Vec<&&&Spin> = rel.iter().filter(|s| val(s).is_some()).collect();
        let total: f64 = known.iter().map(|s| w(s)).sum();
        let hits: f64 = known.iter().filter(|s| val(s).map(|v| v.eq_ignore_ascii_case(&g.value)).unwrap_or(false)).map(|s| w(s)).sum();
        let coverage = if rel.is_empty() { 0.0 } else { known.len() as f64 * 100.0 / rel.len() as f64 };
        let actual = if total > 0.0 { Some(r1(hits * 100.0 / total)) } else { None };
        let (status, score) = match actual {
            None => ("nelze vyhodnotit".to_string(), None),
            Some(x) if coverage < 50.0 => { let _ = x; ("nelze vyhodnotit".to_string(), None) }
            Some(x) => {
                let dev = (x - g.target_pct).abs();
                if dev <= g.tolerance_pct { ("ok".into(), Some(100.0)) }
                else { let over = dev - g.tolerance_pct; (if g.severity == "hard" { "fail" } else { "warn" }.into(), Some((100.0 - over / g.tolerance_pct.max(1.0) * 50.0).max(0.0))) }
            }
        };
        let examples = if status == "warn" || status == "fail" {
            let want_more = actual.map(|x| x < g.target_pct).unwrap_or(false);
            rel.iter().filter(|s| val(s).map(|v| v.eq_ignore_ascii_case(&g.value) != want_more).unwrap_or(false)).take(3)
                .map(|s| format!("{} {} – {}", s.started.with_timezone(&Local).format("%d.%m. %H:%M"), s.artist, s.title)).collect()
        } else { vec![] };
        out.push(GoalCheck { goal_id: g.id.clone(), label: goal_label(g), actual_pct: actual, target_pct: g.target_pct, tolerance_pct: g.tolerance_pct, status, severity: g.severity.clone(),
            coverage_pct: r1(coverage), denominator: format!("{} {} se známou hodnotou", if g.unit == "minutes" { format!("{total:.0} min") } else { format!("{}", known.len()) }, if g.unit == "minutes" { "hudby" } else { "hudebních spinů" }),
            examples, weight: g.weight, score });
    }
    let ev: Vec<&GoalCheck> = out.iter().filter(|g| g.score.is_some()).collect();
    let ws: f64 = ev.iter().map(|g| g.weight).sum();
    let score = if ws > 0.0 { Some(r1(ev.iter().map(|g| g.score.unwrap() * g.weight).sum::<f64>() / ws)) } else { None };
    let unev = if out.is_empty() { 0.0 } else { r1((out.len() - ev.len()) as f64 * 100.0 / out.len() as f64) };
    (out, score, unev)
}

// ---------------------------------------------------------------------------------------------
// Detail skladby / interpreta

#[derive(Debug, Clone, Serialize, Default)]
pub struct Gaps { pub n: usize, pub avg_h: f64, pub median_h: f64, pub min_h: f64, pub max_h: f64, pub p10_h: Option<f64>, pub p90_h: Option<f64> }

pub fn gaps(times: &[DateTime<Utc>]) -> Option<Gaps> {
    if times.len() < 2 { return None; }
    let mut g: Vec<f64> = times.windows(2).map(|w| (w[1] - w[0]).num_minutes() as f64 / 60.0).collect();
    g.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| g[((g.len() - 1) as f64 * p).round() as usize];
    Some(Gaps { n: g.len(), avg_h: r1(g.iter().sum::<f64>() / g.len() as f64), median_h: r1(pct(0.5)), min_h: r1(g[0]), max_h: r1(*g.last().unwrap()),
        p10_h: if g.len() >= 10 { Some(r1(pct(0.1))) } else { None }, p90_h: if g.len() >= 10 { Some(r1(pct(0.9))) } else { None } })
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PlayRow { pub id: i64, pub started: String, pub played_ms: i64, pub status: String, pub qualified: bool, pub source: String, pub category: String, pub prev: String, pub next: String, pub title: String }

#[derive(Debug, Clone, Serialize, Default)]
pub struct DetailReport {
    pub key: String,
    pub name: String,
    pub counts: BTreeMap<String, usize>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub next_planned: Option<String>,
    pub total_minutes: f64,
    pub gaps: Option<Gaps>,
    pub heatmap: Vec<Vec<usize>>,
    pub by_source: BTreeMap<String, usize>,
    pub by_category: BTreeMap<String, usize>,
    pub by_title: Vec<(String, usize)>,
    pub share_of_music_pct: Option<f64>,
    pub same_daypart_days: Vec<String>,
    pub plays: Vec<PlayRow>,
    pub rotation_status: String,
}

/// Počty za standardní období vůči dnešku (místní čas, týden od `week_start`).
pub fn period_counts(times: &[DateTime<Utc>], week_start: u32, custom: Option<(NaiveDate, NaiveDate)>) -> BTreeMap<String, usize> {
    let today = Local::now().date_naive();
    let wd = today.weekday().number_from_monday();
    let ws = today - Duration::days(((wd + 7 - week_start.clamp(1, 7)) % 7) as i64);
    let ms = today.with_day(1).unwrap();
    let pm = (ms - Duration::days(1)).with_day(1).unwrap();
    let ranges: Vec<(&str, NaiveDate, NaiveDate)> = vec![
        ("dnes", today, today), ("včera", today - Duration::days(1), today - Duration::days(1)),
        ("tento týden", ws, today), ("minulý týden", ws - Duration::days(7), ws - Duration::days(1)),
        ("tento měsíc", ms, today), ("minulý měsíc", pm, ms - Duration::days(1)),
        ("posledních 7 dní", today - Duration::days(6), today), ("posledních 30 dní", today - Duration::days(29), today),
    ];
    let mut m = BTreeMap::new();
    for (k, a, b) in ranges { m.insert(k.to_string(), times.iter().filter(|t| { let d = t.with_timezone(&Local).date_naive(); d >= a && d <= b }).count()); }
    if let Some((a, b)) = custom { m.insert("vlastní období".into(), times.iter().filter(|t| { let d = t.with_timezone(&Local).date_naive(); d >= a && d <= b }).count()); }
    m
}

pub fn detail(cfg: &SelectorConfig, all: &[Spin], pick: impl Fn(&Spin) -> bool, key: &str, custom: Option<(NaiveDate, NaiveDate)>, next_planned: Option<String>, expected_week: Option<f64>) -> DetailReport {
    let music_q = all.iter().filter(|s| is_music(s, cfg) && s.qualified).count();
    let idx: Vec<usize> = (0..all.len()).filter(|i| pick(&all[*i])).collect();
    let qual: Vec<&Spin> = idx.iter().map(|i| &all[*i]).filter(|s| s.qualified).collect();
    let times: Vec<DateTime<Utc>> = qual.iter().map(|s| s.started).collect();
    let mut r = DetailReport { key: key.into(), ..Default::default() };
    r.name = qual.first().or(idx.first().map(|i| &all[*i]).as_ref()).map(|s| if key.starts_with("track:") { format!("{} – {}", s.artist, s.title) } else { s.artist.clone() }).unwrap_or_default();
    r.counts = period_counts(&times, cfg.week_start, custom);
    r.first = times.first().map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string());
    r.last = times.last().map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string());
    r.next_planned = next_planned;
    r.total_minutes = r1(qual.iter().map(|s| s.played_ms as f64 / 60_000.0).sum());
    r.gaps = gaps(&times);
    r.heatmap = vec![vec![0; 24]; 7];
    for t in &times { let l = t.with_timezone(&Local); r.heatmap[l.weekday().num_days_from_monday() as usize][l.hour() as usize] += 1; }
    for s in &qual { *r.by_source.entry(s.source.clone()).or_default() += 1; *r.by_category.entry(s.category.clone()).or_default() += 1; }
    let mut bt: HashMap<String, usize> = HashMap::new();
    for s in &qual { *bt.entry(format!("{} – {}", s.artist, s.title)).or_default() += 1; }
    let mut btv: Vec<_> = bt.into_iter().collect();
    btv.sort_by(|a, b| b.1.cmp(&a.1));
    r.by_title = btv.into_iter().take(20).collect();
    if key.starts_with("artist:") && music_q > 0 { r.share_of_music_pct = Some(r1(qual.len() as f64 * 100.0 / music_q as f64)); }
    // stejná část dne v sousedních dnech
    for w in times.windows(2) {
        let (x, y) = (w[0].with_timezone(&Local), w[1].with_timezone(&Local));
        if (y.date_naive() - x.date_naive()).num_days() == 1 && cfg.daypart_of(x.hour()).map(|d| d.name.clone()) == cfg.daypart_of(y.hour()).map(|d| d.name.clone()) {
            r.same_daypart_days.push(format!("{} a {} – {} ({:02}:{:02} / {:02}:{:02})", x.format("%d.%m."), y.format("%d.%m."), cfg.daypart_of(x.hour()).map(|d| d.name.clone()).unwrap_or_default(), x.hour(), x.minute(), y.hour(), y.minute()));
        }
    }
    r.plays = idx.iter().rev().take(300).map(|&i| {
        let s = &all[i];
        PlayRow { id: s.id, started: s.started.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string(), played_ms: s.played_ms, status: s.status.clone(), qualified: s.qualified,
            source: s.source.clone(), category: s.category.clone(), title: format!("{} – {}", s.artist, s.title),
            prev: if i > 0 { format!("{} – {}", all[i - 1].artist, all[i - 1].title) } else { String::new() },
            next: all.get(i + 1).map(|n| format!("{} – {}", n.artist, n.title)).unwrap_or_default() }
    }).collect();
    let week = r.counts.get("posledních 7 dní").copied().unwrap_or(0) as f64;
    r.rotation_status = match expected_week {
        None => String::new(),
        Some(_) if week == 0.0 => "0 spinů v aktivní rotaci (7 dní)".into(),
        Some(e) if week < e * 0.6 => format!("méně než očekáváno ({week:.0} z {e:.1} za 7 dní)"),
        Some(e) if week > e * 1.4 => format!("nad cílem ({week:.0} z {e:.1} za 7 dní)"),
        Some(e) => format!("v normě ({week:.0} z {e:.1} za 7 dní)"),
    };
    r
}

/// Řádkový CSV export (UTF-8 s BOM, středník) s verzí metrik, pásmem a jednotkami.
pub fn csv(spins: &[Spin]) -> String {
    let mut s = String::from("\u{feff}");
    s += &format!("# {METRICS_VERSION}; časové pásmo stanice {}; délky v ms\r\n", Local::now().format("%:z"));
    s += "id;start_mistni;start_utc;typ;interpret;titul;kategorie;hrano_ms;delka_ms;kvalifikovany_spin;stav;zdroj;polozka_planu\r\n";
    let esc = |x: &str| if x.contains([';', '"', '\n']) { format!("\"{}\"", x.replace('"', "\"\"")) } else { x.to_string() };
    for e in spins {
        s += &format!("{};{};{};{};{};{};{};{};{};{};{};{};{}\r\n", e.id, e.started.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"), e.started.format("%Y-%m-%dT%H:%M:%SZ"), e.kind,
            esc(&e.artist), esc(&e.title), esc(&e.category), e.played_ms, e.duration_ms, if e.qualified { 1 } else { 0 }, e.status, e.source, e.plan_item.map(|x| x.to_string()).unwrap_or_default());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sp(id: i64, track: i64, t: &str, played: i64) -> Spin {
        Spin { id, track_id: Some(track), kind: "song".into(), artist: format!("A{track}"), title: format!("T{track}"), started: DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc), played_ms: played, duration_ms: 200_000, channel: "main".into(), ..Default::default() }
    }
    #[test]
    fn qualify_and_jaccard() {
        let cfg = SelectorConfig::default();
        let mut v = vec![sp(1, 1, "2026-11-20T10:00:00Z", 200_000), sp(2, 2, "2026-11-20T10:04:00Z", 5_000), sp(3, 1, "2026-11-21T10:00:00Z", 200_000), sp(4, 3, "2026-11-21T11:00:00Z", 60_000)];
        for s in v.iter_mut() { classify(s, &cfg); }
        assert!(v[0].qualified && !v[1].qualified && v[3].qualified);
        assert_eq!(v[1].status, "skipped");
        let r = station_report(&cfg, &v, NaiveDate::from_ymd_opt(2026, 11, 20).unwrap(), NaiveDate::from_ymd_opt(2026, 11, 21).unwrap(), "plan", None);
        assert_eq!(r.music_spins, 3);
        assert_eq!(r.music_starts, 4);
        // den 1 = {1}, den 2 = {1,3} → Jaccard 1/2
        assert_eq!(r.jaccard_avg, Some(0.5));
        assert_eq!(r.categories[0].count, 3); // jmenovatel = kvalifikované hudební spiny
    }
    #[test]
    fn minutes_across_midnight() {
        let s = sp(1, 1, "2026-11-20T22:58:00Z", 240_000); // místní čas závisí na pásmu – kontrola rozdělení v UTC
        let a = DateTime::parse_from_rfc3339("2026-11-20T00:00:00Z").unwrap().with_timezone(&Utc);
        let m = DateTime::parse_from_rfc3339("2026-11-20T23:00:00Z").unwrap().with_timezone(&Utc);
        assert!((minutes_in(&s, a, m) - 2.0).abs() < 0.01);
        assert!((minutes_in(&s, m, m + Duration::days(1)) - 2.0).abs() < 0.01);
    }

    #[test]
    fn as_played_idempotent_and_cue() {
        let dir = std::env::temp_dir().join(format!("orca-an-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        let t = crate::model::Track { id: 0, kind: Some(crate::model::TrackKind::Song), artist: "A".into(), title: "T".into(), duration_ms: 200_000, ..Default::default() };
        let a = db.log_start_ex(&t, "auto", "", Some("uid-1"), "main", None).unwrap();
        let b = db.log_start_ex(&t, "auto", "", Some("uid-1"), "main", None).unwrap(); // replay téže položky
        assert_eq!(a, b);
        db.log_end(a, 150_000).unwrap();
        db.log_end(a, 2_000).unwrap(); // pozdě doručený konec kratšího přehrání nepřepíše delší
        let _ = db.log_start_ex(&t, "instant", "", Some("uid-2"), "overlay", None).unwrap(); // jiný výstup
        let cfg = SelectorConfig::default();
        let today = Local::now().date_naive();
        let s = load_spins(&db, &cfg, today - Duration::days(1), today + Duration::days(1)).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].played_ms, 150_000);
        assert!(s[0].qualified);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
