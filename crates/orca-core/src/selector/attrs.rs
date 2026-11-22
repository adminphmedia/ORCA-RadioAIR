//! Hudební atributy skladeb a identity: interpreti (hlavní + hosté), dílo (společné pro verze), jazyk, lokální tvorba,
//! energie, tempo, nálada, známost, explicitnost, vlastní atributy. Neznámá hodnota (NULL / "") ≠ nula.

use crate::db::{now_utc, Db};
use crate::model::{Track, TrackKind};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct MusicAttrs {
    pub track_id: i64,
    /// Dílo – verze se stejným klíčem sdílí ochranu (prázdné = automaticky z názvu a interpreta).
    pub work_key: String,
    /// Identity interpretů oddělené „|“ (prázdné = automaticky z pole interpret).
    pub artists: String,
    pub language: String,
    pub country: String,
    pub local: Option<bool>,
    pub bpm: Option<f64>,
    /// 1–5
    pub energy: Option<i64>,
    pub mood: String,
    /// 1–5
    pub familiarity: Option<i64>,
    pub explicit: Option<bool>,
    /// male | female | duet | group | instrumental
    pub gender: String,
    /// Zvukový kód (např. A = akustická, E = elektronická)
    pub sound: String,
    pub custom: BTreeMap<String, String>,
    /// Aktivní od / do (YYYY-MM-DD, prázdné = bez omezení) – sezónní nasazení, expirace.
    pub active_from: String,
    pub active_to: String,
    pub paused: bool,
    /// Váha v rámci kategorie (1 = normální).
    pub weight: f64,
    /// Původ hodnot: pole → "manual" | "import" | "suggest:<jistota>"
    pub origin: BTreeMap<String, String>,
    pub updated_at: String,
}

impl MusicAttrs {
    pub fn new(track_id: i64) -> Self { Self { track_id, weight: 1.0, ..Default::default() } }
}

fn strip(s: &str) -> String {
    crate::import::norm(s)
}

/// Identity interpretů: „A feat. B & C“ → ["a", "b", "c"]. Normalizace bez slučování různých jmen (jen malá písmena a diakritika).
pub fn artist_ids(artist: &str, title: &str) -> Vec<String> {
    let mut s = format!(" {} ", artist.to_lowercase());
    // host v názvu: „Song (feat. X)“
    let tl = title.to_lowercase();
    for key in ["feat.", "feat ", "ft.", "featuring "] {
        if let Some(i) = tl.find(key) {
            let rest = &tl[i + key.len()..];
            let end = rest.find([')', ']']).unwrap_or(rest.len());
            s.push_str(" & ");
            s.push_str(&rest[..end]);
        }
    }
    for sep in [" feat. ", " feat ", " ft. ", " ft ", " featuring ", " & ", " x ", " vs. ", " vs ", " + ", " with ", ", ", " and ", "; ", " / "] {
        s = s.replace(sep, "|");
    }
    let mut v: Vec<String> = s.split('|').map(strip).filter(|x| !x.is_empty()).collect();
    v.dedup();
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
}

/// Klíč díla: název bez označení verze (Radio Edit, Remix, Live …) + hlavní interpret.
pub fn work_key(artist: &str, title: &str) -> String {
    let mut t = title.to_lowercase();
    // odstranit závorky s označením verze
    let markers = ["edit", "remix", "mix", "version", "verze", "live", "acoustic", "akustick", "radio", "remaster", "feat", "ft.", "instrumental", "extended", "clean", "explicit", "rework", "vip", "single", "album", "mono", "stereo"];
    loop {
        let open = t.find(['(', '[']);
        let Some(o) = open else { break };
        let close = t[o..].find([')', ']']).map(|c| o + c).unwrap_or(t.len().saturating_sub(1));
        let inner = t[o..=close.min(t.len() - 1)].to_string();
        if markers.iter().any(|m| inner.contains(m)) { t.replace_range(o..=close.min(t.len() - 1), ""); } else { t.replace_range(o..o + 1, " "); }
    }
    for suf in [" - radio edit", " - remix", " - live", " - remastered", " - acoustic", " - edit", " - single version", " - extended"] {
        if let Some(i) = t.find(suf) { t.truncate(i); }
    }
    let main = artist_ids(artist, "").into_iter().next().unwrap_or_default();
    format!("{}|{}", main, strip(&t))
}

