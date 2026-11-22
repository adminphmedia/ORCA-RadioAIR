//! Hlášení pro kolektivní správce (OSA – autoři, INTERGRAM – výkonní umělci a výrobci záznamů).
//!
//! Zdroj dat je skutečný log vysílání (`play_log`, hlavní výstup). Metadata se berou z knihovny
//! (pokud je mezitím někdo opravil), jinak ze snímku v logu v okamžiku odvysílání.
//!
//! Struktura sloupců je šablona – výchozí šablona vychází z náležitostí podle licenční smlouvy OSA
//! pro rozhlasové vysílání (datum, název, všichni autoři vč. textařů, stopáž, značka výrobce, způsob užití)
//! a z údajů, které INTERGRAM potřebuje k přiřazení nahrávek (interpret, název, ISRC, výrobce).
//! Přesný formát (pořadí sloupců, kódování) si stanice ověří u OSA/INTERGRAM a šablonu případně upraví.

use crate::db::Db;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const SETTING: &str = "cmo_config";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Column { pub key: String, pub header: String }

fn col(key: &str, header: &str) -> Column { Column { key: key.into(), header: header.into() } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profile {
    /// Druhy položek, které se hlásí (song, jingle, sweeper, bed, spot, instant …)
    pub kinds: Vec<String>,
    pub columns: Vec<Column>,
    /// Povinné údaje kontrolované validací: composer, lyricist, label, isrc, artist, title, duration
    pub require: Vec<String>,
}
impl Default for Profile { fn default() -> Self { Self { kinds: vec![], columns: vec![], require: vec![] } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CmoConfig {
    /// Název stanice (sloupec „stanice“)
    pub station: String,
    /// Značka výrobce pro vlastní produkci (znělky, předěly, podkresy bez vlastního údaje)
    pub own_label: String,
    /// Autor vlastní produkce (znělky apod.), pokud není u položky vyplněn
    pub own_author: String,
    pub delimiter: String,
    /// "utf-8" (s BOM, pro Excel) nebo "windows-1250"
    pub encoding: String,
    /// Kratší odvysílání (v sekundách) se nehlásí (0 = hlásit vše)
    pub min_played_s: u32,
    /// Způsob užití podle druhu položky
    pub usage: BTreeMap<String, String>,
    pub osa: Profile,
    pub intergram: Profile,
}

impl Default for CmoConfig {
    fn default() -> Self {
        let usage = [("song", "hlavní (vysílání skladby)"), ("jingle", "znělka"), ("sweeper", "předěl"), ("instant", "předěl"), ("bed", "podkres"), ("spot", "reklama"), ("voicetrack", "mluvené slovo"), ("asset", "ostatní")]
            .into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        Self {
            station: String::new(), own_label: String::new(), own_author: String::new(), delimiter: ";".into(), encoding: "utf-8".into(), min_played_s: 0, usage,
            osa: Profile {
                kinds: ["song", "jingle", "sweeper", "instant", "bed", "spot"].map(String::from).to_vec(),
                columns: vec![col("date", "Datum vysílání"), col("hour", "Hodina"), col("time", "Čas začátku"), col("title", "Název díla"), col("artist", "Interpret"),
                    col("composer", "Autor hudby"), col("lyricist", "Autor textu"), col("duration", "Stopáž (mm:ss)"), col("duration_s", "Stopáž (s)"),
                    col("label", "Značka výrobce"), col("isrc", "ISRC"), col("usage", "Způsob užití")],
                require: ["title", "composer", "lyricist", "label", "duration"].map(String::from).to_vec(),
            },
            intergram: Profile {
                kinds: vec!["song".into()],
                columns: vec![col("date", "Datum"), col("time", "Čas začátku"), col("end_time", "Čas konce"), col("artist", "Interpret"), col("title", "Název snímku"),
                    col("album", "Album"), col("label", "Výrobce / label"), col("isrc", "ISRC"), col("year", "Rok vydání"), col("duration_s", "Stopáž (s)"), col("station", "Stanice")],
                require: ["artist", "title", "isrc", "label", "duration"].map(String::from).to_vec(),
            },
        }
    }
}

/// Dostupné sloupce šablony (klíč, popis).
pub const COLUMN_KEYS: &[(&str, &str)] = &[
    ("date", "Datum (dd.mm.rrrr)"), ("hour", "Hodina (08–09)"), ("time", "Čas začátku"), ("end_time", "Čas konce"), ("title", "Název"), ("artist", "Interpret"),
    ("composer", "Autor hudby"), ("lyricist", "Autor textu"), ("authors", "Autoři (hudba / text)"), ("duration", "Stopáž mm:ss"), ("duration_s", "Stopáž v sekundách"),
    ("label", "Značka výrobce / label"), ("publisher", "Hudební nakladatel"), ("isrc", "ISRC"), ("album", "Album"), ("year", "Rok vydání"), ("usage", "Způsob užití"),
    ("kind", "Druh položky"), ("category", "Kategorie"), ("station", "Stanice"),
];

pub fn load_config(db: &Db) -> Result<CmoConfig> {
    Ok(db.get_setting(SETTING)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

pub fn save_config(db: &Db, c: &CmoConfig) -> Result<()> {
    if c.delimiter.is_empty() || c.delimiter.chars().count() != 1 { return Err(anyhow!("oddělovač musí být jeden znak")); }
    for p in [&c.osa, &c.intergram] {
        if p.columns.is_empty() { return Err(anyhow!("šablona musí mít aspoň jeden sloupec")); }
        if let Some(bad) = p.columns.iter().find(|x| !COLUMN_KEYS.iter().any(|(k, _)| *k == x.key)) { return Err(anyhow!("neznámý sloupec „{}“", bad.key)); }
    }
    db.set_setting(SETTING, &serde_json::to_string(c)?)
}

/// Jedno odvysílání s doplněnými údaji.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Use {
    pub track_id: Option<i64>,
    pub kind: String,
    pub start: DateTime<Local>,
    pub end: Option<DateTime<Local>>,
    pub played_ms: i64,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub year: Option<i32>,
    pub composer: String,
    pub lyricist: String,
    pub label: String,
    pub publisher: String,
    pub isrc: String,
    pub category: String,
    pub instrumental: bool,
}

/// Rozsah měsíce „rrrr-mm“ v místním čase → (od, do) v UTC.
pub fn month_range(month: &str) -> Result<(NaiveDate, DateTime<Utc>, DateTime<Utc>)> {
    let d = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").map_err(|_| anyhow!("měsíc zadejte jako rrrr-mm"))?;
    let next = if d.month() == 12 { NaiveDate::from_ymd_opt(d.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1) }.unwrap();
    let loc = |x: NaiveDate| Local.from_local_datetime(&x.and_hms_opt(0, 0, 0).unwrap()).earliest().unwrap().with_timezone(&Utc);
    Ok((d, loc(d), loc(next)))
}

fn utc_str(d: DateTime<Utc>) -> String { d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string() }

/// Načte odvysílané položky hlavního výstupu za měsíc.
pub fn uses(db: &Db, month: &str, cfg: &CmoConfig) -> Result<Vec<Use>> {
    let (_, from, to) = month_range(month)?;
    let rows = db.with_conn(|c| -> Result<Vec<Use>> {
        let mut st = c.prepare(
            "SELECT p.track_id, p.kind, p.started_at, p.ended_at, COALESCE(p.played_ms, p.duration_ms), p.duration_ms,
                    COALESCE(NULLIF(t.artist,''), p.artist), COALESCE(NULLIF(t.title,''), p.title), COALESCE(NULLIF(t.album,''), p.album), t.year,
                    COALESCE(NULLIF(t.composer,''), p.composer), COALESCE(NULLIF(t.lyricist,''), p.lyricist), COALESCE(NULLIF(t.label,''), p.label),
                    COALESCE(NULLIF(t.publisher,''), p.publisher), COALESCE(NULLIF(t.isrc,''), p.isrc), p.category, COALESCE(t.vocal,'')
             FROM play_log p LEFT JOIN tracks t ON t.id = p.track_id
             WHERE p.started_at >= ?1 AND p.started_at < ?2 AND p.channel = 'main' ORDER BY p.started_at")?;
        let v = st.query_map(params![utc_str(from), utc_str(to)], |r| {
            let ps = |s: String| DateTime::parse_from_rfc3339(&s).ok().map(|d| d.with_timezone(&Local));
            let played: i64 = r.get(4)?;
            let dur: i64 = r.get(5)?;
            let vocal: String = r.get(16)?;
            Ok(Use {
                track_id: r.get(0)?, kind: r.get(1)?, start: ps(r.get(2)?).unwrap_or_else(Local::now), end: r.get::<_, Option<String>>(3)?.and_then(ps),
                played_ms: if played > 0 { played } else { dur }, artist: r.get(6)?, title: r.get(7)?, album: r.get(8)?, year: r.get(9)?,
                composer: r.get(10)?, lyricist: r.get(11)?, label: r.get(12)?, publisher: r.get(13)?, isrc: r.get(14)?, category: r.get(15)?,
                instrumental: { let v = crate::import::norm(&vocal); v.contains("instr") || v == "bezzpevu" },
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })?;
    let own = |u: &mut Use| {
        if u.kind != "song" {
            if u.label.is_empty() { u.label = cfg.own_label.clone(); }
            if u.composer.is_empty() { u.composer = cfg.own_author.clone(); }
        }
    };
    Ok(rows.into_iter().filter(|u| u.played_ms >= cfg.min_played_s as i64 * 1000).map(|mut u| { own(&mut u); u }).collect())
}

/// ISRC: CC-XXX-YY-NNNNN (12 znaků, pomlčky a mezery se ignorují).
pub fn isrc_valid(s: &str) -> bool {
    let n: String = s.chars().filter(|c| !matches!(c, '-' | ' ')).collect::<String>().to_uppercase();
    n.len() == 12 && n[..2].chars().all(|c| c.is_ascii_alphabetic()) && n[2..5].chars().all(|c| c.is_ascii_alphanumeric()) && n[5..].chars().all(|c| c.is_ascii_digit())
}

pub fn isrc_norm(s: &str) -> String { s.chars().filter(|c| !matches!(c, '-' | ' ')).collect::<String>().to_uppercase() }

fn mmss(ms: i64) -> String { let s = (ms as f64 / 1000.0).round() as i64; format!("{}:{:02}", s / 60, s % 60) }

fn value(u: &Use, key: &str, cfg: &CmoConfig) -> String {
    match key {
        "date" => u.start.format("%d.%m.%Y").to_string(),
        "hour" => format!("{:02}–{:02}", u.start.hour(), (u.start.hour() + 1) % 24),
        "time" => u.start.format("%H:%M:%S").to_string(),
        "end_time" => u.end.unwrap_or(u.start + Duration::milliseconds(u.played_ms)).format("%H:%M:%S").to_string(),
        "title" => u.title.clone(),
        "artist" => u.artist.clone(),
        "composer" => u.composer.clone(),
        "lyricist" => if u.lyricist.is_empty() && u.instrumental { "(instrumentální)".into() } else { u.lyricist.clone() },
        "authors" => [u.composer.as_str(), u.lyricist.as_str()].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" / "),
        "duration" => mmss(u.played_ms),
        "duration_s" => format!("{}", (u.played_ms as f64 / 1000.0).round() as i64),
        "label" => u.label.clone(),
        "publisher" => u.publisher.clone(),
        "isrc" => if isrc_valid(&u.isrc) { isrc_norm(&u.isrc) } else { u.isrc.clone() },
        "album" => u.album.clone(),
        "year" => u.year.map(|y| y.to_string()).unwrap_or_default(),
        "usage" => cfg.usage.get(&u.kind).cloned().unwrap_or_default(),
        "kind" => u.kind.clone(),
        "category" => u.category.clone(),
        "station" => cfg.station.clone(),
        _ => String::new(),
    }
}

fn field(s: &str, d: char) -> String {
    if s.contains(d) || s.contains('"') || s.contains('\n') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

pub fn profile<'a>(cfg: &'a CmoConfig, which: &str) -> Result<&'a Profile> {
    match which { "osa" => Ok(&cfg.osa), "intergram" => Ok(&cfg.intergram), _ => Err(anyhow!("neznámé hlášení „{which}“")) }
}

/// CSV text podle šablony (bez kódování).
pub fn export_text(cfg: &CmoConfig, which: &str, all: &[Use]) -> Result<String> {
    let p = profile(cfg, which)?;
    let d = cfg.delimiter.chars().next().unwrap_or(';');
    let mut out = p.columns.iter().map(|c| field(&c.header, d)).collect::<Vec<_>>().join(&d.to_string());
    out.push_str("\r\n");
    for u in all.iter().filter(|u| p.kinds.contains(&u.kind)) {
        out += &p.columns.iter().map(|c| field(&value(u, &c.key, cfg), d)).collect::<Vec<_>>().join(&d.to_string());
        out.push_str("\r\n");
    }
    Ok(out)
}

/// Bajty souboru v nastaveném kódování.
pub fn encode(cfg: &CmoConfig, text: &str) -> (Vec<u8>, &'static str) {
    if cfg.encoding.eq_ignore_ascii_case("windows-1250") {
        let (b, _, _) = encoding_rs::WINDOWS_1250.encode(text);
        (b.into_owned(), "windows-1250")
    } else {
        let mut v = vec![0xEF, 0xBB, 0xBF];
        v.extend_from_slice(text.as_bytes());
        (v, "utf-8")
    }
}

// ---------------------------------------------------------------------------------------------
// Kontrola úplnosti

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub track_id: Option<i64>,
    pub artist: String,
    pub title: String,
    pub kind: String,
    pub plays: usize,
    pub missing: Vec<String>,
    pub invalid: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub profile: String,
    pub month: String,
    pub deadline: String,
    pub plays: usize,
    pub works: usize,
    pub complete_works: usize,
    /// Počet řádků hlášení, které by byly neúplné
    pub incomplete_rows: usize,
    pub missing_counts: BTreeMap<String, usize>,
    pub issues: Vec<Issue>,
    pub ok: bool,
    pub notes: Vec<String>,
}

fn easter(y: i32) -> NaiveDate {
    let a = y % 19; let b = y / 100; let c = y % 100; let d = b / 4; let e = b % 4; let f = (b + 8) / 25; let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30; let i = c / 4; let k = c % 4; let l = (32 + 2 * e + 2 * i - h - k) % 7; let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31; let day = (h + l - 7 * m + 114) % 31 + 1;
    NaiveDate::from_ymd_opt(y, month as u32, day as u32).unwrap()
}

/// Státní svátky ČR (pevné + Velký pátek a Velikonoční pondělí).
pub fn cz_holiday(d: NaiveDate) -> bool {
    const FIXED: [(u32, u32); 11] = [(1, 1), (5, 1), (5, 8), (7, 5), (7, 6), (9, 28), (10, 28), (11, 17), (12, 24), (12, 25), (12, 26)];
    if FIXED.contains(&(d.month(), d.day())) { return true; }
    let e = easter(d.year());
    d == e - Duration::days(2) || d == e + Duration::days(1)
}

/// Lhůta: 15. pracovní den po skončení měsíce.
pub fn deadline(month_start: NaiveDate) -> NaiveDate {
    let mut d = if month_start.month() == 12 { NaiveDate::from_ymd_opt(month_start.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(month_start.year(), month_start.month() + 1, 1) }.unwrap();
    let mut n = 0;
    loop {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) && !cz_holiday(d) { n += 1; if n == 15 { return d; } }
        d += Duration::days(1);
    }
}

pub const FIELD_NAMES: &[(&str, &str)] = &[("title", "název"), ("artist", "interpret"), ("composer", "autor hudby"), ("lyricist", "autor textu"), ("label", "značka výrobce"), ("isrc", "ISRC"), ("duration", "stopáž")];

pub fn validate(cfg: &CmoConfig, which: &str, month: &str, all: &[Use]) -> Result<Report> {
    let p = profile(cfg, which)?;
    let (start, _, _) = month_range(month)?;
    let mut groups: HashMap<String, (Use, usize)> = HashMap::new();
    let mut order: Vec<String> = vec![];
    let rows: Vec<&Use> = all.iter().filter(|u| p.kinds.contains(&u.kind)).collect();
    for u in &rows {
        let k = u.track_id.map(|t| format!("#{t}")).unwrap_or_else(|| format!("{}|{}", u.artist.to_lowercase(), u.title.to_lowercase()));
        let e = groups.entry(k.clone()).or_insert_with(|| { order.push(k.clone()); ((*u).clone(), 0) });
        e.1 += 1;
    }
    let mut issues = vec![];
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut incomplete_rows = 0;
    for k in &order {
        let (u, n) = &groups[k];
        let mut missing = vec![];
        let mut invalid = vec![];
        for f in &p.require {
            let need = match f.as_str() {
                // text jen u zpívaných skladeb (u instrumentálních a vlastní produkce se nepožaduje)
                "lyricist" => u.kind == "song" && !u.instrumental,
                // ISRC mají jen vydané nahrávky
                "isrc" => u.kind == "song",
                _ => true,
            };
            if !need { continue; }
            let empty = match f.as_str() {
                "title" => u.title.trim().is_empty(), "artist" => u.artist.trim().is_empty(), "composer" => u.composer.trim().is_empty(),
                "lyricist" => u.lyricist.trim().is_empty(), "label" => u.label.trim().is_empty(), "isrc" => u.isrc.trim().is_empty(), "duration" => u.played_ms <= 0,
                _ => false,
            };
            if empty { missing.push(f.clone()); }
            else if f == "isrc" && !isrc_valid(&u.isrc) { invalid.push("isrc".into()); }
        }
        if !missing.is_empty() || !invalid.is_empty() {
            for m in missing.iter().chain(invalid.iter()) { *counts.entry(m.clone()).or_default() += 1; }
            incomplete_rows += n;
            issues.push(Issue { track_id: u.track_id, artist: u.artist.clone(), title: u.title.clone(), kind: u.kind.clone(), plays: *n, missing, invalid });
        }
    }
    issues.sort_by(|a, b| b.plays.cmp(&a.plays).then(a.title.cmp(&b.title)));
    let works = order.len();
    let dl = deadline(start);
    let mut notes = vec![format!("Hlášení za {month} odevzdejte nejpozději {} (15. pracovní den po skončení měsíce).", dl.format("%-d. %-m. %Y"))];
    if which == "osa" {
        notes.push("Podle licenční smlouvy OSA se hlásí všechna užitá díla včetně znělek, reklam a podkresů; za každý chybný nebo neúplný údaj hrozí smluvní pokuta.".into());
        if cfg.own_label.is_empty() { notes.push("Vyplňte „Značku výrobce pro vlastní produkci“ – doplní se u znělek a předělů bez vlastního údaje.".into()); }
    } else {
        notes.push("INTERGRAM přiřazuje nahrávky hlavně podle ISRC – bez něj se odměna výkonným umělcům a výrobcům nemusí rozdělit správně.".into());
    }
    notes.push("Strukturu sloupců si jednou ověřte u kolektivního správce; šablonu můžete upravit v nastavení hlášení.".into());
    Ok(Report {
        profile: which.into(), month: month.into(), deadline: dl.to_string(), plays: rows.len(), works, complete_works: works - issues.len(), incomplete_rows,
        missing_counts: counts, ok: issues.is_empty() && !rows.is_empty(), issues, notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isrc_and_deadline() {
        assert!(isrc_valid("CZ-A12-26-00001"));
        assert!(isrc_valid("usrc17607839"));
        assert!(!isrc_valid("CZA122600001X"));
        assert!(!isrc_valid("1ZA122600001"));
        // listopad 2026: 1. 12. je úterý; 24.–26. 12. svátky → 15. pracovní den je 21. 12.
        assert_eq!(deadline(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()), NaiveDate::from_ymd_opt(2026, 12, 21).unwrap());
        // březen 2027: Velikonoční pondělí 29. 3. 2027 leží mimo, Velký pátek 26. 3. také; duben: 1. 4. čt …
        assert!(cz_holiday(NaiveDate::from_ymd_opt(2027, 3, 29).unwrap()));
        assert!(cz_holiday(NaiveDate::from_ymd_opt(2027, 3, 26).unwrap()));
    }

    #[test]
    fn validation_and_export() {
        let cfg = CmoConfig { own_label: "Rádio Test".into(), own_author: "Jan Novák".into(), ..Default::default() };
        let t0 = Local.with_ymd_and_hms(2026, 11, 3, 8, 15, 0).unwrap();
        let ok = Use { track_id: Some(1), kind: "song".into(), start: t0, played_ms: 201_000, artist: "A".into(), title: "Píseň; první".into(), composer: "K".into(), lyricist: "L".into(), label: "Supraphon".into(), isrc: "CZ-A12-26-00001".into(), ..Default::default() };
        let mut bad = ok.clone(); bad.track_id = Some(2); bad.lyricist.clear(); bad.isrc = "123".into(); bad.title = "B".into();
        let mut instr = bad.clone(); instr.track_id = Some(3); instr.instrumental = true; instr.isrc = "CZA122600002".into();
        let mut jingle = Use { track_id: Some(4), kind: "jingle".into(), start: t0, played_ms: 5000, title: "ID".into(), ..Default::default() };
        jingle.label = cfg.own_label.clone(); jingle.composer = cfg.own_author.clone();
        let all = vec![ok.clone(), bad.clone(), bad.clone(), instr, jingle];
        let r = validate(&cfg, "osa", "2026-11", &all).unwrap();
        assert_eq!(r.plays, 5);
        assert_eq!(r.works, 4);
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert_eq!(r.issues[0].plays, 2);
        assert_eq!(r.issues[0].missing, vec!["lyricist".to_string()]);
        let ri = validate(&cfg, "intergram", "2026-11", &all).unwrap();
        assert_eq!(ri.plays, 4, "jingly se INTERGRAMu nehlásí");
        assert_eq!(ri.issues.len(), 1);
        assert_eq!(ri.issues[0].invalid, vec!["isrc".to_string()]);
        let csv = export_text(&cfg, "osa", &all).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(lines[1].starts_with("03.11.2026;08–09;08:15:00;\"Píseň; první\";A;K;L;3:21;201;Supraphon;CZA122600001;hlavní"), "{}", lines[1]);
        assert!(lines[5].contains("znělka") && lines[5].contains("Rádio Test"));
        let (b, enc) = encode(&CmoConfig { encoding: "windows-1250".into(), ..cfg.clone() }, "Píseň");
        assert_eq!(enc, "windows-1250");
        assert_eq!(b.len(), 5);
    }
}
