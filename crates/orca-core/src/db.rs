//! SQLite databáze (jeden soubor `orca.db`, režim WAL).

use crate::model::{LogEntry, Track, TrackKind};
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub fn now_utc() -> String { chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string() }

pub fn utc_minus_minutes(min: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(min)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE tracks(
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL,
        path TEXT NOT NULL UNIQUE,
        filename TEXT NOT NULL,
        artist TEXT NOT NULL DEFAULT '',
        title TEXT NOT NULL DEFAULT '',
        album TEXT NOT NULL DEFAULT '',
        year INTEGER,
        genre TEXT NOT NULL DEFAULT '',
        category TEXT NOT NULL DEFAULT '',
        category2 TEXT NOT NULL DEFAULT '',
        category3 TEXT NOT NULL DEFAULT '',
        duration_ms INTEGER NOT NULL DEFAULT 0,
        cue_in_ms INTEGER NOT NULL DEFAULT 0,
        intro_ms INTEGER NOT NULL DEFAULT 0,
        mix_ms INTEGER NOT NULL DEFAULT 0,
        cue_out_ms INTEGER NOT NULL DEFAULT 0,
        hook_in_ms INTEGER,
        hook_out_ms INTEGER,
        gain_db REAL NOT NULL DEFAULT 0,
        lufs REAL,
        analyzed INTEGER NOT NULL DEFAULT 0,
        manual_cues INTEGER NOT NULL DEFAULT 0,
        enabled INTEGER NOT NULL DEFAULT 1,
        missing INTEGER NOT NULL DEFAULT 0,
        playlister_code TEXT,
        isrc TEXT NOT NULL DEFAULT '',
        composer TEXT NOT NULL DEFAULT '',
        publisher TEXT NOT NULL DEFAULT '',
        file_mtime INTEGER NOT NULL DEFAULT 0,
        file_size INTEGER NOT NULL DEFAULT 0,
        added_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        last_played_at TEXT,
        play_count INTEGER NOT NULL DEFAULT 0,
        rotation_seq INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX ix_tracks_kind_cat ON tracks(kind, category);
    CREATE INDEX ix_tracks_artist ON tracks(artist);
    CREATE TABLE play_log(
        id INTEGER PRIMARY KEY,
        track_id INTEGER,
        kind TEXT NOT NULL,
        artist TEXT NOT NULL DEFAULT '',
        title TEXT NOT NULL DEFAULT '',
        album TEXT NOT NULL DEFAULT '',
        path TEXT NOT NULL DEFAULT '',
        category TEXT NOT NULL DEFAULT '',
        isrc TEXT NOT NULL DEFAULT '',
        composer TEXT NOT NULL DEFAULT '',
        publisher TEXT NOT NULL DEFAULT '',
        duration_ms INTEGER NOT NULL DEFAULT 0,
        started_at TEXT NOT NULL,
        ended_at TEXT,
        played_ms INTEGER,
        source TEXT NOT NULL DEFAULT '',
        reason TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX ix_log_started ON play_log(started_at);
    CREATE INDEX ix_log_artist ON play_log(artist, started_at);
    CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
    "#,
    // v2 – kompletní knihovna (kategorie, memos, palety, kampaně, bloky)
    r#"
    ALTER TABLE tracks ADD COLUMN vocal TEXT NOT NULL DEFAULT '';
    ALTER TABLE tracks ADD COLUMN properties TEXT NOT NULL DEFAULT '';
    ALTER TABLE tracks ADD COLUMN color TEXT NOT NULL DEFAULT '';
    ALTER TABLE tracks ADD COLUMN comment TEXT NOT NULL DEFAULT '';
    ALTER TABLE tracks ADD COLUMN separation_min INTEGER;
    ALTER TABLE tracks ADD COLUMN cover_path TEXT;
    ALTER TABLE tracks ADD COLUMN bookmarked INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX ix_tracks_code ON tracks(playlister_code);
    CREATE TABLE categories(
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL,
        level TEXT NOT NULL DEFAULT 'category',
        name TEXT NOT NULL,
        color TEXT NOT NULL DEFAULT '',
        comment TEXT NOT NULL DEFAULT '',
        sort INTEGER NOT NULL DEFAULT 0,
        UNIQUE(kind, level, name)
    );
    CREATE TABLE memos(
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL DEFAULT '',
        body TEXT NOT NULL DEFAULT '',
        category TEXT NOT NULL DEFAULT '',
        color TEXT NOT NULL DEFAULT '',
        pinned INTEGER NOT NULL DEFAULT 0,
        playlister_code TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE palettes(
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        rows INTEGER NOT NULL DEFAULT 4,
        cols INTEGER NOT NULL DEFAULT 6,
        pages INTEGER NOT NULL DEFAULT 1,
        sort INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE pads(
        palette_id INTEGER NOT NULL REFERENCES palettes(id) ON DELETE CASCADE,
        page INTEGER NOT NULL,
        idx INTEGER NOT NULL,
        track_id INTEGER,
        label TEXT NOT NULL DEFAULT '',
        color TEXT NOT NULL DEFAULT '',
        PRIMARY KEY(palette_id, page, idx)
    );
    CREATE TABLE campaigns(
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        client TEXT NOT NULL DEFAULT '',
        category TEXT NOT NULL DEFAULT '',
        is_commercial INTEGER NOT NULL DEFAULT 1,
        start_at TEXT,
        expires_at TEXT,
        mode TEXT NOT NULL DEFAULT 'weekly',
        system TEXT NOT NULL DEFAULT 'order',
        priority INTEGER NOT NULL DEFAULT 4,
        enabled INTEGER NOT NULL DEFAULT 1,
        notes TEXT NOT NULL DEFAULT '',
        rr_index INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE campaign_spots(
        campaign_id INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
        track_id INTEGER NOT NULL,
        ord INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(campaign_id, track_id)
    );
    CREATE TABLE campaign_slots(
        campaign_id INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
        weekday INTEGER,
        date TEXT,
        break_time TEXT NOT NULL,
        spot_track_id INTEGER
    );
    CREATE INDEX ix_slots_w ON campaign_slots(weekday, break_time);
    CREATE INDEX ix_slots_d ON campaign_slots(date, break_time);
    CREATE TABLE break_playlist(
        date TEXT NOT NULL,
        break_time TEXT NOT NULL,
        seq INTEGER NOT NULL,
        track_id INTEGER,
        code TEXT NOT NULL DEFAULT '',
        title TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX ix_bpl ON break_playlist(date, break_time);
    CREATE TABLE breaks_inserted(date TEXT NOT NULL, break_time TEXT NOT NULL, inserted_at TEXT NOT NULL, PRIMARY KEY(date, break_time));
    CREATE TABLE spot_presets(id INTEGER PRIMARY KEY, name TEXT NOT NULL, slots TEXT NOT NULL DEFAULT '[]');
    "#,
    // v3 – plánování: pořady, moderátoři, playlisty, programová mřížka, události, epizody, plán vysílání
    r#"
    CREATE TABLE sched_docs(
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL,
        name TEXT NOT NULL DEFAULT '',
        data TEXT NOT NULL DEFAULT '{}',
        updated_at TEXT NOT NULL
    );
    CREATE INDEX ix_sched_docs ON sched_docs(kind);
    CREATE TABLE episodes(
        id INTEGER PRIMARY KEY,
        show_id INTEGER NOT NULL,
        num INTEGER NOT NULL DEFAULT 0,
        title TEXT NOT NULL DEFAULT '',
        description TEXT NOT NULL DEFAULT '',
        parts TEXT NOT NULL DEFAULT '[]',
        status TEXT NOT NULL DEFAULT 'ready',
        air_date TEXT,
        created_at TEXT NOT NULL,
        play_count INTEGER NOT NULL DEFAULT 0,
        last_aired TEXT
    );
    CREATE INDEX ix_episodes_show ON episodes(show_id, num);
    CREATE TABLE plan_items(
        id INTEGER PRIMARY KEY,
        date TEXT NOT NULL,
        hour INTEGER NOT NULL,
        pos INTEGER NOT NULL,
        item TEXT NOT NULL,
        locked INTEGER NOT NULL DEFAULT 0,
        state TEXT NOT NULL DEFAULT 'planned'
    );
    CREATE INDEX ix_plan ON plan_items(date, hour, pos);
    CREATE TABLE sched_fired(key TEXT PRIMARY KEY, fired_at TEXT NOT NULL);
    "#,
    // v4 – vysílací processing: presety, revize, auditní log
    r#"
    CREATE TABLE proc_presets(id TEXT PRIMARY KEY, data TEXT NOT NULL, updated_at TEXT NOT NULL);
    CREATE TABLE proc_revisions(
        id INTEGER PRIMARY KEY,
        created_at TEXT NOT NULL,
        author TEXT NOT NULL DEFAULT '',
        note TEXT NOT NULL DEFAULT '',
        data TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'archived'
    );
    CREATE TABLE audit_log(id INTEGER PRIMARY KEY, at TEXT NOT NULL, user TEXT NOT NULL DEFAULT '', action TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '');
    "#,
];

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrackFilter {
    pub kind: Option<String>,
    pub category: Option<String>,
    pub q: Option<String>,
    pub include_disabled: Option<bool>,
    pub include_missing: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// "artist", "title", "added", "played", "duration", "category"
    pub sort: Option<String>,
    pub category2: Option<String>,
    pub category3: Option<String>,
    pub vocal: Option<String>,
    /// Obsahuje štítek (properties)
    pub property: Option<String>,
    pub bookmarked: Option<bool>,
    pub analyzed: Option<bool>,
    /// "asc" | "desc"
    pub dir: Option<String>,
}

/// Data z načtení souboru (tagy).
#[derive(Debug, Clone, Default)]
pub struct ScannedFile {
    pub kind: TrackKind,
    pub path: String,
    pub filename: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub year: Option<i32>,
    pub genre: String,
    pub category: String,
    pub duration_ms: i64,
    pub isrc: String,
    pub composer: String,
    pub publisher: String,
    pub file_mtime: i64,
    pub file_size: i64,
}

impl Default for TrackKind { fn default() -> Self { TrackKind::Song } }

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TrackPatch {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub category: Option<String>,
    pub category2: Option<String>,
    pub category3: Option<String>,
    pub cue_in_ms: Option<i64>,
    pub intro_ms: Option<i64>,
    pub mix_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub hook_in_ms: Option<i64>,
    pub hook_out_ms: Option<i64>,
    pub gain_db: Option<f64>,
    pub enabled: Option<bool>,
    pub playlister_code: Option<String>,
    pub isrc: Option<String>,
    pub composer: Option<String>,
    pub publisher: Option<String>,
    pub vocal: Option<String>,
    pub properties: Option<String>,
    pub color: Option<String>,
    pub comment: Option<String>,
    /// Some(None) = zrušit vlastní rozestup
    #[serde(default, deserialize_with = "double_option")]
    pub separation_min: Option<Option<i64>>,
    pub bookmarked: Option<bool>,
}

/// Rozliší chybějící pole (None) od explicitního null (Some(None)).
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where D: serde::Deserializer<'de>, T: Deserialize<'de> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryCount { pub kind: String, pub category: String, pub count: i64 }

pub const TRACK_COLS: &str = "id,kind,path,filename,artist,title,album,year,genre,category,category2,category3,duration_ms,cue_in_ms,intro_ms,mix_ms,cue_out_ms,hook_in_ms,hook_out_ms,gain_db,lufs,analyzed,manual_cues,enabled,missing,playlister_code,isrc,composer,publisher,added_at,updated_at,last_played_at,play_count,vocal,properties,color,comment,separation_min,cover_path,bookmarked";

pub fn row_track(r: &Row) -> rusqlite::Result<Track> {
    let kind: String = r.get(1)?;
    Ok(Track {
        id: r.get(0)?,
        kind: TrackKind::parse(&kind),
        path: r.get(2)?,
        filename: r.get(3)?,
        artist: r.get(4)?,
        title: r.get(5)?,
        album: r.get(6)?,
        year: r.get(7)?,
        genre: r.get(8)?,
        category: r.get(9)?,
        category2: r.get(10)?,
        category3: r.get(11)?,
        duration_ms: r.get(12)?,
        cue_in_ms: r.get(13)?,
        intro_ms: r.get(14)?,
        mix_ms: r.get(15)?,
        cue_out_ms: r.get(16)?,
        hook_in_ms: r.get(17)?,
        hook_out_ms: r.get(18)?,
        gain_db: r.get(19)?,
        lufs: r.get(20)?,
        analyzed: r.get::<_, i64>(21)? != 0,
        manual_cues: r.get::<_, i64>(22)? != 0,
        enabled: r.get::<_, i64>(23)? != 0,
        missing: r.get::<_, i64>(24)? != 0,
        playlister_code: r.get(25)?,
        isrc: r.get(26)?,
        composer: r.get(27)?,
        publisher: r.get(28)?,
        added_at: r.get(29)?,
        updated_at: r.get(30)?,
        last_played_at: r.get(31)?,
        play_count: r.get(32)?,
        vocal: r.get(33)?,
        properties: r.get(34)?,
        color: r.get(35)?,
        comment: r.get(36)?,
        separation_min: r.get(37)?,
        cover_path: r.get(38)?,
        bookmarked: r.get::<_, i64>(39)? != 0,
    })
}

pub fn row_log(r: &Row) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        id: r.get(0)?,
        track_id: r.get(1)?,
        kind: r.get(2)?,
        artist: r.get(3)?,
        title: r.get(4)?,
        album: r.get(5)?,
        path: r.get(6)?,
        category: r.get(7)?,
        isrc: r.get(8)?,
        composer: r.get(9)?,
        publisher: r.get(10)?,
        duration_ms: r.get(11)?,
        started_at: r.get(12)?,
        ended_at: r.get(13)?,
        played_ms: r.get(14)?,
        source: r.get(15)?,
        reason: r.get(16)?,
    })
}

pub const LOG_COLS: &str = "id,track_id,kind,artist,title,album,path,category,isrc,composer,publisher,duration_ms,started_at,ended_at,played_ms,source,reason";

pub enum UpsertResult { Inserted(i64), Updated(i64), Unchanged(i64) }

impl Db {
    /// Přímý přístup ke spojení (pro moduly katalog, spoty …).
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let c = self.conn.lock();
        f(&c)
    }

    pub fn delete_track(&self, id: i64) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM tracks WHERE id=?1", [id])?;
        c.execute("DELETE FROM campaign_spots WHERE track_id=?1", [id])?;
        c.execute("UPDATE pads SET track_id=NULL WHERE track_id=?1", [id])?;
        Ok(())
    }

    pub fn set_cover(&self, id: i64, path: Option<&str>) -> Result<()> {
        self.conn.lock().execute("UPDATE tracks SET cover_path=?2, updated_at=?3 WHERE id=?1", params![id, path, now_utc()])?;
        Ok(())
    }

    pub fn track_by_code(&self, code: &str, prefixes: &std::collections::BTreeMap<TrackKind, String>) -> Result<Option<Track>> {
        let c = self.conn.lock();
        if let Some(t) = c.query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE playlister_code=?1"), [code], row_track).optional()? {
            return Ok(Some(t));
        }
        // automatický kód = prefix + id
        for (kind, pre) in prefixes {
            if let Some(rest) = code.strip_prefix(pre.as_str()) {
                if let Ok(id) = rest.trim_start_matches('0').parse::<i64>() {
                    if let Some(t) = c.query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE id=?1 AND kind=?2"), params![id, kind.as_str()], row_track).optional()? {
                        return Ok(Some(t));
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn plays_by_track(&self, from: &str, to: &str, kind: Option<&str>, limit: i64) -> Result<Vec<serde_json::Value>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT track_id, max(kind), max(artist), max(title), max(category), COUNT(*), max(started_at) FROM play_log
             WHERE started_at >= ?1 AND started_at < ?2 AND (?3 IS NULL OR kind=?3) AND track_id IS NOT NULL
             GROUP BY track_id ORDER BY COUNT(*) DESC LIMIT ?4")?;
        let v = st.query_map(params![from, to, kind, limit], |r| Ok(serde_json::json!({
            "track_id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "artist": r.get::<_, String>(2)?, "title": r.get::<_, String>(3)?,
            "category": r.get::<_, String>(4)?, "plays": r.get::<_, i64>(5)?, "last_played": r.get::<_, String>(6)?,
        })))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn plays_of_track(&self, track_id: i64, limit: i64) -> Result<Vec<LogEntry>> {
        let c = self.conn.lock();
        let mut st = c.prepare(&format!("SELECT {LOG_COLS} FROM play_log WHERE track_id=?1 ORDER BY id DESC LIMIT ?2"))?;
        let v = st.query_map(params![track_id, limit], row_log)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> { Self::init(Connection::open_in_memory()?) }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let ver: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(ver as usize) {
            conn.execute_batch(m)?;
            conn.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn track_by_path(&self, path: &str) -> Result<Option<(i64, i64, i64, bool)>> {
        let c = self.conn.lock();
        Ok(c.query_row("SELECT id,file_mtime,file_size,missing FROM tracks WHERE path=?1", [path], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
        }).optional()?)
    }

    /// Vloží nebo aktualizuje soubor z knihovny. Ruční úpravy (cue body, kategorie ručně změněné) zůstávají.
    pub fn upsert_scanned(&self, f: &ScannedFile) -> Result<UpsertResult> {
        let existing = self.track_by_path(&f.path)?;
        let now = now_utc();
        let c = self.conn.lock();
        match existing {
            Some((id, mtime, size, missing)) if mtime == f.file_mtime && size == f.file_size => {
                if missing {
                    c.execute("UPDATE tracks SET missing=0, updated_at=?2 WHERE id=?1", params![id, now])?;
                    return Ok(UpsertResult::Updated(id));
                }
                Ok(UpsertResult::Unchanged(id))
            }
            Some((id, _, _, _)) => {
                // Soubor se změnil: obnovit tagy a nechat znovu analyzovat (pokud nejsou ruční cue body).
                c.execute(
                    "UPDATE tracks SET kind=?2, artist=?3, title=?4, album=?5, year=?6, genre=?7, duration_ms=?8, isrc=?9, composer=?10, publisher=?11,
                     file_mtime=?12, file_size=?13, missing=0, analyzed=CASE WHEN manual_cues=1 THEN analyzed ELSE 0 END, updated_at=?14,
                     category=CASE WHEN category='' THEN ?15 ELSE category END
                     WHERE id=?1",
                    params![id, f.kind.as_str(), f.artist, f.title, f.album, f.year, f.genre, f.duration_ms, f.isrc, f.composer, f.publisher, f.file_mtime, f.file_size, now, f.category],
                )?;
                Ok(UpsertResult::Updated(id))
            }
            None => {
                let seq: i64 = rand::random::<u32>() as i64;
                c.execute(
                    "INSERT INTO tracks(kind,path,filename,artist,title,album,year,genre,category,duration_ms,cue_out_ms,mix_ms,isrc,composer,publisher,file_mtime,file_size,added_at,updated_at,rotation_seq)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,?10,?11,?12,?13,?14,?15,?16,?16,?17)",
                    params![f.kind.as_str(), f.path, f.filename, f.artist, f.title, f.album, f.year, f.genre, f.category, f.duration_ms, f.isrc, f.composer, f.publisher, f.file_mtime, f.file_size, now, seq],
                )?;
                Ok(UpsertResult::Inserted(c.last_insert_rowid()))
            }
        }
    }

    /// Označí jako chybějící všechny skladby na dané cestě nebo pod ní (smazaný soubor/složka).
    pub fn mark_missing_prefix(&self, path: &str) -> Result<usize> {
        let c = self.conn.lock();
        let sep = std::path::MAIN_SEPARATOR;
        let prefix = format!("{}{}%", path.trim_end_matches(sep), sep);
        Ok(c.execute(
            "UPDATE tracks SET missing=1, updated_at=?3 WHERE missing=0 AND (path=?1 OR path LIKE ?2)",
            params![path, prefix, now_utc()],
        )?)
    }

    pub fn paths_under(&self, root: &str) -> Result<Vec<(i64, String)>> {
        let c = self.conn.lock();
        let sep = std::path::MAIN_SEPARATOR;
        let prefix = format!("{}{}%", root.trim_end_matches(sep), sep);
        let mut st = c.prepare("SELECT id,path FROM tracks WHERE missing=0 AND path LIKE ?1")?;
        let v = st.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn set_missing(&self, id: i64, missing: bool) -> Result<()> {
        self.conn.lock().execute("UPDATE tracks SET missing=?2, updated_at=?3 WHERE id=?1", params![id, missing as i64, now_utc()])?;
        Ok(())
    }

    pub fn get_track(&self, id: i64) -> Result<Option<Track>> {
        let c = self.conn.lock();
        Ok(c.query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE id=?1"), [id], row_track).optional()?)
    }

    pub fn list_tracks(&self, f: &TrackFilter) -> Result<(Vec<Track>, i64)> {
        let mut wh = vec!["1=1".to_string()];
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![];
        if let Some(k) = &f.kind { if !k.is_empty() { args.push(Box::new(k.clone())); wh.push(format!("kind=?{}", args.len())); } }
        if let Some(cat) = &f.category { if !cat.is_empty() { args.push(Box::new(cat.clone())); wh.push(format!("category=?{}", args.len())); } }
        if let Some(q) = &f.q {
            if !q.trim().is_empty() {
                args.push(Box::new(format!("%{}%", q.trim())));
                let n = args.len();
                wh.push(format!("(artist LIKE ?{n} OR title LIKE ?{n} OR album LIKE ?{n} OR filename LIKE ?{n} OR playlister_code LIKE ?{n})"));
            }
        }
        for (val, col) in [(&f.category2, "category2"), (&f.category3, "category3"), (&f.vocal, "vocal")] {
            if let Some(v) = val { if !v.is_empty() { args.push(Box::new(v.clone())); wh.push(format!("{col}=?{}", args.len())); } }
        }
        if let Some(pr) = &f.property { if !pr.is_empty() { args.push(Box::new(format!("%{}%", pr))); wh.push(format!("properties LIKE ?{}", args.len())); } }
        if let Some(b) = f.bookmarked { wh.push(format!("bookmarked={}", b as i64)); }
        if let Some(a) = f.analyzed { wh.push(format!("analyzed={}", a as i64)); }
        if !f.include_disabled.unwrap_or(true) { wh.push("enabled=1".into()); }
        if !f.include_missing.unwrap_or(true) { wh.push("missing=0".into()); }
        let desc = f.dir.as_deref() == Some("desc");
        let d = if desc { "DESC" } else { "ASC" };
        let order = match f.sort.as_deref() {
            Some("title") => format!("title COLLATE NOCASE {d}, artist COLLATE NOCASE"),
            Some("added") => format!("added_at {}", if f.dir.is_none() { "DESC" } else { d }),
            Some("played") => format!("last_played_at {}", if f.dir.is_none() { "DESC" } else { d }),
            Some("plays") => format!("play_count {}", if f.dir.is_none() { "DESC" } else { d }),
            Some("duration") => format!("duration_ms {}", if f.dir.is_none() { "DESC" } else { d }),
            Some("category") => format!("category COLLATE NOCASE {d}, artist COLLATE NOCASE"),
            _ => format!("artist COLLATE NOCASE {d}, title COLLATE NOCASE"),
        };
        let where_sql = wh.join(" AND ");
        let c = self.conn.lock();
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM tracks WHERE {where_sql}"), refs.as_slice(), |r| r.get(0))?;
        let limit = f.limit.unwrap_or(200).clamp(1, 5000);
        let offset = f.offset.unwrap_or(0).max(0);
        let mut st = c.prepare(&format!("SELECT {TRACK_COLS} FROM tracks WHERE {where_sql} ORDER BY {order} LIMIT {limit} OFFSET {offset}"))?;
        let v = st.query_map(refs.as_slice(), row_track)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((v, total))
    }

    pub fn categories(&self) -> Result<Vec<CategoryCount>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT kind, category, COUNT(*) FROM tracks WHERE missing=0 GROUP BY kind, category ORDER BY kind, category")?;
        let v = st.query_map([], |r| Ok(CategoryCount { kind: r.get(0)?, category: r.get(1)?, count: r.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn patch_track(&self, id: i64, p: &TrackPatch) -> Result<()> {
        let c = self.conn.lock();
        let tx = c.unchecked_transaction()?;
        macro_rules! set {
            ($field:ident, $col:literal) => {
                if let Some(v) = &p.$field { tx.execute(concat!("UPDATE tracks SET ", $col, "=?2 WHERE id=?1"), params![id, v])?; }
            };
        }
        set!(artist, "artist"); set!(title, "title"); set!(album, "album"); set!(year, "year"); set!(genre, "genre");
        set!(category, "category"); set!(category2, "category2"); set!(category3, "category3");
        set!(hook_in_ms, "hook_in_ms"); set!(hook_out_ms, "hook_out_ms"); set!(gain_db, "gain_db");
        set!(playlister_code, "playlister_code"); set!(isrc, "isrc"); set!(composer, "composer"); set!(publisher, "publisher");
        set!(cue_in_ms, "cue_in_ms"); set!(intro_ms, "intro_ms"); set!(mix_ms, "mix_ms"); set!(cue_out_ms, "cue_out_ms");
        set!(vocal, "vocal"); set!(properties, "properties"); set!(color, "color"); set!(comment, "comment");
        if let Some(sep) = &p.separation_min { tx.execute("UPDATE tracks SET separation_min=?2 WHERE id=?1", params![id, sep])?; }
        if let Some(b) = p.bookmarked { tx.execute("UPDATE tracks SET bookmarked=?2 WHERE id=?1", params![id, b as i64])?; }
        if let Some(e) = p.enabled { tx.execute("UPDATE tracks SET enabled=?2 WHERE id=?1", params![id, e as i64])?; }
        if p.cue_in_ms.is_some() || p.mix_ms.is_some() || p.cue_out_ms.is_some() || p.intro_ms.is_some() {
            tx.execute("UPDATE tracks SET manual_cues=1 WHERE id=?1", [id])?;
        }
        tx.execute("UPDATE tracks SET updated_at=?2 WHERE id=?1", params![id, now_utc()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_analysis(&self, id: i64, duration_ms: i64, cue_in: i64, mix: i64, cue_out: i64, lufs: Option<f64>) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "UPDATE tracks SET duration_ms=?2, lufs=?6, analyzed=1, updated_at=?7,
               cue_in_ms=CASE WHEN manual_cues=1 THEN cue_in_ms ELSE ?3 END,
               mix_ms=CASE WHEN manual_cues=1 THEN mix_ms ELSE ?4 END,
               cue_out_ms=CASE WHEN manual_cues=1 THEN cue_out_ms ELSE ?5 END
             WHERE id=?1",
            params![id, duration_ms, cue_in, mix, cue_out, lufs, now_utc()],
        )?;
        Ok(())
    }

    pub fn reset_analysis(&self, id: i64, clear_manual: bool) -> Result<()> {
        let c = self.conn.lock();
        if clear_manual { c.execute("UPDATE tracks SET analyzed=0, manual_cues=0 WHERE id=?1", [id])?; }
        else { c.execute("UPDATE tracks SET analyzed=0 WHERE id=?1", [id])?; }
        Ok(())
    }

    pub fn unanalyzed_ids(&self) -> Result<Vec<i64>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT id FROM tracks WHERE analyzed=0 AND missing=0 ORDER BY added_at DESC")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(v)
    }

    /// Kandidáti pro rotaci (nejdéle nehrané první).
    pub fn rotation_candidates(&self, kind: TrackKind, category: &str, song_sep_since: &str, limit: i64) -> Result<Vec<Track>> {
        let c = self.conn.lock();
        let cat_clause = if category.is_empty() { "" } else { "AND category=?2" };
        let sql = format!(
            "SELECT {TRACK_COLS} FROM tracks WHERE kind=?1 {cat_clause} AND enabled=1 AND missing=0 AND duration_ms>0
             AND (last_played_at IS NULL
                  OR (separation_min IS NULL AND last_played_at < ?3)
                  OR (separation_min IS NOT NULL AND ?3 <> '9999' AND last_played_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-' || separation_min || ' minutes'))
                  OR ?3 = '9999')
             ORDER BY (analyzed=0), (last_played_at IS NOT NULL), last_played_at, rotation_seq LIMIT ?4"
        );
        let mut st = c.prepare(&sql)?;
        let v = if category.is_empty() {
            st.query_map(params![kind.as_str(), rusqlite::types::Null, song_sep_since, limit], row_track)?.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            st.query_map(params![kind.as_str(), category, song_sep_since, limit], row_track)?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(v)
    }

    pub fn artists_played_since(&self, since: &str) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT DISTINCT lower(artist) FROM play_log WHERE started_at >= ?1 AND artist<>'' AND kind='song'")?;
        let v = st.query_map([since], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(v)
    }

    pub fn reshuffle(&self) -> Result<()> {
        let c = self.conn.lock();
        c.execute("UPDATE tracks SET rotation_seq = abs(random()) % 2147483647", [])?;
        Ok(())
    }

    pub fn log_start(&self, t: &Track, source: &str, reason: &str) -> Result<i64> {
        let now = now_utc();
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO play_log(track_id,kind,artist,title,album,path,category,isrc,composer,publisher,duration_ms,started_at,source,reason)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![if t.id > 0 { Some(t.id) } else { None }, t.kind().as_str(), t.artist, t.title, t.album, t.path, t.category, t.isrc, t.composer, t.publisher, t.duration_ms, now, source, reason],
        )?;
        let id = c.last_insert_rowid();
        if t.id > 0 {
            c.execute("UPDATE tracks SET last_played_at=?2, play_count=play_count+1 WHERE id=?1", params![t.id, now])?;
        }
        Ok(id)
    }

    pub fn log_end(&self, log_id: i64, played_ms: i64) -> Result<()> {
        self.conn.lock().execute("UPDATE play_log SET ended_at=?2, played_ms=?3 WHERE id=?1", params![log_id, now_utc(), played_ms])?;
        Ok(())
    }

    pub fn log_range(&self, from: &str, to: &str, kind: Option<&str>, limit: i64) -> Result<Vec<LogEntry>> {
        let c = self.conn.lock();
        let kind_clause = if kind.is_some() { "AND kind=?3" } else { "AND ?3 IS NULL" };
        let mut st = c.prepare(&format!(
            "SELECT {LOG_COLS} FROM play_log WHERE started_at >= ?1 AND started_at < ?2 {kind_clause} ORDER BY started_at DESC LIMIT ?4"
        ))?;
        let v = st.query_map(params![from, to, kind, limit], row_log)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn recent_log(&self, limit: i64) -> Result<Vec<LogEntry>> {
        let c = self.conn.lock();
        let mut st = c.prepare(&format!("SELECT {LOG_COLS} FROM play_log ORDER BY id DESC LIMIT ?1"))?;
        let v = st.query_map([limit], row_log)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.lock().query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }

    pub fn stats(&self) -> Result<serde_json::Value> {
        let c = self.conn.lock();
        let total: i64 = c.query_row("SELECT COUNT(*) FROM tracks WHERE missing=0", [], |r| r.get(0))?;
        let missing: i64 = c.query_row("SELECT COUNT(*) FROM tracks WHERE missing=1", [], |r| r.get(0))?;
        let pending: i64 = c.query_row("SELECT COUNT(*) FROM tracks WHERE analyzed=0 AND missing=0", [], |r| r.get(0))?;
        Ok(serde_json::json!({"tracks": total, "missing": missing, "analysis_pending": pending}))
    }
}
