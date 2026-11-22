//! Import knihovny z jiných odbavovacích programů.
//!
//! Zdroje:
//! * **Jazler SOHO** – CSV export knihovny (Control Panel → Libraries → Songs → Export, „Customize Export“).
//! * **RadioDJ** – SQL dump databáze (tabulka `songs`, `subcategory`, `genre`) nebo CSV export tabulky `songs`;
//!   značky z pole `cue_times` (`&sta=…&int=…&out=…&xta=…&end=…&fin=…&fou=…`, sekundy).
//! * **CartMaster / Broadcast WAV** – značky v cart chunku WAV souborů (AES46: AUDs, AUDe, INTe, SEGs …)
//!   a v LIST/INFO (IART, INAM, IGNR …).
//! * **mAirList** – metadata `.mmd` vedle zvukového souboru (CueIn, Ramp1, Outro, FadeOut, StartNext, CueOut).
//! * **Playlisty** M3U/M3U8/PLS.
//! * **Obecný import** – libovolné CSV/TSV s nastavitelným přiřazením sloupců a jednotkou času.
//!
//! Postup: náhled (nic se nezapisuje) → úprava přiřazení → import na pozadí s průběhem a protokolem.

use crate::config::WatchFolder;
use crate::db::{Db, TrackPatch};
use crate::decode::is_supported;
use crate::library::{self, Library};
use crate::model::TrackKind;
use anyhow::{anyhow, Result};
use lofty::prelude::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------------------------------------
// Značky (cue body)

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Markers {
    pub cue_in_ms: Option<i64>,
    pub intro_ms: Option<i64>,
    pub outro_ms: Option<i64>,
    pub mix_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub hook_in_ms: Option<i64>,
    pub hook_out_ms: Option<i64>,
    /// Odkud značky pochází (cart, radiodj, mairlist, csv …).
    pub source: String,
}

impl Markers {
    pub fn is_empty(&self) -> bool {
        [self.cue_in_ms, self.intro_ms, self.outro_ms, self.mix_ms, self.cue_out_ms, self.hook_in_ms, self.hook_out_ms].iter().all(|v| v.is_none())
    }
    /// Doplní chybějící hodnoty z jiného zdroje.
    pub fn fill_from(&mut self, o: &Markers) {
        macro_rules! f { ($($x:ident),*) => { $( if self.$x.is_none() { self.$x = o.$x; } )* } }
        f!(cue_in_ms, intro_ms, outro_ms, mix_ms, cue_out_ms, hook_in_ms, hook_out_ms);
        if self.source.is_empty() { self.source = o.source.clone(); }
    }
    /// Odstraní nesmyslné hodnoty (konec 0, mix za koncem …).
    pub fn sanitize(&mut self) {
        for v in [&mut self.cue_in_ms, &mut self.intro_ms, &mut self.outro_ms, &mut self.hook_in_ms, &mut self.hook_out_ms] {
            if matches!(v, Some(x) if *x < 0) { *v = None; }
        }
        for v in [&mut self.mix_ms, &mut self.cue_out_ms] { if matches!(v, Some(x) if *x <= 0) { *v = None; } }
        if let (Some(m), Some(e)) = (self.mix_ms, self.cue_out_ms) { if m > e { self.mix_ms = Some(e); } }
        if let (Some(i), Some(c)) = (self.intro_ms, self.cue_in_ms) { if i < c { self.intro_ms = None; } }
        if self.intro_ms == Some(0) { self.intro_ms = None; }
        if self.outro_ms == Some(0) { self.outro_ms = None; }
    }
}

fn num(s: &str) -> Option<f64> {
    let t = s.trim().replace(',', ".");
    if t.is_empty() { return None; }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Převod času na milisekundy. unit: auto | ms | s | samples | hms
pub fn to_ms(val: &str, unit: &str, sample_rate: u32) -> Option<i64> {
    let v = val.trim().trim_matches('"');
    if v.is_empty() || v.eq_ignore_ascii_case("null") { return None; }
    let hms = |v: &str| -> Option<i64> {
        let parts: Vec<&str> = v.split(':').collect();
        let mut secs = 0.0;
        for p in &parts { secs = secs * 60.0 + num(p)?; }
        Some((secs * 1000.0).round() as i64)
    };
    match unit {
        "ms" => num(v).map(|x| x.round() as i64),
        "s" => if v.contains(':') { hms(v) } else { num(v).map(|x| (x * 1000.0).round() as i64) },
        "samples" => num(v).map(|x| (x / sample_rate.max(1) as f64 * 1000.0).round() as i64),
        "hms" => hms(v),
        _ => {
            if v.contains(':') { return hms(v); }
            let x = num(v)?;
            if v.contains('.') || v.contains(',') { Some((x * 1000.0).round() as i64) }
            else if x >= 10_000.0 { Some(x.round() as i64) } else { Some((x * 1000.0).round() as i64) }
        }
    }
}

/// RadioDJ `cue_times`: `&sta=0.25&int=7.3&out=147.4&xta=160.2&end=161.1&fin=0&fou=0.86` (sekundy).
pub fn parse_radiodj_cues(s: &str) -> Markers {
    let mut kv: HashMap<String, f64> = HashMap::new();
    for part in s.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if let Some(x) = num(v) { kv.entry(k.trim().to_ascii_lowercase()).or_insert(x); } // první výskyt platí (jako v RadioDJ)
        }
    }
    let ms = |k: &str| kv.get(k).map(|x| (x * 1000.0).round() as i64);
    let mut m = Markers {
        cue_in_ms: ms("sta"), intro_ms: ms("int"), outro_ms: ms("out"), mix_ms: ms("xta"), cue_out_ms: ms("end"),
        hook_in_ms: ms("hin"), hook_out_ms: ms("hou"), source: "radiodj".into(),
    };
    // bez „start next“ začne další položka na začátku zeslabení (fou = délka zeslabení končící na END)
    if m.mix_ms.is_none() {
        if let (Some(end), Some(fou)) = (m.cue_out_ms, ms("fou")) { if fou > 0 { m.mix_ms = Some(end - fou); } }
    }
    m
}

fn looks_like_radiodj(s: &str) -> bool { s.contains("sta=") && (s.contains("end=") || s.contains("xta=") || s.contains("int=")) }

/// mAirList `.mmd` (XML) – tolerantní čtení: `<CueIn>1.5</CueIn>` i `<CuePoint Type="CueIn" Position="1.5"/>`.
pub fn parse_mairlist_xml(xml: &str) -> (Markers, BTreeMap<String, String>) {
    let low = xml.to_ascii_lowercase();
    let find = |name: &str| -> Option<i64> {
        let n = name.to_ascii_lowercase();
        // element <name>value</name>
        if let Some(i) = low.find(&format!("<{n}>")) {
            let rest = &xml[i + n.len() + 2..];
            if let Some(end) = rest.find('<') { if let Some(x) = num(&rest[..end]) { return Some((x * 1000.0).round() as i64); } }
        }
        // atribut "name" a číselný atribut v témže elementu
        let mut from = 0;
        while let Some(j) = low[from..].find(&format!("\"{n}\"")) {
            let at = from + j;
            let start = low[..at].rfind('<').unwrap_or(0);
            let end = low[at..].find('>').map(|e| at + e).unwrap_or(low.len());
            let el = &xml[start..end];
            let el_low = &low[start..end];
            for attr in ["position", "value", "time", "pos"] {
                if let Some(k) = el_low.find(&format!("{attr}=\"")) {
                    let v = &el[k + attr.len() + 2..];
                    if let Some(q) = v.find('"') { if let Some(x) = num(&v[..q]) { return Some((x * 1000.0).round() as i64); } }
                }
            }
            from = at + n.len() + 2;
        }
        None
    };
    let mut m = Markers {
        cue_in_ms: find("CueIn"), intro_ms: find("Ramp1").or_else(|| find("Ramp")), outro_ms: find("Outro"),
        mix_ms: find("StartNext").or_else(|| find("FadeOut")), cue_out_ms: find("CueOut"),
        hook_in_ms: find("HookIn"), hook_out_ms: find("HookOut"), source: "mairlist".into(),
    };
    m.sanitize();
    let mut meta = BTreeMap::new();
    for tag in ["Title", "Artist", "Album", "Year", "Comment"] {
        let t = tag.to_ascii_lowercase();
        if let Some(i) = low.find(&format!("<{t}>")) {
            let rest = &xml[i + t.len() + 2..];
            if let Some(e) = rest.find('<') { let v = xml_unescape(rest[..e].trim()); if !v.is_empty() { meta.insert(t, v); } }
        }
    }
    (m, meta)
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Cart chunk (AES46) a LIST/INFO z WAV souboru.
#[derive(Debug, Default, Clone)]
pub struct WavInfo { pub markers: Markers, pub artist: String, pub title: String, pub category: String, pub cut_id: String, pub genre: String, pub comment: String }

pub fn read_wav_info(path: &Path) -> Option<WavInfo> {
    let data = std::fs::read(path).ok()?;
    parse_wav_info(&data)
}

fn fixed_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s = match std::str::from_utf8(&b[..end]) { Ok(s) => s.to_string(), Err(_) => encoding_rs::WINDOWS_1250.decode(&b[..end]).0.to_string() };
    s.trim().to_string()
}