pub fn get(db: &Db, track_id: i64) -> Result<Option<MusicAttrs>> {
    db.with_conn(|c| -> Result<Option<MusicAttrs>> {
        Ok(c.query_row("SELECT track_id,work_key,artists,language,country,local,bpm,energy,mood,familiarity,explicit,gender,sound,custom,active_from,active_to,paused,weight,origin,updated_at FROM music_attrs WHERE track_id=?1", [track_id], row).optional()?)
    })
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<MusicAttrs> {
    Ok(MusicAttrs {
        track_id: r.get(0)?, work_key: r.get(1)?, artists: r.get(2)?, language: r.get(3)?, country: r.get(4)?,
        local: r.get::<_, Option<i64>>(5)?.map(|v| v != 0), bpm: r.get(6)?, energy: r.get(7)?, mood: r.get(8)?, familiarity: r.get(9)?,
        explicit: r.get::<_, Option<i64>>(10)?.map(|v| v != 0), gender: r.get(11)?, sound: r.get(12)?,
        custom: serde_json::from_str(&r.get::<_, String>(13)?).unwrap_or_default(), active_from: r.get(14)?, active_to: r.get(15)?,
        paused: r.get::<_, i64>(16)? != 0, weight: r.get(17)?, origin: serde_json::from_str(&r.get::<_, String>(18)?).unwrap_or_default(), updated_at: r.get(19)?,
    })
}

pub fn all(db: &Db) -> Result<HashMap<i64, MusicAttrs>> {
    db.with_conn(|c| -> Result<HashMap<i64, MusicAttrs>> {
        let mut st = c.prepare("SELECT track_id,work_key,artists,language,country,local,bpm,energy,mood,familiarity,explicit,gender,sound,custom,active_from,active_to,paused,weight,origin,updated_at FROM music_attrs")?;
        let v = st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v.into_iter().map(|a| (a.track_id, a)).collect())
    })
}

pub fn save(db: &Db, a: &MusicAttrs) -> Result<()> {
    db.with_conn(|c| -> Result<()> {
        c.execute("INSERT INTO music_attrs(track_id,work_key,artists,language,country,local,bpm,energy,mood,familiarity,explicit,gender,sound,custom,active_from,active_to,paused,weight,origin,updated_at)
                   VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
                   ON CONFLICT(track_id) DO UPDATE SET work_key=excluded.work_key, artists=excluded.artists, language=excluded.language, country=excluded.country, local=excluded.local,
                   bpm=excluded.bpm, energy=excluded.energy, mood=excluded.mood, familiarity=excluded.familiarity, explicit=excluded.explicit, gender=excluded.gender, sound=excluded.sound,
                   custom=excluded.custom, active_from=excluded.active_from, active_to=excluded.active_to, paused=excluded.paused, weight=excluded.weight, origin=excluded.origin, updated_at=excluded.updated_at",
            params![a.track_id, a.work_key, a.artists, a.language, a.country, a.local.map(|b| b as i64), a.bpm, a.energy, a.mood, a.familiarity, a.explicit.map(|b| b as i64),
                    a.gender, a.sound, serde_json::to_string(&a.custom)?, a.active_from, a.active_to, a.paused as i64, a.weight, serde_json::to_string(&a.origin)?, now_utc()])?;
        Ok(())
    })
}

