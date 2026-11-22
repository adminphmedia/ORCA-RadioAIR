//! Reporty vysílání (pro autorské organizace a vlastní potřebu).

use crate::db::Db;
use anyhow::Result;
use chrono::{DateTime, Local};

fn csv_field(s: &str) -> String {
    if s.contains(';') || s.contains('"') || s.contains('\n') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// CSV (UTF-8 s BOM, středník – otevře se rovnou v českém Excelu).
/// `from`/`to` v UTC ISO; `kind` např. Some("song").
pub fn broadcast_csv(db: &Db, from: &str, to: &str, kind: Option<&str>, lang: &str) -> Result<String> {
    let mut rows = db.log_range(from, to, kind, 1_000_000)?;
    rows.reverse();
    let header = if lang == "en" {
        "Date;Start time;End time;Artist;Title;Album;Composer;Publisher;ISRC;Played (s);Duration (s);Category;Type;Source"
    } else {
        "Datum;Čas začátku;Čas konce;Interpret;Název;Album;Autor;Vydavatel;ISRC;Odvysíláno (s);Délka (s);Kategorie;Typ;Zdroj"
    };
    let mut out = String::from("\u{feff}");
    out += header;
    out += "\n";
    let loc = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Local));
    for r in rows {
        let st = loc(&r.started_at);
        let en = r.ended_at.as_deref().and_then(loc);
        let fields = [
            st.map(|d| d.format("%d.%m.%Y").to_string()).unwrap_or_default(),
            st.map(|d| d.format("%H:%M:%S").to_string()).unwrap_or_default(),
            en.map(|d| d.format("%H:%M:%S").to_string()).unwrap_or_default(),
            r.artist, r.title, r.album, r.composer, r.publisher, r.isrc,
            r.played_ms.map(|m| format!("{}", (m as f64 / 1000.0).round() as i64)).unwrap_or_default(),
            format!("{}", (r.duration_ms as f64 / 1000.0).round() as i64),
            r.category, r.kind, r.source,
        ];
        out += &fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(";");
        out += "\n";
    }
    Ok(out)
}