pub fn parse_wav_info(data: &[u8]) -> Option<WavInfo> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" { return None; }
    let mut info = WavInfo::default();
    let mut rate: u32 = 44100;
    let mut timers: Vec<([u8; 4], u32)> = vec![];
    let mut pos = 12;
    let mut found = false;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(data.len());
        let body = &data[body_start..body_end];
        match id {
            b"fmt " if body.len() >= 8 => { rate = u32::from_le_bytes(body[4..8].try_into().ok()?); }
            b"cart" if body.len() >= 748 => {
                found = true;
                info.title = fixed_str(&body[4..68]);
                info.artist = fixed_str(&body[68..132]);
                info.cut_id = fixed_str(&body[132..196]);
                info.category = fixed_str(&body[260..324]);
                for i in 0..8 {
                    let o = 684 + i * 8;
                    let usage: [u8; 4] = body[o..o + 4].try_into().ok()?;
                    let val = u32::from_le_bytes(body[o + 4..o + 8].try_into().ok()?);
                    if usage != [0; 4] && val != u32::MAX { timers.push((usage, val)); }
                }
            }
            b"LIST" if body.len() >= 4 && &body[0..4] == b"INFO" => {
                let mut p = 4;
                while p + 8 <= body.len() {
                    let sid = &body[p..p + 4];
                    let sz = u32::from_le_bytes(body[p + 4..p + 8].try_into().ok()?) as usize;
                    let v = fixed_str(&body[p + 8..(p + 8 + sz).min(body.len())]);
                    match sid {
                        b"IART" if info.artist.is_empty() => info.artist = v,
                        b"INAM" if info.title.is_empty() => info.title = v,
                        b"IGNR" => info.genre = v,
                        b"ICMT" => info.comment = v,
                        b"IMED" => if let Some(x) = to_ms(&v, "auto", rate) { info.markers.mix_ms.get_or_insert(x); found = true; },
                        b"ISRF" => if let Some(x) = to_ms(&v, "auto", rate) { info.markers.intro_ms.get_or_insert(x); found = true; },
                        _ => {}
                    }
                    p += 8 + sz + (sz & 1);
                }
            }
            _ => {}
        }
        pos = body_start + size + (size & 1);
    }
    let ms = |v: u32| (v as f64 / rate.max(1) as f64 * 1000.0).round() as i64;
    for (u, v) in timers {
        let code = String::from_utf8_lossy(&u).to_string();
        let m = &mut info.markers;
        let base = code.get(0..3).unwrap_or("").to_ascii_uppercase();
        let last = code.chars().last().unwrap_or(' ');
        match (base.as_str(), last) {
            ("AUD", 's') => m.cue_in_ms = Some(ms(v)),
            ("AUD", 'e') => m.cue_out_ms = Some(ms(v)),
            ("EOD", _) => { m.cue_out_ms.get_or_insert(ms(v)); }
            ("INT", 's') => {}
            ("INT", _) => m.intro_ms = Some(ms(v)),
            ("SEG", 's') | ("SEG", '1') | ("SEG", ' ') => m.mix_ms = Some(ms(v)),
            ("OUT", _) => m.outro_ms = Some(ms(v)),
            ("HOO", 's') | ("HKs", _) => m.hook_in_ms = Some(ms(v)),
            ("HOO", 'e') => m.hook_out_ms = Some(ms(v)),
            _ => {}
        }
    }
    if !found && info.artist.is_empty() && info.title.is_empty() { return None; }
    info.markers.source = "cart".into();
    info.markers.sanitize();
    Some(info)
}

/// Značky uložené v souboru nebo vedle něj: mAirList .mmd, cart chunk, RadioDJ (tag sync).
pub fn file_markers(path: &Path) -> Markers {
    let mut m = Markers::default();
    let mut sidecars = vec![PathBuf::from(format!("{}.mmd", path.display())), path.with_extension("mmd")];
    sidecars.dedup();
    for sc in sidecars {
        if let Ok(x) = std::fs::read_to_string(&sc) { let (mm, _) = parse_mairlist_xml(&x); m.fill_from(&mm); break; }
    }
    if path.extension().map(|e| e.eq_ignore_ascii_case("wav")).unwrap_or(false) {
        if let Some(w) = read_wav_info(path) { m.fill_from(&w.markers); }
    }
    if let Ok(tf) = lofty::read_from_path(path) {
        'outer: for tag in tf.tags() {
            for item in tag.items() {
                if let Some(s) = item.value().text() {
                    if looks_like_radiodj(s) { m.fill_from(&parse_radiodj_cues(s)); break 'outer; }
                }
            }
        }
    }
    m.sanitize();
    m
}

// ---------------------------------------------------------------------------------------------
// Tabulka (CSV, SQL)

#[derive(Debug, Clone, Default)]
pub struct Table { pub format: String, pub headers: Vec<String>, pub rows: Vec<Vec<String>> }

/// Text z bajtů: UTF-8 (s BOM), UTF-16 (BOM), jinak Windows-1250.
pub fn decode_text(b: &[u8]) -> String {
    if b.starts_with(&[0xFF, 0xFE]) { return encoding_rs::UTF_16LE.decode(b).0.to_string(); }
    if b.starts_with(&[0xFE, 0xFF]) { return encoding_rs::UTF_16BE.decode(b).0.to_string(); }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    match std::str::from_utf8(b) { Ok(s) => s.to_string(), Err(_) => encoding_rs::WINDOWS_1250.decode(b).0.to_string() }
}

fn detect_delim(text: &str) -> char {
    let first: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(5).collect();
    let mut best = (',', 0usize);
    for d in [';', '\t', ',', '|'] {
        let counts: Vec<usize> = first.iter().map(|l| l.matches(d).count()).collect();
        let min = counts.iter().copied().min().unwrap_or(0);
        if min > best.1 { best = (d, min); }
    }
    best.0
}

