//! Číselníky kategorií (Category 1/2/3, Vocal, Properties) s barvami a komentáři,
//! knihovna Memos a palety instant jinglů.

use crate::db::{now_utc, row_track, Db, TRACK_COLS};
use crate::model::Track;
use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const LEVELS: [&str; 5] = ["category", "category2", "category3", "vocal", "properties"];

fn level_col(level: &str) -> Result<&'static str> {
    LEVELS.iter().find(|l| **l == level).copied().ok_or_else(|| anyhow!("neznámá úroveň kategorie '{level}'"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CategoryEntry {
    pub id: Option<i64>,
    /// song | jingle | spot | sweeper | voicetrack | asset | instant | memo
    pub kind: String,
    /// category | category2 | category3 | vocal | properties
    pub level: String,
    pub name: String,
    pub color: String,
    pub comment: String,
    pub sort: i64,
    /// Počet položek s touto hodnotou (jen pro čtení).
    pub count: i64,
}

/// Číselník: zaregistrované kategorie + hodnoty použité ve skladbách (i když nejsou v číselníku).
pub fn list_categories(db: &Db, kind: Option<&str>, level: Option<&str>) -> Result<Vec<CategoryEntry>> {
    db.with_conn(|c| {
        let mut out: Vec<CategoryEntry> = vec![];
        let mut st = c.prepare("SELECT id,kind,level,name,color,comment,sort FROM categories WHERE (?1 IS NULL OR kind=?1) AND (?2 IS NULL OR level=?2) ORDER BY kind, level, sort, name COLLATE NOCASE")?;
        let rows = st.query_map(params![kind, level], |r| Ok(CategoryEntry {
            id: Some(r.get(0)?), kind: r.get(1)?, level: r.get(2)?, name: r.get(3)?, color: r.get(4)?, comment: r.get(5)?, sort: r.get(6)?, count: 0,
        }))?;
        for r in rows { out.push(r?); }
        // použité hodnoty
        for lvl in LEVELS.iter().filter(|l| level.map(|x| x == **l).unwrap_or(true)) {
            if *lvl == "properties" { continue; }
            let sql = format!("SELECT kind, {lvl}, COUNT(*) FROM tracks WHERE missing=0 AND {lvl}<>'' AND (?1 IS NULL OR kind=?1) GROUP BY kind, {lvl}");
            let mut st = c.prepare(&sql)?;
            let used = st.query_map(params![kind], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
            for u in used {
                let (k, name, cnt) = u?;
                if let Some(e) = out.iter_mut().find(|e| e.kind == k && e.level == *lvl && e.name == name) { e.count = cnt; }
                else { out.push(CategoryEntry { id: None, kind: k, level: lvl.to_string(), name, count: cnt, ..Default::default() }); }
            }
        }
        if level.map(|l| l == "memo" || l == "category").unwrap_or(true) && kind.map(|k| k == "memo").unwrap_or(true) {
            let mut st = c.prepare("SELECT category, COUNT(*) FROM memos WHERE category<>'' GROUP BY category")?;
            let used = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for u in used {
                let (name, cnt) = u?;
                if let Some(e) = out.iter_mut().find(|e| e.kind == "memo" && e.level == "category" && e.name == name) { e.count = cnt; }
                else { out.push(CategoryEntry { id: None, kind: "memo".into(), level: "category".into(), name, count: cnt, ..Default::default() }); }
            }
        }
        Ok(out)
    })
}

pub fn create_category(db: &Db, e: &CategoryEntry) -> Result<i64> {
    level_col(&e.level)?;
    if e.name.trim().is_empty() { return Err(anyhow!("prázdný název")); }
    db.with_conn(|c| {
        c.execute("INSERT INTO categories(kind,level,name,color,comment,sort) VALUES(?1,?2,?3,?4,?5,?6)
                   ON CONFLICT(kind,level,name) DO UPDATE SET color=excluded.color, comment=excluded.comment, sort=excluded.sort",
            params![e.kind, e.level, e.name.trim(), e.color, e.comment, e.sort])?;
        Ok(c.query_row("SELECT id FROM categories WHERE kind=?1 AND level=?2 AND name=?3", params![e.kind, e.level, e.name.trim()], |r| r.get(0))?)
    })
}

/// Úprava; přejmenování se promítne do všech skladeb (a memos).
pub fn update_category(db: &Db, id: i64, e: &CategoryEntry) -> Result<()> {
    db.with_conn(|c| {
        let (kind, level, old): (String, String, String) = c.query_row("SELECT kind,level,name FROM categories WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?.ok_or_else(|| anyhow!("kategorie nenalezena"))?;
        let col = level_col(&level)?;
        let new = e.name.trim();
        if !new.is_empty() && new != old {
            if kind == "memo" { c.execute("UPDATE memos SET category=?1 WHERE category=?2", params![new, old])?; }
            else if col == "properties" {
                c.execute("UPDATE tracks SET properties = trim(replace(',' || properties || ',', ',' || ?1 || ',', ',' || ?2 || ','), ',') WHERE kind=?3", params![old, new, kind])?;
            } else {
                c.execute(&format!("UPDATE tracks SET {col}=?1, updated_at=?4 WHERE {col}=?2 AND kind=?3"), params![new, old, kind, now_utc()])?;
            }
        }
        c.execute("UPDATE categories SET name=?2, color=?3, comment=?4, sort=?5 WHERE id=?1", params![id, if new.is_empty() { &old } else { new }, e.color, e.comment, e.sort])?;
        Ok(())
    })
}

/// Smazání kategorie; `clear_tracks` = odebrat hodnotu i ze skladeb.
pub fn delete_category(db: &Db, id: i64, clear_tracks: bool) -> Result<()> {
    db.with_conn(|c| {
        if let Some((kind, level, name)) = c.query_row("SELECT kind,level,name FROM categories WHERE id=?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).optional()? {
            if clear_tracks {
                let col = level_col(&level)?;
                if kind == "memo" { c.execute("UPDATE memos SET category='' WHERE category=?1", [&name])?; }
                else if col != "properties" { c.execute(&format!("UPDATE tracks SET {col}='' WHERE {col}=?1 AND kind=?2"), params![name, kind])?; }
            }
            c.execute("DELETE FROM categories WHERE id=?1", [id])?;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------
// Memos

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Memo {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub category: String,
    pub color: String,
    pub pinned: bool,
    pub playlister_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_memo(r: &rusqlite::Row) -> rusqlite::Result<Memo> {
    Ok(Memo { id: r.get(0)?, title: r.get(1)?, body: r.get(2)?, category: r.get(3)?, color: r.get(4)?, pinned: r.get::<_, i64>(5)? != 0,
        playlister_code: r.get(6)?, created_at: r.get(7)?, updated_at: r.get(8)? })
}

pub fn list_memos(db: &Db, category: Option<&str>, q: Option<&str>) -> Result<Vec<Memo>> {
    db.with_conn(|c| {
        let like = q.filter(|s| !s.trim().is_empty()).map(|s| format!("%{}%", s.trim()));
        let mut st = c.prepare("SELECT id,title,body,category,color,pinned,playlister_code,created_at,updated_at FROM memos
            WHERE (?1 IS NULL OR ?1='' OR category=?1) AND (?2 IS NULL OR title LIKE ?2 OR body LIKE ?2)
            ORDER BY pinned DESC, updated_at DESC")?;
        let v = st.query_map(params![category, like], row_memo)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

pub fn save_memo(db: &Db, m: &Memo) -> Result<Memo> {
    let now = now_utc();
    let id = db.with_conn(|c| -> Result<i64> {
        if m.id > 0 {
            c.execute("UPDATE memos SET title=?2, body=?3, category=?4, color=?5, pinned=?6, playlister_code=?7, updated_at=?8 WHERE id=?1",
                params![m.id, m.title, m.body, m.category, m.color, m.pinned as i64, m.playlister_code, now])?;
            Ok(m.id)
        } else {
            c.execute("INSERT INTO memos(title,body,category,color,pinned,playlister_code,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
                params![m.title, m.body, m.category, m.color, m.pinned as i64, m.playlister_code, now])?;
            Ok(c.last_insert_rowid())
        }
    })?;
    db.with_conn(|c| Ok(c.query_row("SELECT id,title,body,category,color,pinned,playlister_code,created_at,updated_at FROM memos WHERE id=?1", [id], row_memo)?))
}

pub fn delete_memo(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM memos WHERE id=?1", [id])?; Ok(()) })
}

// ---------------------------------------------------------------------------------------------
// Palety instant jinglů

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Pad {
    pub page: i64,
    pub idx: i64,
    pub track_id: Option<i64>,
    pub label: String,
    pub color: String,
    /// Skladba na padu (jen pro čtení).
    #[serde(skip_deserializing)]
    pub track: Option<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Palette {
    pub id: i64,
    pub name: String,
    pub rows: i64,
    pub cols: i64,
    pub pages: i64,
    pub sort: i64,
    #[serde(skip_deserializing)]
    pub pads: Vec<Pad>,
}

pub fn list_palettes(db: &Db) -> Result<Vec<Palette>> {
    db.with_conn(|c| {
        let mut st = c.prepare("SELECT id,name,rows,cols,pages,sort FROM palettes ORDER BY sort, id")?;
        let mut pals = st.query_map([], |r| Ok(Palette { id: r.get(0)?, name: r.get(1)?, rows: r.get(2)?, cols: r.get(3)?, pages: r.get(4)?, sort: r.get(5)?, pads: vec![] }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut pst = c.prepare("SELECT page,idx,track_id,label,color FROM pads WHERE palette_id=?1 ORDER BY page, idx")?;
        let mut tst = c.prepare(&format!("SELECT {TRACK_COLS} FROM tracks WHERE id=?1"))?;
        for p in pals.iter_mut() {
            let pads = pst.query_map([p.id], |r| Ok(Pad { page: r.get(0)?, idx: r.get(1)?, track_id: r.get(2)?, label: r.get(3)?, color: r.get(4)?, track: None }))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for mut pad in pads {
                if let Some(tid) = pad.track_id { pad.track = tst.query_row([tid], row_track).optional()?; }
                p.pads.push(pad);
            }
        }
        Ok(pals)
    })
}

pub fn save_palette(db: &Db, p: &Palette) -> Result<i64> {
    let (rows, cols, pages) = (p.rows.clamp(1, 12), p.cols.clamp(1, 12), p.pages.clamp(1, 20));
    db.with_conn(|c| {
        if p.id > 0 {
            c.execute("UPDATE palettes SET name=?2, rows=?3, cols=?4, pages=?5, sort=?6 WHERE id=?1", params![p.id, p.name, rows, cols, pages, p.sort])?;
            c.execute("DELETE FROM pads WHERE palette_id=?1 AND (page>=?2 OR idx>=?3)", params![p.id, pages, rows * cols])?;
            Ok(p.id)
        } else {
            c.execute("INSERT INTO palettes(name,rows,cols,pages,sort) VALUES(?1,?2,?3,?4,?5)", params![if p.name.is_empty() { "Paleta" } else { &p.name }, rows, cols, pages, p.sort])?;
            Ok(c.last_insert_rowid())
        }
    })
}

pub fn delete_palette(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| { c.execute("DELETE FROM pads WHERE palette_id=?1", [id])?; c.execute("DELETE FROM palettes WHERE id=?1", [id])?; Ok(()) })
}

pub fn set_pad(db: &Db, palette_id: i64, pad: &Pad) -> Result<()> {
    db.with_conn(|c| {
        if pad.track_id.is_none() && pad.label.is_empty() {
            c.execute("DELETE FROM pads WHERE palette_id=?1 AND page=?2 AND idx=?3", params![palette_id, pad.page, pad.idx])?;
        } else {
            c.execute("INSERT INTO pads(palette_id,page,idx,track_id,label,color) VALUES(?1,?2,?3,?4,?5,?6)
                       ON CONFLICT(palette_id,page,idx) DO UPDATE SET track_id=excluded.track_id, label=excluded.label, color=excluded.color",
                params![palette_id, pad.page, pad.idx, pad.track_id, pad.label, pad.color])?;
        }
        Ok(())
    })
}

/// Při prvním spuštění vytvoří výchozí paletu.
pub fn ensure_default_palette(db: &Db) -> Result<()> {
    let n: i64 = db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM palettes", [], |r| r.get(0)))?;
    if n == 0 { save_palette(db, &Palette { name: "Hlavní".into(), rows: 4, cols: 6, pages: 2, ..Default::default() })?; }
    Ok(())
}
