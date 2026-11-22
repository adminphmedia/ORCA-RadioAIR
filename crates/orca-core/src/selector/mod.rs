//! ORCA Music Selector – profesionální plánování hudby (vedle jednoduchého plánování podle clocků).
//!
//! * `SelectorConfig` – formát stanice: role kategorií, pravidla (tvrdá/měkká), cíle zastoupení, přechody,
//!   hodinová schémata se sloty, týdenní mřížka, denní pásma a datované výjimky. Uloženo jako verzovaný dokument.
//! * [`attrs`] – hudební atributy skladeb (identity interpretů, dílo/verze, jazyk, lokální tvorba, energie, tempo …).
//! * [`generator`] – generátor plánu (logu) dne: projekce, způsobilost, tvrdé filtry, skóre s rozkladem, varianty, diagnostika.
//! * [`logs`] – plány dne s revizemi: koncept → publikováno, úpravy editoru, odbavení automatikou.
//! * [`analytics`] – analytika as-played (stejný výpočet pro rozhraní, export i simulaci).
//! * [`presets`] – katalog formátů (CHR, AC, Classic Hits, Rock, Dance, Jazz, Klasika …) a průvodce.
//!
//! Režimy: `off` (jednoduché plánování), `shadow` (plán se generuje a porovnává, vysílání řídí clocky),
//! `active` (automatika bere hudbu z publikovaného plánu; bez plánu nouzově z clocků).

pub mod analytics;
pub mod attrs;
pub mod generator;
pub mod logs;
pub mod presets;

use crate::db::{now_utc, Db};
use crate::model::TrackKind;
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use attrs::attrs_snapshot;

// ---------------------------------------------------------------------------------------------
// Konfigurace formátu

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SelCategory {
    /// Kategorie knihovny (Track.category).
    pub name: String,
    pub kind: TrackKind,
    /// Role ve formátu: power | current | recurrent | gold | deep | special …
    pub role: String,
    /// Pořadí průchodu (nižší = plní se dřív – kritické skupiny).
    pub pass: u32,
    /// Náhradní kategorie, použije se jen pokud je zadaná (zaznamená se jako „náhrada“).
    pub fallback: String,
    /// Rychlost rotace: fast | normal | slow (ovlivňuje cílový obrat).
    pub rotation: String,
    pub color: String,
}