pub fn parse_csv(text: &str, delim: Option<char>, has_header: bool) -> Table {
    let d = delim.unwrap_or_else(|| detect_delim(text));
    let mut rows: Vec<Vec<String>> = vec![];
    let mut row = vec![];
    let mut field = String::new();
    let mut inq = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if inq {
            if c == '"' { if chars.peek() == Some(&'"') { field.push('"'); chars.next(); } else { inq = false; } } else { field.push(c); }
        } else if c == '"' && field.trim().is_empty() { field.clear(); inq = true; }
        else if c == d { row.push(std::mem::take(&mut field)); }
        else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') { chars.next(); }
            row.push(std::mem::take(&mut field));
            if !(row.len() == 1 && row[0].trim().is_empty()) { rows.push(std::mem::take(&mut row)); } else { row.clear(); }
        } else { field.push(c); }
    }
    if !field.is_empty() || !row.is_empty() { row.push(field); rows.push(row); }
    let headers = if has_header && !rows.is_empty() { rows.remove(0).into_iter().map(|h| h.trim().to_string()).collect() }
        else { (1..=rows.iter().map(|r| r.len()).max().unwrap_or(0)).map(|i| format!("#{i}")).collect() };
    Table { format: format!("csv ({})", if d == '\t' { "tab".to_string() } else { d.to_string() }), headers, rows }
}

/// Hodnoty z `INSERT INTO … VALUES (…),(…);` (MySQL dump).
fn parse_sql_values(s: &str) -> (Vec<Vec<String>>, usize) {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut rows = vec![];
    let mut row: Vec<String> = vec![];
    let mut field = String::new();
    let mut depth = 0;
    let mut quoted = false;
    while i < b.len() {
        let c = b[i];
        if depth == 0 {
            if c == '(' { depth = 1; row.clear(); field.clear(); quoted = false; }
            else if c == ';' { return (rows, i + 1); }
            i += 1; continue;
        }
        match c {
            '\'' => {
                quoted = true;
                i += 1;
                while i < b.len() {
                    let ch = b[i];
                    if ch == '\\' && i + 1 < b.len() {
                        let n = b[i + 1];
                        field.push(match n { 'n' => '\n', 'r' => '\r', 't' => '\t', '0' => '\0', other => other });
                        i += 2; continue;
                    }
                    if ch == '\'' { if i + 1 < b.len() && b[i + 1] == '\'' { field.push('\''); i += 2; continue; } break; }
                    field.push(ch); i += 1;
                }
            }
            ',' => { row.push(if !quoted && field.trim().eq_ignore_ascii_case("null") { String::new() } else if quoted { field.clone() } else { field.trim().to_string() }); field.clear(); quoted = false; }
            ')' => {
                row.push(if !quoted && field.trim().eq_ignore_ascii_case("null") { String::new() } else if quoted { field.clone() } else { field.trim().to_string() });
                field.clear(); quoted = false; depth = 0;
                rows.push(std::mem::take(&mut row));
            }
            _ => if !quoted { field.push(c); },
        }
        i += 1;
    }
    (rows, b.len())
}

/// Tabulka z MySQL dumpu (sloupce z `CREATE TABLE` nebo ze seznamu v `INSERT`).
pub fn parse_sql_table(text: &str, table: &str) -> Option<Table> {
    let low = text.to_ascii_lowercase();
    let t = table.to_ascii_lowercase();
    let mut headers: Vec<String> = vec![];
    for pat in [format!("create table `{t}`"), format!("create table if not exists `{t}`"), format!("create table {t} ")] {
        if let Some(i) = low.find(&pat) {
            let body = &text[i..];
            let end = body.find(";").unwrap_or(body.len());
            for line in body[..end].lines().skip(1) {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix('`') { if let Some(e) = rest.find('`') { headers.push(rest[..e].to_string()); } }
            }
            break;
        }
    }
    let mut rows = vec![];
    let mut from = 0;
    let pats = [format!("insert into `{t}`"), format!("insert into {t} "), format!("insert into {t}(")];
    loop {
        let next = pats.iter().filter_map(|p| low[from..].find(p.as_str()).map(|j| (from + j, p.len()))).min();
        let Some((i, plen)) = next else { break };
        let after = &text[i + plen..];
        let vpos = after.to_ascii_lowercase().find("values")?;
        let cols = &after[..vpos];
        if let (Some(a), Some(b)) = (cols.find('('), cols.rfind(')')) {
            let list: Vec<String> = cols[a + 1..b].split(',').map(|c| c.trim().trim_matches('`').to_string()).filter(|c| !c.is_empty()).collect();
            if !list.is_empty() { headers = list; }
        }
        let (r, used) = parse_sql_values(&after[vpos + 6..]);
        rows.extend(r);
        from = i + plen + vpos + 6 + after[vpos + 6..].chars().take(used).map(|c| c.len_utf8()).sum::<usize>();
        if from >= text.len() { break; }
    }
    if rows.is_empty() { return None; }
    if headers.is_empty() { headers = (1..=rows[0].len()).map(|i| format!("#{i}")).collect(); }
    Some(Table { format: format!("sql ({table})"), headers, rows })
}

/// Playlist M3U / M3U8 / PLS.
pub fn parse_playlist(text: &str) -> Table {
    let mut rows = vec![];
    let mut pending: Option<(String, String)> = None;
    let mut pls: BTreeMap<usize, [String; 3]> = BTreeMap::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() { continue; }
        if let Some(rest) = l.strip_prefix("#EXTINF:") {
            let (dur, name) = rest.split_once(',').unwrap_or((rest, ""));
            pending = Some((dur.trim().to_string(), name.trim().to_string()));
        } else if l.starts_with('#') || l.starts_with('[') || l.to_ascii_lowercase().starts_with("numberofentries") || l.to_ascii_lowercase().starts_with("version") {
            continue;
        } else if let Some((k, v)) = l.split_once('=').filter(|(k, _)| k.to_ascii_lowercase().starts_with("file") || k.to_ascii_lowercase().starts_with("title") || k.to_ascii_lowercase().starts_with("length")) {
            let kl = k.to_ascii_lowercase();
            let digits: String = kl.chars().filter(|c| c.is_ascii_digit()).collect();
            let n = digits.parse().unwrap_or(0);
            let e = pls.entry(n).or_default();
            if kl.starts_with("file") { e[0] = v.to_string(); } else if kl.starts_with("title") { e[1] = v.to_string(); } else { e[2] = v.to_string(); }
        } else {
            let (dur, name) = pending.take().unwrap_or_default();
            let (a, t) = name.split_once(" - ").map(|(a, t)| (a.to_string(), t.to_string())).unwrap_or((String::new(), name));
            rows.push(vec![l.to_string(), a, t, dur]);
        }
    }
    for (_, [f, title, len]) in pls {
        if f.is_empty() { continue; }
        let (a, t) = title.split_once(" - ").map(|(a, t)| (a.to_string(), t.to_string())).unwrap_or((String::new(), title));
        rows.push(vec![f, a, t, len]);
    }
    Table { format: "playlist".into(), headers: vec!["path".into(), "artist".into(), "title".into(), "duration".into()], rows }
}

// ---------------------------------------------------------------------------------------------
// Předvolby a přiřazení sloupců

/// Pole knihovny ORCA, do kterých lze importovat.
pub const FIELDS: &[&str] = &[
    "path", "filename", "artist", "title", "album", "year", "genre", "category", "category2", "kind", "duration",
    "cue_in", "intro", "outro", "mix", "cue_out", "fade_out", "hook_in", "hook_out", "cue_string",
    "gain", "isrc", "composer", "lyricist", "label", "publisher", "comment", "code", "enabled",
];

