//! Výběr skladeb podle clocků (Main Auto): separace interpreta/skladby, randomness, kompenzace.

use crate::config::{AutoConfig, Clock, ClockStep, Config};
use crate::db::{utc_minus_minutes, Db};
use crate::model::{Track, TrackKind};
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, Timelike};
use rand::seq::SliceRandom;
use std::collections::HashSet;

pub fn grid_key(t: &DateTime<Local>) -> String { format!("{}-{}", t.weekday().number_from_monday(), t.hour()) }

/// Clock platný pro daný čas (týdenní mřížka 7×24, jinak výchozí).
pub fn clock_for<'a>(cfg: &'a Config, t: &DateTime<Local>) -> Option<&'a Clock> {
    let name = cfg.auto.clock_grid.get(&grid_key(t)).unwrap_or(&cfg.auto.default_clock);
    cfg.clock_by_name(name).or_else(|| cfg.auto.clocks.first())
}

pub struct Exclusions {
    pub track_ids: HashSet<i64>,
    pub artists: HashSet<String>,
    /// Čas, pro který se vybírá (platnost souborů); None = teď
    pub at: Option<chrono::NaiveDateTime>,
}

/// Vybere skladbu pro krok clocku. Vrací skladbu a zdůvodnění výběru.
pub fn select(db: &Db, auto: &AutoConfig, step: &ClockStep, backup_category: &str, excl: &Exclusions) -> Result<Option<(Track, String)>> {
    // pevný soubor: vždy tentýž (znělka, ID stanice…) – jen musí existovat, být zapnutý a platný
    if let Some(id) = step.track_id {
        let at = excl.at.unwrap_or_else(|| chrono::Local::now().naive_local());
        return Ok(db.get_track(id)?.filter(|t| t.enabled && !t.missing && t.valid_at(at)).map(|t| (t, "pevný soubor".to_string())));
    }
    let is_song = step.kind == TrackKind::Song;
    let song_since = if is_song { utc_minus_minutes(auto.song_separation_min as i64) } else { "9999".into() };
    let mut artists_recent: HashSet<String> = excl.artists.clone();
    if is_song && auto.artist_separation_min > 0 {
        for a in db.artists_played_since(&utc_minus_minutes(auto.artist_separation_min as i64))? { artists_recent.insert(a); }
    }
    let pick = |cands: Vec<Track>, check_artist: bool| -> Option<Track> {
        let ok: Vec<Track> = cands.into_iter()
            .filter(|t| !excl.track_ids.contains(&t.id))
            .filter(|t| t.valid_at(excl.at.unwrap_or_else(|| chrono::Local::now().naive_local())))
            .filter(|t| !check_artist || t.artist.is_empty() || !artists_recent.contains(&t.artist.to_lowercase()))
            .collect();
        if ok.is_empty() { return None; }
        if auto.randomness == 0 { return ok.into_iter().next(); }
        let n = (auto.randomness as usize).min(ok.len());
        ok[..n].choose(&mut rand::thread_rng()).cloned()
    };
    let cat_label = if step.category.is_empty() { "*".to_string() } else { step.category.clone() };

    // 1) plná pravidla
    let c = db.rotation_candidates(step.kind, &step.category, &song_since, 300)?;
    if let Some(t) = pick(c, is_song) {
        return Ok(Some((t, format!("clock: {} / {}; rotace (nejdéle nehraná); separace splněna", step.kind.as_str(), cat_label))));
    }
    // 2) uvolnit separaci interpreta
    if is_song {
        let c = db.rotation_candidates(step.kind, &step.category, &song_since, 300)?;
        if let Some(t) = pick(c, false) {
            return Ok(Some((t, format!("clock: song / {cat_label}; separace interpreta uvolněna (málo skladeb)"))));
        }
    }
    // 3) uvolnit separaci skladby
    let c = db.rotation_candidates(step.kind, &step.category, "9999", 300)?;
    if let Some(t) = pick(c, false) {
        return Ok(Some((t, format!("clock: {} / {cat_label}; separace skladby uvolněna (málo skladeb)", step.kind.as_str()))));
    }
    // 4) kompenzační kategorie
    if !backup_category.is_empty() && backup_category != step.category {
        let c = db.rotation_candidates(TrackKind::Song, backup_category, "9999", 300)?;
        if let Some(t) = pick(c, false) {
            return Ok(Some((t, format!("kompenzace: kategorie {backup_category} (krok {cat_label} nemá skladby)"))));
        }
    }
    Ok(None)
}