impl Default for SelCategory {
    fn default() -> Self { Self { name: String::new(), kind: TrackKind::Song, role: "current".into(), pass: 5, fallback: String::new(), rotation: "normal".into(), color: String::new() } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Rules {
    /// Tvrdý minimální odstup téže skladby (min).
    pub song_sep_min: u32,
    /// Cílový (měkký) odstup skladby.
    pub song_target_min: u32,
    pub artist_sep_min: u32,
    pub artist_target_min: u32,
    /// Ochrana verzí téhož díla (radio edit, remix, originál) – společný odstup.
    pub protect_versions: bool,
    pub work_sep_min: u32,
    pub album_sep_min: u32,
    /// Odstup měřený i počtem hudebních pozic (0 = jen čas).
    pub artist_sep_positions: u32,
    /// Ochrana stejné hodiny v předchozích N dnech.
    pub same_hour_days: u32,
    pub same_hour_hard: bool,
    pub max_artist_per_day: u32,
    pub max_song_per_day: u32,
    /// Max. počet stejné hodnoty atributu za sebou (gender: 3, energy: 4 …).
    pub max_in_row: BTreeMap<String, u32>,
    /// Hodiny (místní), kdy jsou zakázané explicitní skladby, např. [[6,22]].
    pub explicit_forbidden: Vec<(u32, u32)>,
    /// Max. skok energie mezi sousedními skladbami (0 = nehlídat).
    pub energy_max_jump: u32,
    /// Ochrana ranního / odpoledního vysílání: stejná skladba nehraje v obou špičkách téhož dne.
    pub protect_drive_times: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            song_sep_min: 180, song_target_min: 240, artist_sep_min: 60, artist_target_min: 90, protect_versions: true, work_sep_min: 180,
            album_sep_min: 0, artist_sep_positions: 0, same_hour_days: 2, same_hour_hard: false, max_artist_per_day: 0, max_song_per_day: 0,
            max_in_row: BTreeMap::new(), explicit_forbidden: vec![], energy_max_jump: 0, protect_drive_times: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Weights { pub rest: f64, pub underplayed: f64, pub goals: f64, pub separation: f64, pub same_hour: f64, pub transitions: f64, pub energy_flow: f64, pub length: f64 }
impl Default for Weights {
    fn default() -> Self { Self { rest: 3.0, underplayed: 2.0, goals: 2.0, separation: 2.0, same_hour: 1.5, transitions: 1.5, energy_flow: 1.0, length: 0.5 } }
}

/// Cíl zastoupení: např. language = cs 30 % ±5 (počet) za den, závazný / měkký.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Goal {
    pub id: String,
    /// language | local | decade | energy | gender | genre | mood | sound | explicit | category | role | custom:<klíč>
    pub attr: String,
    pub value: String,
    pub target_pct: f64,
    pub tolerance_pct: f64,
    /// count | minutes
    pub unit: String,
    /// hour | daypart | day | week
    pub scope: String,
    /// hard | soft
    pub severity: String,
    /// Jen v pásmu (název), prázdné = vždy.
    pub daypart: String,
    pub weight: f64,
}
impl Default for Goal {
    fn default() -> Self { Self { id: String::new(), attr: "language".into(), value: "cs".into(), target_pct: 30.0, tolerance_pct: 5.0, unit: "count".into(), scope: "day".into(), severity: "soft".into(), daypart: String::new(), weight: 1.0 } }
}

/// Přechod mezi sousedními skladbami: např. energy 5 → 1 zakázáno.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Transition { pub attr: String, pub from: String, pub to: String, /// forbidden | undesirable
    pub level: String }

/// Pozice v hodinovém schématu.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Slot {
    /// music | fixed | imaging | spot | vt | news | stitch | note | marker
    pub kind: String,
    pub category: String,
    /// Pevný titul.
    pub track_id: Option<i64>,
    /// Typ knihovny pro imaging (jingle, sweeper …).
    pub track_kind: Option<TrackKind>,
    /// Požadované atributy: {"energy": ">=4", "familiarity": ">=4", "language": "cs"}
    pub attrs: BTreeMap<String, String>,
    pub label: String,
    /// Pevný čas v hodině (minuta) – pevný bod, jinak volná návaznost.
    pub at_min: Option<u32>,
    /// Lze vypustit při přetečení hodiny.
    pub optional: bool,
    /// Sweeper / voice track: pozice přes skladbu.
    pub position: Option<String>,
    /// Stitcher: hooks | time | news
    pub stitch: Option<String>,
    /// Odhad délky zástupné pozice v sekundách (reklama, zprávy, moderace) – pro výpočet průběhu hodiny.
    pub est_s: Option<u32>,
}
impl Default for Slot {
    fn default() -> Self { Self { kind: "music".into(), category: String::new(), track_id: None, track_kind: None, attrs: BTreeMap::new(), label: String::new(), at_min: None, optional: false, position: None, stitch: None, est_s: None } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SelClock { pub name: String, pub color: String, pub slots: Vec<Slot>, pub note: String }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Daypart { pub name: String, pub from_hour: u32, pub to_hour: u32 }

/// Datovaná výjimka mřížky (svátky, akce): od–do (včetně), hodiny [from, to), clock. Vyšší priorita vyhrává, při shodě pozdější v seznamu.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct GridException { pub name: String, pub date_from: String, pub date_to: String, pub from_hour: u32, pub to_hour: u32, pub clock: String, pub priority: i32 }

/// Kvalifikace „spinu“ pro analytiku: hraní se počítá, pokud trvalo aspoň min_ms nebo min_pct délky.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Qualify { pub min_ms: i64, pub min_pct: f64 }
impl Default for Qualify { fn default() -> Self { Self { min_ms: 30_000, min_pct: 0.5 } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SelectorConfig {
    /// off | shadow | active
    pub mode: String,
    pub format_name: String,
    pub preset_id: String,
    pub preset_version: u32,
    pub categories: Vec<SelCategory>,
    pub rules: Rules,
    pub weights: Weights,
    pub goals: Vec<Goal>,
    pub transitions: Vec<Transition>,
    pub clocks: Vec<SelClock>,
    /// "den-hodina" (1=Po … 7=Ne) → název clocku
    pub grid: BTreeMap<String, String>,
    pub default_clock: String,
    pub dayparts: Vec<Daypart>,
    pub exceptions: Vec<GridException>,
    pub seed: u64,
    /// Počet variant obtížných hodin.
    pub variants: u32,
    /// Max. kandidátů na pozici (výkon).
    pub max_candidates: usize,
    pub qualify: Qualify,
    /// Ochranné okno publikace (min) – pozice začínající dřív se nemění.
    pub protect_min: u32,
    /// Den, kterým začíná týden (1 = pondělí).
    pub week_start: u32,
    /// Typ pro analytiku „hudba“.
    pub music_kinds: Vec<TrackKind>,
}

impl Default for SelectorConfig {
    fn default() -> Self {
        Self {
            mode: "off".into(), format_name: String::new(), preset_id: String::new(), preset_version: 0, categories: vec![], rules: Rules::default(),
            weights: Weights::default(), goals: vec![], transitions: vec![], clocks: vec![], grid: BTreeMap::new(), default_clock: String::new(),
            dayparts: vec![
                Daypart { name: "Noc".into(), from_hour: 0, to_hour: 6 }, Daypart { name: "Ráno".into(), from_hour: 6, to_hour: 10 },
                Daypart { name: "Dopoledne".into(), from_hour: 10, to_hour: 15 }, Daypart { name: "Odpoledne".into(), from_hour: 15, to_hour: 19 },
                Daypart { name: "Večer".into(), from_hour: 19, to_hour: 24 },
            ],
            exceptions: vec![], seed: 1, variants: 4, max_candidates: 400, qualify: Qualify::default(), protect_min: 10, week_start: 1,
            music_kinds: vec![TrackKind::Song],
        }
    }
}

impl SelectorConfig {
    pub fn category(&self, name: &str) -> Option<&SelCategory> { self.categories.iter().find(|c| c.name == name) }
    pub fn clock(&self, name: &str) -> Option<&SelClock> { self.clocks.iter().find(|c| c.name == name) }
    pub fn daypart_of(&self, hour: u32) -> Option<&Daypart> {
        self.dayparts.iter().find(|d| if d.from_hour <= d.to_hour { hour >= d.from_hour && hour < d.to_hour } else { hour >= d.from_hour || hour < d.to_hour })
    }
    /// Clock pro místní datum, den v týdnu (1–7) a hodinu – výjimky mají přednost (vyšší priorita, pak pozdější).
    /// Vrací (clock, zdroj) – zdroj je „mřížka“, „výchozí“ nebo název výjimky.
    pub fn clock_for(&self, date: chrono::NaiveDate, weekday: u32, hour: u32) -> Option<(&SelClock, String)> {
        let d = date.format("%Y-%m-%d").to_string();
        let ex = self.exceptions.iter().enumerate()
            .filter(|(_, e)| !e.clock.is_empty() && e.date_from.as_str() <= d.as_str() && d.as_str() <= (if e.date_to.is_empty() { e.date_from.as_str() } else { e.date_to.as_str() }))
            .filter(|(_, e)| { let to = if e.to_hour == 0 { 24 } else { e.to_hour }; hour >= e.from_hour && hour < to })
            .max_by_key(|(i, e)| (e.priority, *i));
        if let Some((_, e)) = ex { if let Some(c) = self.clock(&e.clock) { return Some((c, format!("výjimka: {}", e.name))); } }
        if let Some(n) = self.grid.get(&format!("{weekday}-{hour}")) { if let Some(c) = self.clock(n) { return Some((c, "mřížka".into())); } }
        self.clock(&self.default_clock).or(self.clocks.first()).map(|c| (c, "výchozí".into()))
    }
}

// ---------------------------------------------------------------------------------------------
// Úložiště verzovaných dokumentů (konfigurace selektoru)

pub const CONFIG_DOC: &str = "config";

#[derive(Debug, Clone, Serialize)]
pub struct Versioned<T> { pub version: i64, pub updated_at: String, pub updated_by: String, pub data: T }

pub fn load_config(db: &Db) -> Result<Versioned<SelectorConfig>> {
    let row: Option<(String, i64, String, String)> = db.with_conn(|c| c.query_row("SELECT data, version, updated_at, updated_by FROM sel_docs WHERE id=?1", [CONFIG_DOC], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).ok());
    Ok(match row {
        Some((d, v, at, by)) => Versioned { version: v, updated_at: at, updated_by: by, data: serde_json::from_str(&d).unwrap_or_default() },
        None => Versioned { version: 0, updated_at: String::new(), updated_by: String::new(), data: SelectorConfig::default() },
    })
}

/// Uloží konfiguraci. `base_version` = verze, ze které uživatel vycházel – při souběžné úpravě vrátí chybu (konflikt).
pub fn save_config(db: &Db, cfg: &SelectorConfig, base_version: i64, user: &str) -> Result<i64> {
    let data = serde_json::to_string(cfg)?;
    db.with_conn(|c| -> Result<i64> {
        let tx = c.unchecked_transaction()?;
        let cur: i64 = tx.query_row("SELECT version FROM sel_docs WHERE id=?1", [CONFIG_DOC], |r| r.get(0)).unwrap_or(0);
        if base_version >= 0 && cur != base_version {
            return Err(anyhow!("CONFLICT: konfiguraci mezitím změnil jiný uživatel (verze {cur}, vaše {base_version}). Načtěte ji znovu."));
        }
        let v = cur + 1;
        let now = now_utc();
        tx.execute("INSERT INTO sel_docs(id,data,version,updated_at,updated_by) VALUES(?1,?2,?3,?4,?5)
                    ON CONFLICT(id) DO UPDATE SET data=excluded.data, version=excluded.version, updated_at=excluded.updated_at, updated_by=excluded.updated_by",
            params![CONFIG_DOC, data, v, now, user])?;
        tx.execute("INSERT INTO sel_doc_revisions(doc_id,version,data,at,user) VALUES(?1,?2,?3,?4,?5)", params![CONFIG_DOC, v, data, now, user])?;
        tx.commit()?;
        Ok(v)
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DocRevision { pub version: i64, pub at: String, pub user: String, pub mode: String, pub format_name: String }

pub fn config_revisions(db: &Db) -> Result<Vec<DocRevision>> {
    db.with_conn(|c| -> Result<Vec<DocRevision>> {
        let mut st = c.prepare("SELECT version, at, user, data FROM sel_doc_revisions WHERE doc_id=?1 ORDER BY version DESC LIMIT 100")?;
        let v = st.query_map([CONFIG_DOC], |r| {
            let d: String = r.get(3)?;
            let cfg: SelectorConfig = serde_json::from_str(&d).unwrap_or_default();
            Ok(DocRevision { version: r.get(0)?, at: r.get(1)?, user: r.get(2)?, mode: cfg.mode, format_name: cfg.format_name })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

/// Obnoví dřívější verzi konfiguraci (jako novou verzi – historie zůstává).
pub fn restore_config(db: &Db, version: i64, user: &str) -> Result<i64> {
    let data: String = db.with_conn(|c| c.query_row("SELECT data FROM sel_doc_revisions WHERE doc_id=?1 AND version=?2", params![CONFIG_DOC, version], |r| r.get(0)))?;
    let cfg: SelectorConfig = serde_json::from_str(&data)?;
    save_config(db, &cfg, -1, user)
}

/// Lidsky čitelný popis pravidel (základní režim).
pub fn describe_rules(r: &Rules) -> Vec<String> {
    let mut v = vec![];
    let hm = |m: u32| if m >= 60 && m % 60 == 0 { format!("{} h", m / 60) } else if m >= 60 { format!("{} h {} min", m / 60, m % 60) } else { format!("{m} min") };
    if r.song_sep_min > 0 { v.push(format!("Stejná skladba nejdříve za {} (ideálně za {}).", hm(r.song_sep_min), hm(r.song_target_min.max(r.song_sep_min)))); }
    if r.artist_sep_min > 0 { v.push(format!("Stejný interpret (i jako host) nejdříve za {} (ideálně za {}).", hm(r.artist_sep_min), hm(r.artist_target_min.max(r.artist_sep_min)))); }
    if r.protect_versions { v.push(format!("Jiná verze téže písně (radio edit, remix) nejdříve za {}.", hm(r.work_sep_min))); }
    if r.album_sep_min > 0 { v.push(format!("Stejné album nejdříve za {}.", hm(r.album_sep_min))); }
    if r.same_hour_days > 0 { v.push(format!("Skladba {} hrát ve stejné hodině jako v posledních {} dnech.", if r.same_hour_hard { "nesmí" } else { "by neměla" }, r.same_hour_days)); }
    if r.max_artist_per_day > 0 { v.push(format!("Interpret nejvýše {}× za den.", r.max_artist_per_day)); }
    if r.max_song_per_day > 0 { v.push(format!("Skladba nejvýše {}× za den.", r.max_song_per_day)); }
    for (a, n) in &r.max_in_row { v.push(format!("Nejvýše {n} skladby se stejnou hodnotou „{a}“ za sebou.")); }
    for (f, t) in &r.explicit_forbidden { v.push(format!("Explicitní skladby nehrají mezi {f}:00 a {t}:00.")); }
    if r.energy_max_jump > 0 { v.push(format!("Energie sousedních skladeb se liší nejvýše o {}.", r.energy_max_jump)); }
    if r.protect_drive_times { v.push("Skladba nehraje ráno i odpoledne ve špičce téhož dne.".into()); }
    v
}