/// Synonyma názvů sloupců (normalizovaná: malá písmena, bez diakritiky, mezer a podtržítek).
fn synonyms(field: &str) -> &'static [&'static str] {
    match field {
        "path" => &["path", "filepath", "fullpath", "file", "location", "cesta", "soubor", "filelocation", "pathname", "audiofile"],
        "filename" => &["filename", "nazevsouboru", "fname"],
        "artist" => &["artist", "artists", "interpret", "performer", "umelec", "autor"],
        "title" => &["title", "nazev", "songtitle", "song", "titul", "skladba"],
        "album" => &["album"],
        "year" => &["year", "rok", "releaseyear"],
        "genre" => &["genre", "zanr", "style", "idgenre", "genrename"],
        "category" => &["category", "kategorie", "subcategory", "subcat", "subcategoryname", "idsubcat", "cat", "group", "skupina"],
        "category2" => &["category2", "cat2", "mood", "nalada", "category02"],
        "kind" => &["songtype", "type", "kind", "typ", "itemtype", "druh"],
        "duration" => &["duration", "length", "delka", "len", "time", "playtime"],
        "cue_in" => &["start", "cuein", "sta", "audiostart", "begin", "startpoint", "in"],
        "intro" => &["intro", "introtime", "ramp", "ramp1", "int", "introend", "talkover"],
        "outro" => &["outro", "outrotime", "out", "vocalend", "outrostart"],
        "mix" => &["mix", "mixpoint", "next", "segue", "startnext", "xta", "nextstart", "mixtime", "seguetime", "crossfade"],
        "cue_out" => &["end", "cueout", "stop", "audioend", "endpoint", "konec"],
        "fade_out" => &["fadeout", "fou", "fade"],
        "hook_in" => &["hookin", "hookstart", "hin"],
        "hook_out" => &["hookout", "hookend", "hou"],
        "cue_string" => &["cuetimes", "cues", "cuepoints"],
        "gain" => &["gain", "replaygain", "gaindb", "volume", "trackgain"],
        "isrc" => &["isrc"],
        "composer" => &["composer", "skladatel", "writer", "composers", "author"],
        "lyricist" => &["lyricist", "lyricists", "lyrics", "textar", "autortextu", "autorislov", "lyricwriter", "textari"],
        "label" => &["label", "recordlabel", "znacka", "znackavyrobce", "vyrobce", "producer", "labelname"],
        "publisher" => &["publisher", "vydavatel", "nakladatel", "musicpublisher"],
        "comment" => &["comment", "comments", "poznamka", "notes", "note", "memo"],
        "code" => &["code", "cartid", "cutid", "cart", "kod", "playlistercode", "externalid"],
        "enabled" => &["enabled", "active", "aktivni", "isenabled"],
        _ => &[],
    }
}