/// Hromadná změna: nastaví vybraná pole u více skladeb. `set` = pole → hodnota ("" / "null" = neznámé). Vrací počet změněných.
pub fn bulk_set(db: &Db, ids: &[i64], set: &BTreeMap<String, String>, dry_run: bool) -> Result<Vec<(i64, String, String, String)>> {
    let mut changes = vec![];
    for &id in ids {
        let mut a = get(db, id)?.unwrap_or_else(|| MusicAttrs::new(id));
        for (k, v) in set {
            let before = field_value(&a, k);
            set_field(&mut a, k, v);
            let after = field_value(&a, k);
            if before != after { changes.push((id, k.clone(), before, after)); a.origin.insert(k.clone(), "manual".into()); }
        }
        if !dry_run { save(db, &a)?; }
    }
    Ok(changes)
}

pub fn field_value(a: &MusicAttrs, k: &str) -> String {
    let o = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_default();
    let b = |v: Option<bool>| v.map(|x| if x { "1".to_string() } else { "0".into() }).unwrap_or_default();
    match k {
        "work_key" => a.work_key.clone(), "artists" => a.artists.clone(), "language" => a.language.clone(), "country" => a.country.clone(),
        "local" => b(a.local), "bpm" => a.bpm.map(|x| format!("{x}")).unwrap_or_default(), "energy" => o(a.energy), "mood" => a.mood.clone(),
        "familiarity" => o(a.familiarity), "explicit" => b(a.explicit), "gender" => a.gender.clone(), "sound" => a.sound.clone(),
        "active_from" => a.active_from.clone(), "active_to" => a.active_to.clone(), "paused" => (a.paused as i64).to_string(), "weight" => format!("{}", a.weight),
        k if k.starts_with("custom:") => a.custom.get(&k[7..]).cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

pub fn set_field(a: &mut MusicAttrs, k: &str, v: &str) {
    let v = v.trim();
    let unknown = v.is_empty() || v.eq_ignore_ascii_case("null");
    let int = || if unknown { None } else { v.parse::<i64>().ok() };
    let bool_ = || if unknown { None } else { Some(matches!(v.to_lowercase().as_str(), "1" | "true" | "ano" | "yes" | "a")) };
    match k {
        "work_key" => a.work_key = v.into(), "artists" => a.artists = v.into(), "language" => a.language = v.to_lowercase(), "country" => a.country = v.to_uppercase(),
        "local" => a.local = bool_(), "bpm" => a.bpm = if unknown { None } else { v.replace(',', ".").parse().ok() }, "energy" => a.energy = int().map(|x| x.clamp(1, 5)),
        "mood" => a.mood = v.into(), "familiarity" => a.familiarity = int().map(|x| x.clamp(1, 5)), "explicit" => a.explicit = bool_(), "gender" => a.gender = v.to_lowercase(),
        "sound" => a.sound = v.into(), "active_from" => a.active_from = v.into(), "active_to" => a.active_to = v.into(), "paused" => a.paused = bool_().unwrap_or(false),
        "weight" => a.weight = v.replace(',', ".").parse().unwrap_or(1.0),
        k if k.starts_with("custom:") => { if unknown { a.custom.remove(&k[7..]); } else { a.custom.insert(k[7..].into(), v.into()); } }
        _ => {}
    }
}

/// Návrhy hodnot (nepřepisují potvrzené ruční údaje): dekáda z roku je odvozená vždy, jazyk podle diakritiky, pohlaví podle pole Vocal.
pub fn suggest(t: &Track, a: &MusicAttrs) -> BTreeMap<String, (String, f64)> {
    let mut s = BTreeMap::new();
    let text = format!("{} {}", t.artist, t.title);
    if a.language.is_empty() {
        if text.chars().any(|c| "ěščřžůťďň".contains(c.to_ascii_lowercase()) || "ĚŠČŘŽŮŤĎŇ".contains(c)) { s.insert("language".into(), ("cs".into(), 0.7)); }
        else if text.chars().any(|c| "ľĺŕô".contains(c)) { s.insert("language".into(), ("sk".into(), 0.6)); }
    }
    if a.gender.is_empty() && !t.vocal.is_empty() {
        let v = t.vocal.to_lowercase();
        let g = if v.contains("žen") || v.contains("fem") || v == "f" { "female" } else if v.contains("muž") || v.contains("male") || v == "m" { "male" } else if v.contains("duet") { "duet" } else if v.contains("instr") { "instrumental" } else { "" };
        if !g.is_empty() { s.insert("gender".into(), (g.into(), 0.9)); }
    }
    if a.local.is_none() && (t.category2.eq_ignore_ascii_case("cz") || t.category.to_lowercase().contains(" cz") || t.category.to_lowercase().starts_with("cz")) {
        s.insert("local".into(), ("1".into(), 0.6));
    }
    s
}

/// Uplatní návrhy u všech skladeb, kde chybí hodnota (označí původ „suggest“). Vrací počet změn.
pub fn apply_suggestions(db: &Db, kinds: &[TrackKind]) -> Result<usize> {
    let attrs = all(db)?;
    let mut n = 0;
    for k in kinds {
        let (tracks, _) = db.list_tracks(&crate::db::TrackFilter { kind: Some(k.as_str().into()), include_disabled: Some(true), limit: Some(1_000_000), ..Default::default() })?;
        for t in tracks {
            let mut a = attrs.get(&t.id).cloned().unwrap_or_else(|| MusicAttrs::new(t.id));
            let sug = suggest(&t, &a);
            if sug.is_empty() { continue; }
            for (k, (v, conf)) in sug { set_field(&mut a, &k, &v); a.origin.insert(k, format!("suggest:{conf:.1}")); n += 1; }
            save(db, &a)?;
        }
    }
    Ok(n)
}

/// Snímek atributů pro as-played (historie si pamatuje stav v čase hraní).
pub fn attrs_snapshot(db: &Db, track_id: i64) -> Result<String> {
    let Some(a) = get(db, track_id)? else { return Ok(String::new()) };
    let mut m = serde_json::Map::new();
    for k in ["language", "local", "energy", "bpm", "gender", "mood", "familiarity", "explicit", "sound"] {
        let v = field_value(&a, k);
        if !v.is_empty() { m.insert(k.into(), v.into()); }
    }
    Ok(serde_json::Value::Object(m).to_string())
}

/// Návrh duplicit: stejné dílo, stejný hlavní interpret a podobná délka (±3 s). Uživatel potvrdí.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateGroup { pub work_key: String, pub tracks: Vec<(i64, String, String, String, i64)> }

pub fn duplicates(db: &Db) -> Result<Vec<DuplicateGroup>> {
    let (tracks, _) = db.list_tracks(&crate::db::TrackFilter { kind: Some("song".into()), include_disabled: Some(true), limit: Some(1_000_000), ..Default::default() })?;
    let attrs = all(db)?;
    let mut groups: BTreeMap<String, Vec<&Track>> = BTreeMap::new();
    for t in &tracks {
        let wk = attrs.get(&t.id).map(|a| a.work_key.clone()).filter(|w| !w.is_empty()).unwrap_or_else(|| work_key(&t.artist, &t.title));
        groups.entry(wk).or_default().push(t);
    }
    Ok(groups.into_iter().filter(|(_, v)| v.len() > 1).map(|(k, v)| DuplicateGroup {
        work_key: k, tracks: v.into_iter().map(|t| (t.id, t.artist.clone(), t.title.clone(), t.category.clone(), t.duration_ms)).collect(),
    }).collect())
}

// ---------------------------------------------------------------------------------------------
// Skladba pro plánování (snímek knihovny + atributy)

#[derive(Debug, Clone, Serialize)]
pub struct SelTrack {
    pub id: i64,
    pub kind: TrackKind,
    pub category: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub artists: Vec<String>,
    pub work: String,
    pub year: Option<i32>,
    pub genre: String,
    /// Délka, o kterou skladba posune vysílání (cue in → mix).
    pub len_ms: i64,
    pub intro_ms: i64,
    pub enabled: bool,
    pub attrs: MusicAttrs,
}

impl SelTrack {
    pub fn from(t: &Track, a: Option<&MusicAttrs>) -> Self {
        let a = a.cloned().unwrap_or_else(|| MusicAttrs::new(t.id));
        let artists = if a.artists.trim().is_empty() { artist_ids(&t.artist, &t.title) } else { a.artists.split('|').map(strip).filter(|x| !x.is_empty()).collect() };
        let work = if a.work_key.trim().is_empty() { work_key(&t.artist, &t.title) } else { a.work_key.clone() };
        let (ci, mix, _) = t.effective_cues();
        Self {
            id: t.id, kind: t.kind(), category: t.category.clone(), artist: t.artist.clone(), title: t.title.clone(), album: t.album.clone(), artists, work,
            year: t.year, genre: t.genre.clone(), len_ms: (mix - ci).max(0), intro_ms: (t.intro_ms - ci).max(0),
            enabled: t.enabled && !t.missing && t.analyzed && t.duration_ms > 0, attrs: a,
        }
    }

    /// Hodnota atributu pro pravidla a cíle; None = neznámá.
    pub fn attr(&self, key: &str) -> Option<String> {
        let v = match key {
            "decade" => self.year.map(|y| format!("{}", y / 10 * 10)),
            "genre" => Some(self.genre.to_lowercase()).filter(|s| !s.is_empty()),
            "category" => Some(self.category.clone()),
            "artist" => self.artists.first().cloned(),
            k => Some(field_value(&self.attrs, k)).filter(|s| !s.is_empty()),
        };
        v
    }

    /// Aktivní k danému dni (platnost, pozastavení).
    pub fn active_on(&self, day: &str) -> bool {
        if self.attrs.paused || !self.enabled { return false; }
        if !self.attrs.active_from.is_empty() && day < self.attrs.active_from.as_str() { return false; }
        if !self.attrs.active_to.is_empty() && day > self.attrs.active_to.as_str() { return false; }
        true
    }
}

/// Načte všechny skladby daných typů se atributy.
pub fn load_tracks(db: &Db, kinds: &[TrackKind]) -> Result<Vec<SelTrack>> {
    let attrs = all(db)?;
    let mut v = vec![];
    for k in kinds {
        let (tracks, _) = db.list_tracks(&crate::db::TrackFilter { kind: Some(k.as_str().into()), include_disabled: Some(true), include_missing: Some(true), limit: Some(1_000_000), ..Default::default() })?;
        v.extend(tracks.iter().map(|t| SelTrack::from(t, attrs.get(&t.id))));
    }
    Ok(v)
}

/// Porovná hodnotu atributu s požadavkem slotu: "cs", ">=4", "<=2", "!x", "a|b". None = neznámá hodnota.
pub fn matches(val: Option<&str>, req: &str) -> Option<bool> {
    let v = val?;
    let req = req.trim();
    if let Some(r) = req.strip_prefix(">=") { return Some(v.parse::<f64>().ok()? >= r.trim().parse::<f64>().ok()?); }
    if let Some(r) = req.strip_prefix("<=") { return Some(v.parse::<f64>().ok()? <= r.trim().parse::<f64>().ok()?); }
    if let Some(r) = req.strip_prefix('!') { return Some(!r.split('|').any(|x| x.trim().eq_ignore_ascii_case(v))); }
    Some(req.split('|').any(|x| x.trim().eq_ignore_ascii_case(v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artists_and_works() {
        assert_eq!(artist_ids("David Guetta feat. Sia", ""), vec!["davidguetta", "sia"]);
        assert_eq!(artist_ids("Calvin Harris", "Feel So Close (feat. Rihanna)"), vec!["calvinharris", "rihanna"]);
        assert_eq!(artist_ids("Kabát", ""), vec!["kabat"]);
        assert_eq!(work_key("Dua Lipa", "Levitating (Radio Edit)"), work_key("Dua Lipa feat. DaBaby", "Levitating - Remix"));
        assert_ne!(work_key("Dua Lipa", "Levitating"), work_key("Dua Lipa", "Physical"));
        assert_eq!(matches(Some("4"), ">=4"), Some(true));
        assert_eq!(matches(Some("cs"), "cs|sk"), Some(true));
        assert_eq!(matches(None, "cs"), None);
    }
}
