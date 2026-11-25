//! Obchodní a provozní analytika stanice: reklamní zátěž, odhad tržeb, plnění kampaní, klienti a obory,
//! skladba programu (hudba / mluvené slovo / reklama / imaging), podíl živého vysílání a provozní ukazatele.
//! Počítá se ze skutečně odvysílaného logu (play_log) a údajů kampaní.

use crate::db::Db;
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Default)]
pub struct ClientRow { pub client: String, pub campaigns: usize, pub spots: i64, pub minutes: f64, pub revenue: f64, pub share_pct: f64 }

#[derive(Debug, Clone, Serialize, Default)]
pub struct CampaignRow {
    pub id: i64, pub name: String, pub client: String, pub industry: String,
    pub start: Option<String>, pub end: Option<String>,
    pub ordered: i64, pub played_total: i64, pub played_range: i64, pub expected_now: Option<f64>,
    /// done | ok | behind | ahead | noorder | ended_short
    pub status: String, pub pct: Option<f64>, pub revenue_range: f64, pub price: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Program { pub music: f64, pub spoken: f64, pub ads: f64, pub imaging: f64, pub other: f64 }

#[derive(Debug, Clone, Serialize, Default)]
pub struct DayRow { pub day: String, pub on_air_min: f64, pub ad_min: f64, pub spots: i64, pub revenue: f64, pub live_pct: f64 }

#[derive(Debug, Clone, Serialize, Default)]
pub struct BizReport {
    pub from: String, pub to: String, pub days: i64,
    pub on_air_hours: f64,
    pub spots: i64, pub ad_minutes: f64,
    /// průměrná reklamní minutáž na hodinu vysílání
    pub ad_min_per_hour: f64,
    /// nejvytíženější hodina (reklamní minuty) – „den hh:00“
    pub peak_ad_hour: Option<(String, f64)>,
    /// hodiny s reklamou nad 12 minut (běžný strop / doporučení)
    pub hours_over_limit: i64,
    pub ad_heatmap: Vec<Vec<f64>>,
    pub partial_spots: i64,
    pub spots_without_campaign: i64,
    pub revenue: f64, pub revenue_per_hour: f64, pub priced: bool,
    pub clients: Vec<ClientRow>, pub industries: Vec<(String, i64, f64)>,
    pub campaigns: Vec<CampaignRow>,
    pub program: Program,
    pub imaging_per_hour: f64,
    pub live_pct: f64,
    pub sources: Vec<(String, i64)>,
    pub skipped_music: i64,
    pub daily: Vec<DayRow>,
    pub hourly_ad: Vec<f64>,
}

fn utc_bound(d: NaiveDate) -> String {
    let l = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().unwrap_or_else(|| Local::now());
    l.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub fn report(db: &Db, from: NaiveDate, to: NaiveDate) -> Result<BizReport> {
    let (a, b) = (utc_bound(from), utc_bound(to + chrono::Duration::days(1)));
    let days = (to - from).num_days() + 1;
    // spot → kampaň
    struct Camp { id: i64, name: String, client: String, industry: String, start: Option<String>, end: Option<String>, ordered: i64, price: f64, created: String }
    let (rows, camps, spot_camp, played_total) = db.with_conn(|c| -> Result<_> {
        let mut st = c.prepare("SELECT kind, track_id, COALESCE(played_ms, duration_ms), duration_ms, source, started_at FROM play_log WHERE started_at >= ?1 AND started_at < ?2 AND channel='main' ORDER BY started_at")?;
        let rows = st.query_map([&a, &b], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut st = c.prepare("SELECT c.id, c.name, COALESCE(NULLIF(cl.name,''), c.client), COALESCE(NULLIF(c.industry,''), COALESCE(cl.industry,'')), c.start_at, c.expires_at, c.ordered_plays, c.price_per_spot, c.created_at FROM campaigns c LEFT JOIN clients cl ON cl.id = c.client_id WHERE c.is_commercial = 1")?;
        let camps = st.query_map([], |r| Ok(Camp { id: r.get(0)?, name: r.get(1)?, client: r.get(2)?, industry: r.get(3)?, start: r.get(4)?, end: r.get(5)?, ordered: r.get(6)?, price: r.get(7)?, created: r.get(8)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut st = c.prepare("SELECT track_id, campaign_id FROM campaign_spots ORDER BY campaign_id")?;
        let mut sc: HashMap<i64, i64> = HashMap::new();
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? { let (t, cid) = r?; sc.entry(t).or_insert(cid); }
        // odvysíláno celkem (od začátku kampaně do konce období)
        let mut st = c.prepare("SELECT track_id, COUNT(*) FROM play_log WHERE kind='spot' AND started_at < ?1 GROUP BY track_id")?;
        let mut pt: HashMap<i64, i64> = HashMap::new();
        for r in st.query_map([&b], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?)))? { let (t, n) = r?; if let Some(t) = t { if let Some(cid) = sc.get(&t) { *pt.entry(*cid).or_default() += n; } } }
        Ok((rows, camps, sc, pt))
    })?;
    let camp_by_id: HashMap<i64, &Camp> = camps.iter().map(|c| (c.id, c)).collect();

    let mut r = BizReport { from: from.to_string(), to: to.to_string(), days, ad_heatmap: vec![vec![0.0; 24]; 7], hourly_ad: vec![0.0; 24], ..Default::default() };
    let mut on_air_ms = 0i64;
    let mut hour_ad: BTreeMap<String, f64> = BTreeMap::new();
    let mut hours_seen: std::collections::HashSet<String> = Default::default();
    let mut client_map: BTreeMap<String, ClientRow> = BTreeMap::new();
    let mut client_camps: HashMap<String, std::collections::HashSet<i64>> = HashMap::new();
    let mut ind: BTreeMap<String, i64> = BTreeMap::new();
    let mut camp_range: HashMap<i64, i64> = HashMap::new();
    let mut src: BTreeMap<String, i64> = BTreeMap::new();
    let mut day: BTreeMap<String, DayRow> = BTreeMap::new();
    let mut live_ms_day: HashMap<String, i64> = HashMap::new();
    let mut live_ms = 0i64; let mut imaging = 0i64;
    for (kind, tid, played, dur, source, started) in &rows {
        let Ok(t) = DateTime::parse_from_rfc3339(started) else { continue };
        let t = t.with_timezone(&Local);
        let ms = (*played).clamp(0, (*dur).max(*played)).min(3 * 3_600_000);
        let min = ms as f64 / 60_000.0;
        on_air_ms += ms;
        let dkey = t.date_naive().to_string();
        let hkey = format!("{} {:02}", dkey, t.hour());
        hours_seen.insert(hkey.clone());
        let dr = day.entry(dkey.clone()).or_insert_with(|| DayRow { day: dkey.clone(), ..Default::default() });
        dr.on_air_min += min;
        *src.entry(source.clone()).or_default() += 1;
        let live = matches!(source.as_str(), "manual" | "instant" | "cart" | "live" | "pad");
        if live { live_ms += ms; *live_ms_day.entry(dkey.clone()).or_default() += ms; }
        match kind.as_str() {
            "song" => { r.program.music += min; if *dur > 0 && *played < dur * 6 / 10 { r.skipped_music += 1; } }
            "voicetrack" => r.program.spoken += min,
            "jingle" | "sweeper" | "instant" => { r.program.imaging += min; imaging += 1; }
            "spot" => {
                r.program.ads += min; r.spots += 1; r.ad_minutes += min;
                dr.ad_min += min; dr.spots += 1;
                *hour_ad.entry(hkey.clone()).or_default() += min;
                let wd = t.weekday().num_days_from_monday() as usize;
                r.ad_heatmap[wd][t.hour() as usize] += min;
                r.hourly_ad[t.hour() as usize] += min;
                if *dur > 0 && *played < dur * 9 / 10 { r.partial_spots += 1; }
                match tid.and_then(|t| spot_camp.get(&t)).and_then(|cid| camp_by_id.get(cid)) {
                    Some(c) => {
                        *camp_range.entry(c.id).or_default() += 1;
                        let name = if c.client.trim().is_empty() { c.name.clone() } else { c.client.clone() };
                        let cr = client_map.entry(name.clone()).or_insert_with(|| ClientRow { client: name.clone(), ..Default::default() });
                        cr.spots += 1; cr.minutes += min; cr.revenue += c.price;
                        client_camps.entry(name).or_default().insert(c.id);
                        *ind.entry(if c.industry.trim().is_empty() { "?".into() } else { c.industry.clone() }).or_default() += 1;
                        r.revenue += c.price; dr.revenue += c.price;
                        if c.price > 0.0 { r.priced = true; }
                    }
                    None => r.spots_without_campaign += 1,
                }
            }
            _ => r.program.other += min,
        }
    }
    r.on_air_hours = on_air_ms as f64 / 3_600_000.0;
    let hours = hours_seen.len().max(1) as f64;
    r.ad_min_per_hour = r.ad_minutes / hours;
    r.revenue_per_hour = r.revenue / hours;
    r.imaging_per_hour = imaging as f64 / hours;
    r.live_pct = if on_air_ms > 0 { live_ms as f64 * 100.0 / on_air_ms as f64 } else { 0.0 };
    r.peak_ad_hour = hour_ad.iter().max_by(|x, y| x.1.partial_cmp(y.1).unwrap()).map(|(k, v)| (k.clone(), *v));
    r.hours_over_limit = hour_ad.values().filter(|v| **v > 12.0).count() as i64;
    // průměr na jeden výskyt dne v týdnu / hodiny v období
    let mut wd_count = [0f64; 7];
    let mut d = from; while d <= to { wd_count[d.weekday().num_days_from_monday() as usize] += 1.0; d += chrono::Duration::days(1); }
    for (w, row) in r.ad_heatmap.iter_mut().enumerate() { for v in row.iter_mut() { *v = (*v / wd_count[w].max(1.0) * 10.0).round() / 10.0; } }
    for v in r.hourly_ad.iter_mut() { *v = (*v / days.max(1) as f64 * 10.0).round() / 10.0; }
    let total_spots = r.spots.max(1) as f64;
    r.clients = client_map.into_values().map(|mut c| { c.campaigns = client_camps.get(&c.client).map(|s| s.len()).unwrap_or(0); c.share_pct = c.spots as f64 * 100.0 / total_spots; c }).collect();
    r.clients.sort_by(|a, b| b.spots.cmp(&a.spots).then(b.revenue.partial_cmp(&a.revenue).unwrap_or(std::cmp::Ordering::Equal)));
    let known: i64 = ind.values().sum();
    r.industries = ind.into_iter().map(|(k, v)| { let p = v as f64 * 100.0 / known.max(1) as f64; (k, v, p) }).collect();
    r.industries.sort_by(|a, b| b.1.cmp(&a.1));
    r.sources = src.into_iter().collect(); r.sources.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, dr) in day.iter_mut() { dr.live_pct = live_ms_day.get(k).copied().unwrap_or(0) as f64 * 100.0 / (dr.on_air_min * 60_000.0).max(1.0); }
    r.daily = day.into_values().collect();

    // kampaně: běžící v období nebo s odvysíláním v období
    let now = Local::now().naive_local();
    let end_of_range = (to + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap();
    let parse = |s: &Option<String>| s.as_deref().and_then(|x| chrono::NaiveDateTime::parse_from_str(x, "%Y-%m-%d %H:%M").ok().or_else(|| chrono::NaiveDateTime::parse_from_str(x, "%Y-%m-%dT%H:%M:%S").ok()).or_else(|| NaiveDate::parse_from_str(&x[..x.len().min(10)], "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0))));
    for c in &camps {
        let s = parse(&c.start).or_else(|| DateTime::parse_from_rfc3339(&c.created).ok().map(|d| d.with_timezone(&Local).naive_local()));
        let e = parse(&c.end);
        let overlaps = s.map(|s| s < end_of_range).unwrap_or(true) && e.map(|e| e >= from.and_hms_opt(0, 0, 0).unwrap()).unwrap_or(true);
        let pr = camp_range.get(&c.id).copied().unwrap_or(0);
        if !overlaps && pr == 0 { continue; }
        let pt = played_total.get(&c.id).copied().unwrap_or(0);
        let mut row = CampaignRow { id: c.id, name: c.name.clone(), client: c.client.clone(), industry: c.industry.clone(), start: c.start.clone(), end: c.end.clone(), ordered: c.ordered, played_total: pt, played_range: pr, price: c.price, revenue_range: pr as f64 * c.price, status: "noorder".into(), ..Default::default() };
        if c.ordered > 0 {
            row.pct = Some(pt as f64 * 100.0 / c.ordered as f64);
            let at = now.min(end_of_range);
            let exp = match (s, e) { (Some(s), Some(e)) if e > s => Some(((at - s).num_seconds() as f64 / (e - s).num_seconds() as f64).clamp(0.0, 1.0) * c.ordered as f64), _ => None };
            row.expected_now = exp.map(|x| (x * 10.0).round() / 10.0);
            row.status = if pt >= c.ordered { "done".into() } else if e.map(|e| e < now).unwrap_or(false) { "ended_short".into() } else {
                match exp { Some(x) if x >= 1.0 && (pt as f64) < x * 0.85 => "behind".into(), Some(x) if x >= 1.0 && (pt as f64) > x * 1.15 => "ahead".into(), _ => "ok".into() }
            };
        }
        r.campaigns.push(row);
    }
    let rank = |s: &str| match s { "ended_short" => 0, "behind" => 1, "ahead" => 2, "ok" => 3, "noorder" => 4, _ => 5 };
    r.campaigns.sort_by(|a, b| rank(&a.status).cmp(&rank(&b.status)).then(b.played_range.cmp(&a.played_range)));
    for v in [&mut r.ad_minutes, &mut r.ad_min_per_hour, &mut r.on_air_hours, &mut r.imaging_per_hour, &mut r.live_pct, &mut r.revenue_per_hour] { *v = (*v * 10.0).round() / 10.0; }
    Ok(r)
}