pub fn norm(s: &str) -> String {
    s.chars().flat_map(|c| c.to_lowercase()).map(|c| match c {
        'á' => 'a', 'č' => 'c', 'ď' => 'd', 'é' | 'ě' => 'e', 'í' => 'i', 'ň' => 'n', 'ó' => 'o', 'ř' => 'r', 'š' => 's',
        'ť' => 't', 'ú' | 'ů' => 'u', 'ý' => 'y', 'ž' => 'z', other => other,
    }).filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// Automatické přiřazení sloupců podle názvů v hlavičce.
pub fn auto_mapping(headers: &[String]) -> BTreeMap<String, String> {
    let normed: Vec<String> = headers.iter().map(|h| norm(h)).collect();
    let mut used = vec![false; headers.len()];
    let mut m = BTreeMap::new();
    for f in FIELDS {
        for syn in synonyms(f) {
            if let Some(i) = normed.iter().enumerate().position(|(i, h)| !used[i] && h == syn) {
                used[i] = true;
                m.insert(f.to_string(), headers[i].clone());
                break;
            }
        }
    }
    m
}

#[derive(Debug, Clone, Serialize)]
pub struct PresetInfo { pub id: &'static str, pub name: &'static str, pub source: &'static str, pub hint: &'static str, pub time_unit: &'static str }

pub const PRESETS: &[PresetInfo] = &[
    PresetInfo { id: "jazler_soho", name: "Jazler SOHO", source: "file",
        hint: "Control Panel → Libraries → Songs (Jingles, Spots …) → Export. Při prvním exportu v „Customize Export“ zaškrtněte všechna pole včetně cesty k souboru, kategorií a časů intra a mixu. Každou knihovnu importujte zvlášť a zvolte typ.", time_unit: "auto" },
    PresetInfo { id: "radiodj", name: "RadioDJ", source: "file",
        hint: "Export databáze MySQL (HeidiSQL / phpMyAdmin / mysqldump) jako .sql nebo tabulky songs jako CSV. Značky se čtou z pole cue_times, typ ze song_type, kategorie ze subcategory.", time_unit: "s" },
    PresetInfo { id: "cartchunk", name: "CartMaster / Broadcast WAV (cart chunk)", source: "folder",
        hint: "Vyberte složku se soubory. Načtou se značky z cart chunku WAV (AES46: AUDs, AUDe, INTe, SEGs …), LIST/INFO a tagy. Hodí se pro CartMaster, ENCO, Dalet, Rivendell a další systémy, které ukládají značky do WAV.", time_unit: "samples" },
    PresetInfo { id: "mairlist", name: "mAirList", source: "folder",
        hint: "Vyberte složku se zvukovými soubory a jejich .mmd metadaty. Převezme se CueIn, Ramp1 (intro), Outro, StartNext/FadeOut (mix), CueOut a háčky.", time_unit: "s" },
    PresetInfo { id: "folder", name: "Složka se soubory (tagy a značky)", source: "folder",
        hint: "Projde složku a podsložky, přečte tagy a všechny známé značky (cart chunk, RadioDJ tag sync, mAirList .mmd). Kategorie podle podsložky.", time_unit: "auto" },
    PresetInfo { id: "playlist", name: "Playlist M3U / PLS", source: "file", hint: "Seznam souborů z jakéhokoli přehrávače nebo odbavovacího programu.", time_unit: "s" },
    PresetInfo { id: "generic", name: "Obecný CSV / TSV (vlastní přiřazení)", source: "file",
        hint: "Libovolná tabulka (CSV, TSV, středník, tabulátor). Sloupce přiřaďte ručně, zvolte jednotku času (ms, s, mm:ss, vzorky).", time_unit: "auto" },
];

pub fn radiodj_kind_map() -> BTreeMap<String, String> {
    [("0", "song"), ("1", "jingle"), ("2", "sweeper"), ("3", "voicetrack"), ("4", "spot"), ("5", "asset"), ("6", "asset"),
     ("7", "asset"), ("8", "asset"), ("9", "asset"), ("10", "asset"), ("11", "asset"), ("12", "asset"), ("13", "asset"), ("14", "jingle")]
        .into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportOptions {
    pub preset: String,
    /// Cesta k souboru (CSV, SQL, M3U) nebo ke složce.
    pub source: String,
    /// Obsah nahraného souboru (místo cesty).
    pub text: Option<String>,
    /// "" = automaticky, jinak ";" "," "\t" "|"
    pub delimiter: String,
    pub has_header: bool,
    /// pole ORCA → název sloupce
    pub mapping: BTreeMap<String, String>,
    /// auto | ms | s | samples | hms
    pub time_unit: String,
    pub sample_rate: u32,
    /// Přemapování cest ze starého počítače: např. `C:\Jazler\Songs` → `/Volumes/Radio/Songs`
    pub path_from: String,
    pub path_to: String,
    /// Kde hledat soubory podle názvu, když cesta neodpovídá (např. zkopírovaná složka staré stanice).
    pub files_root: String,
    pub default_kind: TrackKind,
    /// hodnota sloupce „kind“ → typ ORCA (song, jingle, spot …)
    pub kind_map: BTreeMap<String, String>,
    pub category_from_folder: bool,
    pub default_category: String,
    /// copy (do složky stanice) | move | data_only (jen doplnit údaje k už naimportovaným souborům)
    pub file_mode: String,
    /// Přepsat údaje u skladeb, které už v knihovně jsou.
    pub update_existing: bool,
    /// Doplnit značky ze souborů (cart chunk, .mmd, RadioDJ tag), pokud je tabulka nemá.
    pub read_file_markers: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            preset: "generic".into(), source: String::new(), text: None, delimiter: String::new(), has_header: true,
            mapping: BTreeMap::new(), time_unit: "auto".into(), sample_rate: 44100, path_from: String::new(), path_to: String::new(),
            files_root: String::new(), default_kind: TrackKind::Song, kind_map: BTreeMap::new(), category_from_folder: false,
            default_category: String::new(), file_mode: "copy".into(), update_existing: true, read_file_markers: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportRow {
    pub line: usize,
    /// cesta tak, jak je ve zdroji
    pub source_path: String,
    /// nalezený soubor
    pub file: Option<String>,
    pub kind: Option<TrackKind>,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub genre: String,
    pub category: String,
    pub category2: String,
    pub year: Option<i32>,
    pub duration_ms: Option<i64>,
    pub gain_db: Option<f64>,
    pub isrc: String,
    pub composer: String,
    pub lyricist: String,
    pub label: String,
    pub publisher: String,
    pub comment: String,
    pub code: String,
    pub enabled: Option<bool>,
    pub markers: Markers,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Preview {
    pub format: String,
    pub headers: Vec<String>,
    pub mapping: BTreeMap<String, String>,
    pub kind_map: BTreeMap<String, String>,
    pub total: usize,
    pub with_file: usize,
    pub missing_files: usize,
    pub with_markers: usize,
    pub kinds: BTreeMap<String, usize>,
    pub categories: BTreeMap<String, usize>,
    pub rows: Vec<ImportRow>,
    pub sample: Vec<Vec<String>>,
    pub warnings: Vec<String>,
}

fn read_source_text(o: &ImportOptions) -> Result<String> {
    if let Some(t) = &o.text { if !t.is_empty() { return Ok(t.clone()); } }
    if o.source.trim().is_empty() { return Err(anyhow!("vyberte soubor k importu")); }
    let b = std::fs::read(o.source.trim()).map_err(|e| anyhow!("{}: {e}", o.source))?;
    Ok(decode_text(&b))
}

/// Načte zdroj jako tabulku (CSV, SQL, playlist). Složky vrací None.
pub fn load_table(o: &ImportOptions) -> Result<Option<Table>> {
    let is_folder = PRESETS.iter().find(|p| p.id == o.preset).map(|p| p.source == "folder").unwrap_or(false) || Path::new(o.source.trim()).is_dir();
    if is_folder && o.text.as_deref().unwrap_or("").is_empty() { return Ok(None); }
    let text = read_source_text(o)?;
    let low_head: String = text.chars().take(200_000).collect::<String>().to_ascii_lowercase();
    if low_head.contains("insert into") {
        let mut t = parse_sql_table(&text, "songs").or_else(|| {
            // jiný název tabulky: první INSERT
            let i = low_head.find("insert into")?;
            let rest = text[i + 11..].trim_start();
            let name: String = rest.trim_start_matches('`').chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            parse_sql_table(&text, &name)
        }).ok_or_else(|| anyhow!("v SQL souboru nejsou žádná data (INSERT INTO …)"))?;
        // RadioDJ: názvy podkategorií a žánrů místo čísel
        let names = |table: &str| -> HashMap<String, String> {
            parse_sql_table(&text, table).map(|tb| {
                let id = tb.headers.iter().position(|h| norm(h) == "id").unwrap_or(0);
                let name = tb.headers.iter().position(|h| norm(h) == "name").unwrap_or(tb.headers.len().saturating_sub(1));
                tb.rows.iter().filter_map(|r| Some((r.get(id)?.clone(), r.get(name)?.clone()))).collect()
            }).unwrap_or_default()
        };
        for (col, table, newcol) in [("idsubcat", "subcategory", "category"), ("idgenre", "genre", "genre")] {
            if let Some(ci) = t.headers.iter().position(|h| norm(h) == col) {
                let map = names(table);
                if !map.is_empty() {
                    t.headers.push(newcol.into());
                    for r in t.rows.iter_mut() { let v = r.get(ci).and_then(|x| map.get(x)).cloned().unwrap_or_default(); r.push(v); }
                }
            }
        }
        return Ok(Some(t));
    }
    let lowsrc = o.source.to_ascii_lowercase();
    if o.preset == "playlist" || lowsrc.ends_with(".m3u") || lowsrc.ends_with(".m3u8") || lowsrc.ends_with(".pls") || text.trim_start().starts_with("#EXTM3U") {
        return Ok(Some(parse_playlist(&text)));
    }
    let delim = match o.delimiter.as_str() { "" => None, "\\t" | "tab" => Some('\t'), d => d.chars().next() };
    Ok(Some(parse_csv(&text, delim, o.has_header)))
}

fn rewrite_path(o: &ImportOptions, p: &str) -> String {
    let mut s = p.trim().trim_matches('"').to_string();
    if s.to_ascii_lowercase().starts_with("file://") {
        s = urlencoding::decode(&s[7..]).map(|c| c.to_string()).unwrap_or(s[7..].to_string());
    }
    let from = o.path_from.trim();
    if !from.is_empty() {
        let (sl, fl) = (s.replace('\\', "/").to_lowercase(), from.replace('\\', "/").trim_end_matches('/').to_lowercase());
        if sl.starts_with(&fl) {
            let rest = s.replace('\\', "/")[fl.len()..].to_string();
            s = format!("{}{}", o.path_to.trim().trim_end_matches(['/', '\\']), rest);
        }
    }
    if !cfg!(windows) { s = s.replace('\\', "/"); }
    s
}

fn filename_of(p: &str) -> String { p.rsplit(['/', '\\']).next().unwrap_or(p).to_string() }

/// Rejstřík souborů ve složce podle názvu (malá písmena).
fn index_files(root: &str) -> HashMap<String, Vec<PathBuf>> {
    let mut m: HashMap<String, Vec<PathBuf>> = HashMap::new();
    if root.trim().is_empty() || !Path::new(root.trim()).is_dir() { return m; }
    for e in walkdir::WalkDir::new(root.trim()).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        if e.file_type().is_file() && is_supported(e.path()) {
            m.entry(e.file_name().to_string_lossy().to_lowercase()).or_default().push(e.path().to_path_buf());
        }
    }
    m
}

fn resolve_file(o: &ImportOptions, raw: &str, idx: &HashMap<String, Vec<PathBuf>>) -> Option<PathBuf> {
    if raw.trim().is_empty() { return None; }
    let p = rewrite_path(o, raw);
    let pb = PathBuf::from(&p);
    if pb.is_file() { return Some(pb); }
    if pb.is_relative() && !o.files_root.trim().is_empty() {
        let j = Path::new(o.files_root.trim()).join(&p);
        if j.is_file() { return Some(j); }
    }
    let name = filename_of(raw).to_lowercase();
    let cands = idx.get(&name)?;
    if cands.len() == 1 { return Some(cands[0].clone()); }
    // více souborů se stejným názvem – vyber ten, jehož nadřazená složka odpovídá
    let parent = raw.replace('\\', "/").rsplit('/').nth(1).unwrap_or("").to_lowercase();
    cands.iter().find(|c| c.parent().and_then(|x| x.file_name()).map(|n| n.to_string_lossy().to_lowercase() == parent).unwrap_or(false)).or(cands.first()).cloned()
}

fn kind_from(o: &ImportOptions, v: &str) -> Option<TrackKind> {
    let v = v.trim();
    if v.is_empty() { return None; }
    if let Some(k) = o.kind_map.get(v).or_else(|| o.kind_map.get(&v.to_lowercase())) { return TrackKind::parse(k); }
    let n = norm(v);
    Some(match n.as_str() {
        "song" | "songs" | "music" | "hudba" | "skladba" | "pisen" => TrackKind::Song,
        "jingle" | "jingles" | "znelka" | "id" | "station id" => TrackKind::Jingle,
        "spot" | "spots" | "commercial" | "commercials" | "reklama" | "ad" | "advert" => TrackKind::Spot,
        "sweeper" | "sweepers" | "promo" | "liner" => TrackKind::Sweeper,
        "voicetrack" | "voiceover" | "vt" | "voice" => TrackKind::VoiceTrack,
        "bed" | "beds" | "podkres" => TrackKind::Bed,
        "instant" | "instantjingle" => TrackKind::Instant,
        "asset" | "other" | "ostatni" | "news" | "zpravy" => TrackKind::Asset,
        _ => return None,
    })
}

fn row_from_table(o: &ImportOptions, headers: &[String], r: &[String], line: usize, mapping: &BTreeMap<String, String>) -> ImportRow {
    let col = |f: &str| -> String {
        let Some(name) = mapping.get(f).filter(|n| !n.is_empty()) else { return String::new() };
        let idx = headers.iter().position(|h| h == name)
            .or_else(|| name.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()).map(|n| n.saturating_sub(1)));
        idx.and_then(|i| r.get(i)).map(|s| s.trim().to_string()).unwrap_or_default()
    };
    let unit = o.time_unit.as_str();
    let t = |f: &str| to_ms(&col(f), unit, o.sample_rate);
    let mut row = ImportRow { line, ..Default::default() };
    row.source_path = { let p = col("path"); if p.is_empty() { col("filename") } else { p } };
    row.artist = col("artist");
    row.title = col("title");
    row.album = col("album");
    row.genre = col("genre");
    row.category = col("category");
    row.category2 = col("category2");
    row.year = col("year").chars().take(4).collect::<String>().parse().ok().filter(|y: &i32| *y > 1000);
    row.duration_ms = t("duration");
    row.gain_db = num(&col("gain"));
    row.isrc = col("isrc");
    row.composer = col("composer");
    row.lyricist = col("lyricist");
    row.label = col("label");
    row.publisher = col("publisher");
    row.comment = col("comment");
    row.code = col("code");
    let en = col("enabled").to_lowercase();
    if !en.is_empty() { row.enabled = Some(!matches!(en.as_str(), "0" | "false" | "ne" | "no" | "n")); }
    row.kind = kind_from(o, &col("kind"));
    let cs = col("cue_string");
    let mut m = if looks_like_radiodj(&cs) { parse_radiodj_cues(&cs) } else { Markers::default() };
    let tab = Markers {
        cue_in_ms: t("cue_in"), intro_ms: t("intro"), outro_ms: t("outro"), mix_ms: t("mix"), cue_out_ms: t("cue_out"),
        hook_in_ms: t("hook_in"), hook_out_ms: t("hook_out"), source: "tabulka".into(),
    };
    m.fill_from(&tab);
    if m.mix_ms.is_none() {
        if let (Some(end), Some(fo)) = (m.cue_out_ms, t("fade_out")) { if fo > 0 && fo < end { m.mix_ms = Some(end - fo); } }
    }
    // Jazler a další ukládají intro často jako délku od začátku skladby – to je u nás totéž (čas od začátku souboru).
    m.sanitize();
    row.markers = m;
    row
}

fn rows_from_folder(o: &ImportOptions) -> Result<Vec<ImportRow>> {
    let root = PathBuf::from(o.source.trim());
    if !root.is_dir() { return Err(anyhow!("složka {} neexistuje", root.display())); }
    let wf = WatchFolder { id: "import".into(), path: root.clone(), kind: o.default_kind, category: o.default_category.clone(), category_from_subfolder: o.category_from_folder, enabled: true };
    let mut rows = vec![];
    for (n, e) in walkdir::WalkDir::new(&root).follow_links(true).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file() && is_supported(e.path())).enumerate() {
        let p = e.path();
        let mut row = ImportRow { line: n + 1, source_path: p.display().to_string(), file: Some(p.display().to_string()), ..Default::default() };
        match library::read_file(p, &wf) {
            Ok(f) => {
                row.artist = f.artist; row.title = f.title; row.album = f.album; row.genre = f.genre; row.year = f.year;
                row.isrc = f.isrc; row.composer = f.composer; row.publisher = f.publisher; row.lyricist = f.lyricist; row.label = f.label; row.category = f.category;
                row.duration_ms = Some(f.duration_ms).filter(|d| *d > 0);
            }
            Err(err) => row.warnings.push(err.to_string()),
        }
        if p.extension().map(|x| x.eq_ignore_ascii_case("wav")).unwrap_or(false) {
            if let Some(w) = read_wav_info(p) {
                if !w.artist.is_empty() { row.artist = w.artist; }
                if !w.title.is_empty() { row.title = w.title; }
                if row.category.is_empty() && !w.category.is_empty() { row.category = w.category; }
                if !w.cut_id.is_empty() { row.code = w.cut_id; }
                if row.genre.is_empty() { row.genre = w.genre; }
                if row.comment.is_empty() { row.comment = w.comment; }
            }
        }
        for sc in [PathBuf::from(format!("{}.mmd", p.display())), p.with_extension("mmd")] {
            if let Ok(x) = std::fs::read_to_string(&sc) {
                let (_, meta) = parse_mairlist_xml(&x);
                if let Some(v) = meta.get("title") { row.title = v.clone(); }
                if let Some(v) = meta.get("artist") { row.artist = v.clone(); }
                if let Some(v) = meta.get("album") { row.album = v.clone(); }
                if let Some(v) = meta.get("comment") { row.comment = v.clone(); }
                break;
            }
        }
        row.markers = file_markers(p);
        rows.push(row);
    }
    Ok(rows)
}

/// Kompletní zpracování zdroje: tabulka → řádky se soubory a značkami.
pub fn build_rows(o: &ImportOptions) -> Result<(Vec<ImportRow>, Preview)> {
    let mut pv = Preview::default();
    let rows: Vec<ImportRow> = match load_table(o)? {
        None => { pv.format = "složka".into(); rows_from_folder(o)? }
        Some(t) => {
            let mut mapping = auto_mapping(&t.headers);
            for (k, v) in &o.mapping { if v.is_empty() { mapping.remove(k); } else { mapping.insert(k.clone(), v.clone()); } }
            if o.preset == "radiodj" && !mapping.contains_key("cue_string") {
                if let Some(h) = t.headers.iter().find(|h| norm(h) == "cuetimes") { mapping.insert("cue_string".into(), h.clone()); }
            }
            pv.format = t.format.clone();
            pv.headers = t.headers.clone();
            pv.sample = t.rows.iter().take(5).cloned().collect();
            if !mapping.contains_key("path") && !mapping.contains_key("filename") {
                pv.warnings.push("Nenašel jsem sloupec s cestou ani názvem souboru – přiřaďte ho ručně.".into());
            }
            let mut o2 = o.clone();
            if o2.preset == "radiodj" && o2.kind_map.is_empty() { o2.kind_map = radiodj_kind_map(); }
            pv.kind_map = o2.kind_map.clone();
            let idx = index_files(&o.files_root);
            let r: Vec<ImportRow> = t.rows.iter().enumerate().map(|(i, r)| {
                let mut row = row_from_table(&o2, &t.headers, r, i + 1 + o.has_header as usize, &mapping);
                match resolve_file(&o2, &row.source_path, &idx) {
                    Some(f) => {
                        if o.category_from_folder && row.category.is_empty() {
                            row.category = f.parent().and_then(|x| x.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        }
                        if o.read_file_markers { let fm = file_markers(&f); row.markers.fill_from(&fm); row.markers.sanitize(); }
                        row.file = Some(f.display().to_string());
                    }
                    None => if o.file_mode != "data_only" { row.warnings.push("soubor nenalezen".into()) },
                }
                row
            }).collect();
            pv.mapping = mapping;
            r
        }
    };
    let mut rows = rows;
    for r in rows.iter_mut() {
        if r.kind.is_none() { r.kind = Some(o.default_kind); }
        if r.category.is_empty() { r.category = o.default_category.clone(); }
        if r.title.is_empty() {
            let stem = filename_of(&r.source_path);
            let stem = stem.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(stem);
            if let Some((a, t)) = stem.split_once(" - ") { if r.artist.is_empty() { r.artist = a.trim().into(); } r.title = t.trim().into(); } else { r.title = stem; }
        }
    }
    pv.total = rows.len();
    pv.with_file = rows.iter().filter(|r| r.file.is_some()).count();
    pv.missing_files = pv.total - pv.with_file;
    pv.with_markers = rows.iter().filter(|r| !r.markers.is_empty()).count();
    for r in &rows {
        *pv.kinds.entry(r.kind.map(|k| k.as_str()).unwrap_or("?").to_string()).or_default() += 1;
        *pv.categories.entry(if r.category.is_empty() { "(bez kategorie)".into() } else { r.category.clone() }).or_default() += 1;
    }
    pv.rows = rows.iter().take(300).cloned().collect();
    Ok((rows, pv))
}

pub fn preview(o: &ImportOptions) -> Result<Preview> { Ok(build_rows(o)?.1) }

// ---------------------------------------------------------------------------------------------
// Provedení importu (na pozadí)

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportStatus {
    pub running: bool,
    pub preset: String,
    pub total: usize,
    pub done: usize,
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
    pub missing: usize,
    pub errors: Vec<String>,
    pub log: Vec<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub report_path: Option<String>,
}

fn status_cell() -> &'static Mutex<ImportStatus> {
    static S: OnceLock<Mutex<ImportStatus>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(ImportStatus::default()))
}

pub fn status() -> ImportStatus { status_cell().lock().clone() }

/// Záplata knihovny podle importovaného řádku.
pub fn patch_for(r: &ImportRow, overwrite: bool) -> TrackPatch {
    let s = |v: &String| if v.is_empty() { None } else { Some(v.clone()) };
    let m = &r.markers;
    let mut p = TrackPatch {
        album: s(&r.album), genre: s(&r.genre), category2: s(&r.category2), isrc: s(&r.isrc), composer: s(&r.composer), lyricist: s(&r.lyricist), label: s(&r.label),
        publisher: s(&r.publisher), comment: s(&r.comment), playlister_code: s(&r.code), year: r.year, enabled: r.enabled,
        gain_db: r.gain_db.filter(|g| g.abs() < 30.0),
        cue_in_ms: m.cue_in_ms, intro_ms: m.intro_ms, outro_ms: m.outro_ms, mix_ms: m.mix_ms, cue_out_ms: m.cue_out_ms,
        hook_in_ms: m.hook_in_ms, hook_out_ms: m.hook_out_ms,
        ..Default::default()
    };
    if overwrite { p.artist = s(&r.artist); p.title = s(&r.title); p.category = s(&r.category); }
    p
}

fn ids_by_filename(db: &Db, name: &str) -> Vec<i64> {
    db.with_conn(|c| -> rusqlite::Result<Vec<i64>> {
        let mut st = c.prepare("SELECT id FROM tracks WHERE filename = ?1 COLLATE NOCASE")?;
        st.query_map([name], |r| r.get(0))?.collect()
    }).unwrap_or_default()
}

/// Spustí import na pozadí. Vrací chybu, pokud už import běží nebo zdroj nejde načíst.
pub fn start(lib: Arc<Library>, db: Arc<Db>, o: ImportOptions) -> Result<ImportStatus> {
    {
        let st = status_cell().lock();
        if st.running { return Err(anyhow!("import už probíhá ({}/{})", st.done, st.total)); }
    }
    let (rows, _) = build_rows(&o)?;
    {
        let mut st = status_cell().lock();
        *st = ImportStatus { running: true, preset: o.preset.clone(), total: rows.len(), started_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), ..Default::default() };
    }
    std::thread::Builder::new().name("orca-import".into()).spawn(move || run(lib, db, o, rows))?;
    Ok(status())
}

fn push_log(msg: String) {
    let mut st = status_cell().lock();
    st.log.push(msg);
    if st.log.len() > 400 { st.log.remove(0); }
}

fn run(lib: Arc<Library>, db: Arc<Db>, o: ImportOptions, rows: Vec<ImportRow>) {
    let mut report = String::new();
    report += &format!("Import ORCA RadioAIR – {} – {}\r\n\r\n", o.preset, chrono::Local::now().format("%Y-%m-%d %H:%M"));
    let mut added_total = 0;
    for r in rows {
        let label = format!("{} – {}", r.artist, r.title);
        let res: Result<&'static str> = (|| {
            let kind = r.kind.unwrap_or(o.default_kind);
            if o.file_mode == "data_only" {
                let mut ids = vec![];
                if let Some(f) = &r.file { if let Some((id, ..)) = db.track_by_path(f)? { ids.push(id); } }
                if ids.is_empty() { ids = ids_by_filename(&db, &filename_of(&r.source_path)); }
                if ids.is_empty() { return Ok("missing"); }
                for id in ids { db.patch_track(id, &patch_for(&r, true))?; }
                return Ok("updated");
            }
            let Some(src) = r.file.as_ref().map(PathBuf::from) else { return Ok("missing") };
            // už v knihovně (stejná cesta)?
            if let Some((id, ..)) = db.track_by_path(&src.display().to_string())? {
                if o.update_existing { db.patch_track(id, &patch_for(&r, true))?; return Ok("updated"); }
                return Ok("skipped");
            }
            let dest = {
                let in_lib = lib.folder_for_public(&src).map(|w| w.kind == kind).unwrap_or(false);
                if in_lib { src.clone() } else {
                    let dir = lib.target_dir(kind, &r.category);
                    let name = library::sanitize(&filename_of(&src.display().to_string()));
                    let same = dir.join(&name);
                    let same_size = std::fs::metadata(&same).ok().zip(std::fs::metadata(&src).ok()).map(|(a, b)| a.len() == b.len()).unwrap_or(false);
                    if same.exists() && same_size { same } else {
                        let policy = if o.update_existing { "rename" } else { "skip" };
                        let Some(d) = library::unique_path(&dir, &name, policy) else { return Ok("skipped") };
                        if o.file_mode == "move" {
                            if std::fs::rename(&src, &d).is_err() { std::fs::copy(&src, &d)?; std::fs::remove_file(&src)?; }
                        } else { std::fs::copy(&src, &d)?; }
                        d
                    }
                }
            };
            let existed = db.track_by_path(&dest.display().to_string())?.is_some();
            let t = lib.register_file(&dest, &r.category)?.ok_or_else(|| anyhow!("soubor nelze zařadit (není ve sledované složce)"))?;
            if existed && !o.update_existing { return Ok("skipped"); }
            db.patch_track(t.id, &patch_for(&r, true))?;
            // sloupec „sekundární kategorie“ apod. nemění typ – ten určuje složka
            Ok(if existed { "updated" } else { "added" })
        })();
        let mut st = status_cell().lock();
        st.done += 1;
        match res {
            Ok("added") => { st.added += 1; added_total += 1; }
            Ok("updated") => st.updated += 1,
            Ok("skipped") => st.skipped += 1,
            Ok(_) => { st.missing += 1; report += &format!("CHYBÍ SOUBOR\t{}\t{}\r\n", r.source_path, label); }
            Err(e) => {
                let msg = format!("{}: {e}", if r.source_path.is_empty() { label.clone() } else { r.source_path.clone() });
                report += &format!("CHYBA\t{msg}\r\n");
                if st.errors.len() < 500 { st.errors.push(msg); }
            }
        }
        if st.done % 100 == 0 { let m = format!("{} / {} zpracováno", st.done, st.total); drop(st); push_log(m); }
    }
    let dir = lib.data_dir.join("Reports");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("import_{}.txt", chrono::Local::now().format("%Y%m%d_%H%M%S")));
    let mut st = status_cell().lock();
    report = format!("Přidáno: {}\r\nAktualizováno: {}\r\nPřeskočeno: {}\r\nChybějící soubory: {}\r\nChyby: {}\r\n\r\n", st.added, st.updated, st.skipped, st.missing, st.errors.len()) + &report;
    if std::fs::write(&path, report).is_ok() { st.report_path = Some(path.display().to_string()); }
    st.running = false;
    st.finished_at = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
    let summary = format!("Hotovo: +{} přidáno, {} aktualizováno, {} chybí, {} chyb", st.added, st.updated, st.missing, st.errors.len());
    st.log.push(summary.clone());
    drop(st);
    log::info!("import: {summary}");
    if added_total > 0 { lib.notify_changed(added_total); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_units() {
        assert_eq!(to_ms("1:30", "auto", 44100), Some(90_000));
        assert_eq!(to_ms("01:02:03.5", "hms", 44100), Some(3_723_500));
        assert_eq!(to_ms("7,25", "auto", 44100), Some(7250));
        assert_eq!(to_ms("12", "auto", 44100), Some(12_000));
        assert_eq!(to_ms("215000", "auto", 44100), Some(215_000));
        assert_eq!(to_ms("44100", "samples", 44100), Some(1000));
        assert_eq!(to_ms("", "s", 44100), None);
    }

    #[test]
    fn radiodj_cues() {
        let m = parse_radiodj_cues("&sta=0.246&int=62.883&out=223.242&xta=237.970&end=239.193&fin=0&fou=1.223");
        assert_eq!(m.cue_in_ms, Some(246));
        assert_eq!(m.intro_ms, Some(62_883));
        assert_eq!(m.outro_ms, Some(223_242));
        assert_eq!(m.mix_ms, Some(237_970));
        assert_eq!(m.cue_out_ms, Some(239_193));
        let m2 = parse_radiodj_cues("&sta=0&end=200&fou=3");
        assert_eq!(m2.mix_ms, Some(197_000));
    }

    #[test]
    fn csv_with_quotes_and_mapping() {
        let t = parse_csv("Artist;Title;File Path;Intro;Mix;Category\r\n\"Kabát\";\"Pohoda; v klidu\";C:\\Hudba\\Pop\\a.mp3;0:12;3:40;Pop\r\n", None, true);
        assert_eq!(t.headers.len(), 6);
        assert_eq!(t.rows[0][1], "Pohoda; v klidu");
        let m = auto_mapping(&t.headers);
        assert_eq!(m.get("path").map(|s| s.as_str()), Some("File Path"));
        assert_eq!(m.get("intro").map(|s| s.as_str()), Some("Intro"));
        let o = ImportOptions { path_from: "C:\\Hudba".into(), path_to: "/Volumes/R".into(), ..Default::default() };
        let r = row_from_table(&o, &t.headers, &t.rows[0], 2, &m);
        assert_eq!(r.markers.intro_ms, Some(12_000));
        assert_eq!(r.markers.mix_ms, Some(220_000));
        assert_eq!(rewrite_path(&o, &r.source_path), "/Volumes/R/Pop/a.mp3");
    }

    #[test]
    fn sql_dump_radiodj() {
        let sql = "CREATE TABLE `songs` (\n  `ID` int(11) NOT NULL,\n  `path` text,\n  `song_type` tinyint,\n  `id_subcat` int,\n  `artist` varchar(250),\n  `title` varchar(250),\n  `cue_times` text\n);\n\
INSERT INTO `songs` VALUES (1,'C:\\\\Music\\\\a.mp3',0,5,'O\\'Brien','Song, one','&sta=1&int=10&end=180'),(2,'C:\\\\Jingles\\\\j.mp3',1,6,'ID','Jingle',NULL);\n\
CREATE TABLE `subcategory` (\n `ID` int,\n `parentid` int,\n `name` varchar(50)\n);\nINSERT INTO `subcategory` VALUES (5,1,'Pop 90s'),(6,2,'Station IDs');\n";
        let o = ImportOptions { preset: "radiodj".into(), text: Some(sql.into()), ..Default::default() };
        let t = load_table(&o).unwrap().unwrap();
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0][1], "C:\\Music\\a.mp3");
        assert_eq!(t.rows[0][4], "O'Brien");
        let (rows, pv) = build_rows(&ImportOptions { file_mode: "data_only".into(), ..o }).unwrap();
        assert_eq!(pv.total, 2);
        assert_eq!(rows[0].category, "Pop 90s");
        assert_eq!(rows[0].markers.intro_ms, Some(10_000));
        assert_eq!(rows[1].kind, Some(TrackKind::Jingle));
        assert_eq!(rows[0].title, "Song, one");
    }

    #[test]
    fn mairlist_xml() {
        let (m, meta) = parse_mairlist_xml("<MetaData><Title>Test</Title><CuePoints><CueIn>0,5</CueIn><Ramp1>12.25</Ramp1><FadeOut>200</FadeOut><CueOut>205</CueOut></CuePoints></MetaData>");
        assert_eq!(m.cue_in_ms, Some(500));
        assert_eq!(m.intro_ms, Some(12_250));
        assert_eq!(m.mix_ms, Some(200_000));
        assert_eq!(meta.get("title").map(|s| s.as_str()), Some("Test"));
        let (m2, _) = parse_mairlist_xml(r#"<CuePoints><CuePoint Type="CueIn" Position="1.0"/><CuePoint Type="Outro" Position="180.5"/></CuePoints>"#);
        assert_eq!(m2.cue_in_ms, Some(1000));
        assert_eq!(m2.outro_ms, Some(180_500));
    }

    #[test]
    fn cart_chunk() {
        // minimální WAV: fmt + cart (s časovači) + data
        let mut cart = vec![0u8; 2048];
        cart[0..4].copy_from_slice(b"0101");
        cart[4..9].copy_from_slice(b"Titul");
        cart[68..74].copy_from_slice(b"Umelec");
        cart[132..137].copy_from_slice(b"C1234");
        cart[260..263].copy_from_slice(b"POP");
        let timers: [(&[u8; 4], u32); 4] = [(b"AUDs", 4410), (b"INTe", 441_000), (b"SEGs", 8_820_000), (b"AUDe", 8_900_000)];
        for (i, (u, v)) in timers.iter().enumerate() {
            let o = 684 + i * 8;
            cart[o..o + 4].copy_from_slice(*u);
            cart[o + 4..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        let mut fmt = vec![0u8; 16];
        fmt[0..2].copy_from_slice(&1u16.to_le_bytes());
        fmt[2..4].copy_from_slice(&2u16.to_le_bytes());
        fmt[4..8].copy_from_slice(&44100u32.to_le_bytes());
        let mut w = b"RIFF\0\0\0\0WAVE".to_vec();
        for (id, body) in [(b"fmt ", fmt), (b"cart", cart), (b"data", vec![0u8; 8])] {
            w.extend_from_slice(id); w.extend_from_slice(&(body.len() as u32).to_le_bytes()); w.extend_from_slice(&body);
        }
        let info = parse_wav_info(&w).unwrap();
        assert_eq!(info.title, "Titul");
        assert_eq!(info.artist, "Umelec");
        assert_eq!(info.category, "POP");
        assert_eq!(info.markers.cue_in_ms, Some(100));
        assert_eq!(info.markers.intro_ms, Some(10_000));
        assert_eq!(info.markers.mix_ms, Some(200_000));
        assert_eq!(info.markers.cue_out_ms, Some(201_814));
    }

    #[test]
    fn playlist_m3u() {
        let t = parse_playlist("#EXTM3U\n#EXTINF:215,Kabát - Pohoda\n/hudba/a.mp3\n");
        assert_eq!(t.rows[0], vec!["/hudba/a.mp3", "Kabát", "Pohoda", "215"]);
        let p = parse_playlist("[playlist]\nFile1=C:\\a.mp3\nTitle1=A - B\nLength1=100\nNumberOfEntries=1\n");
        assert_eq!(p.rows[0][0], "C:\\a.mp3");
    }

    #[test]
    fn windows_1250_text() {
        let (b, _, _) = encoding_rs::WINDOWS_1250.encode("Žluťoučký kůň");
        assert_eq!(decode_text(&b), "Žluťoučký kůň");
    }
}
