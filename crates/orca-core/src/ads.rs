//! Reklama – databáze klientů, obrázek a text spotu pro DAB+ (slideshow + DLS), obsazenost bloků
//! a protokol o odvysílání pro klienta (proof of performance).

use crate::config::Config;
use crate::db::{now_utc, Db};
use crate::spots::{build_block, get_campaign, list_campaigns, spot_len};
use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Client {
    pub id: i64,
    pub name: String,
    pub company: String,
    pub ico: String,
    pub dic: String,
    pub contact: String,
    pub email: String,
    pub phone: String,
    pub address: String,
    /// Obor (pro hlídání konkurence v bloku)
    pub industry: String,
    pub notes: String,
    pub color: String,
    pub enabled: bool,
    #[serde(skip_deserializing)]
    pub campaigns: i64,
    #[serde(skip_deserializing)]
    pub active_campaigns: i64,
    #[serde(skip_deserializing)]
    pub created_at: String,
}

const CL_COLS: &str = "id,name,company,ico,dic,contact,email,phone,address,industry,notes,color,enabled,created_at";
fn row_client(r: &rusqlite::Row) -> rusqlite::Result<Client> {
    Ok(Client { id: r.get(0)?, name: r.get(1)?, company: r.get(2)?, ico: r.get(3)?, dic: r.get(4)?, contact: r.get(5)?, email: r.get(6)?, phone: r.get(7)?,
        address: r.get(8)?, industry: r.get(9)?, notes: r.get(10)?, color: r.get(11)?, enabled: r.get::<_, i64>(12)? != 0, created_at: r.get(13)?, campaigns: 0, active_campaigns: 0 })
}

pub fn list_clients(db: &Db, now_local: &str) -> Result<Vec<Client>> {
    let camps = list_campaigns(db, None, now_local)?;
    let mut v: Vec<Client> = db.with_conn(|c| -> Result<_> {
        let mut st = c.prepare(&format!("SELECT {CL_COLS} FROM clients ORDER BY enabled DESC, name COLLATE NOCASE"))?;
        Ok(st.query_map([], row_client)?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    for cl in v.iter_mut() {
        let mine: Vec<_> = camps.iter().filter(|cp| cp.client_id == Some(cl.id)).collect();
        cl.campaigns = mine.len() as i64;
        cl.active_campaigns = mine.iter().filter(|cp| cp.enabled && cp.start_at.as_deref().map(|s| s <= now_local).unwrap_or(true) && cp.expires_at.as_deref().map(|e| e > now_local).unwrap_or(true)).count() as i64;
    }
    Ok(v)
}

pub fn get_client(db: &Db, id: i64) -> Result<Option<Client>> {
    db.with_conn(|c| Ok(c.query_row(&format!("SELECT {CL_COLS} FROM clients WHERE id=?1"), [id], row_client).optional()?))
}

pub fn save_client(db: &Db, cl: &Client) -> Result<i64> {
    let name = cl.name.trim();
    if name.is_empty() { return Err(anyhow!("klient musí mít název")); }
    let now = now_utc();
    let id = db.with_conn(|c| -> Result<i64> {
        if cl.id > 0 {
            c.execute("UPDATE clients SET name=?2, company=?3, ico=?4, dic=?5, contact=?6, email=?7, phone=?8, address=?9, industry=?10, notes=?11, color=?12, enabled=?13, updated_at=?14 WHERE id=?1",
                params![cl.id, name, cl.company, cl.ico, cl.dic, cl.contact, cl.email, cl.phone, cl.address, cl.industry, cl.notes, cl.color, cl.enabled as i64, now])?;
            // název klienta v jeho kampaních
            c.execute("UPDATE campaigns SET client=?2 WHERE client_id=?1", params![cl.id, name])?;
            Ok(cl.id)
        } else {
            c.execute("INSERT INTO clients(name,company,ico,dic,contact,email,phone,address,industry,notes,color,enabled,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13)",
                params![name, cl.company, cl.ico, cl.dic, cl.contact, cl.email, cl.phone, cl.address, cl.industry, cl.notes, cl.color, cl.enabled as i64, now])?;
            Ok(c.last_insert_rowid())
        }
    })?;
    Ok(id)
}

pub fn delete_client(db: &Db, id: i64) -> Result<()> {
    db.with_conn(|c| {
        let n: i64 = c.query_row("SELECT COUNT(*) FROM campaigns WHERE client_id=?1", [id], |r| r.get(0))?;
        if n > 0 { return Err(anyhow!("klient má {n} kampaní – nejdřív je smažte nebo přiřaďte jinému klientovi")); }
        c.execute("DELETE FROM clients WHERE id=?1", [id])?;
        Ok(())
    })
}

/// Převod starých kampaní (klient jen jako text) do databáze klientů.
pub fn migrate_text_clients(db: &Db) -> Result<usize> {
    let names: Vec<String> = db.with_conn(|c| -> Result<_> {
        let mut st = c.prepare("SELECT DISTINCT trim(client) FROM campaigns WHERE client_id IS NULL AND trim(client) <> ''")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?)
    })?;
    let mut n = 0;
    for name in names {
        let existing: Option<i64> = db.with_conn(|c| c.query_row("SELECT id FROM clients WHERE lower(name)=lower(?1)", [&name], |r| r.get(0)).optional())?;
        let id = match existing { Some(id) => id, None => { n += 1; save_client(db, &Client { name: name.clone(), enabled: true, ..Default::default() })? } };
        db.with_conn(|c| c.execute("UPDATE campaigns SET client_id=?1 WHERE client_id IS NULL AND trim(client)=?2", params![id, name]))?;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// Obrázek a text spotu pro DAB+

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SpotMeta {
    pub track_id: i64,
    /// Obrázek pro DAB+ slideshow (zobrazí se po dobu spotu)
    pub image_path: String,
    /// Text pro DAB+ DLS a RDS RadioText po dobu spotu (prázdné = šablona „Reklamy, jingly a ostatní“)
    pub dls_text: String,
    /// Odkaz (ClickThroughURL ve slideshow)
    pub url: String,
}

pub fn spot_meta(db: &Db, track_id: i64) -> Result<Option<SpotMeta>> {
    db.with_conn(|c| Ok(c.query_row("SELECT track_id,image_path,dls_text,url FROM spot_meta WHERE track_id=?1", [track_id],
        |r| Ok(SpotMeta { track_id: r.get(0)?, image_path: r.get(1)?, dls_text: r.get(2)?, url: r.get(3)? })).optional()?))
}

pub fn spot_metas(db: &Db) -> Result<Vec<SpotMeta>> {
    db.with_conn(|c| -> Result<_> {
        let mut st = c.prepare("SELECT track_id,image_path,dls_text,url FROM spot_meta")?;
        Ok(st.query_map([], |r| Ok(SpotMeta { track_id: r.get(0)?, image_path: r.get(1)?, dls_text: r.get(2)?, url: r.get(3)? }))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn save_spot_meta(db: &Db, m: &SpotMeta) -> Result<()> {
    db.with_conn(|c| {
        if m.image_path.is_empty() && m.dls_text.trim().is_empty() && m.url.trim().is_empty() {
            c.execute("DELETE FROM spot_meta WHERE track_id=?1", [m.track_id])?;
        } else {
            c.execute("INSERT INTO spot_meta(track_id,image_path,dls_text,url) VALUES(?1,?2,?3,?4) ON CONFLICT(track_id) DO UPDATE SET image_path=?2, dls_text=?3, url=?4",
                params![m.track_id, m.image_path, m.dls_text.trim(), m.url.trim()])?;
        }
        Ok(())
    })
}

/// Uloží obrázek spotu do Media/spots a vrátí cestu.
/// `dir` = složka obrázků ke spotům (viz [`crate::station::images_dir`]).
pub fn store_spot_image(dir: &Path, track_id: i64, bytes: &[u8], ffmpeg: &Path) -> Result<String> {
    let (data, ext) = crate::slides::normalize_upload(bytes, ffmpeg)?;
    let dir = dir.to_path_buf();
    std::fs::create_dir_all(&dir)?;
    for e in std::fs::read_dir(&dir)?.flatten() {
        if e.file_name().to_string_lossy().starts_with(&format!("{track_id}-")) { let _ = std::fs::remove_file(e.path()); }
    }
    let p = dir.join(format!("{track_id}-{}.{ext}", chrono::Utc::now().timestamp()));
    std::fs::write(&p, data)?;
    Ok(p.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------------------------
// Obsazenost reklamních bloků

#[derive(Debug, Clone, Serialize)]
pub struct Availability { pub date: String, pub time: String, pub duration_ms: i64, pub commercial_ms: i64, pub max_s: u32, pub count: usize, pub warnings: usize }

pub fn availability(db: &Db, cfg: &Config, from: NaiveDate, days: u32) -> Result<Vec<Availability>> {
    let mut out = vec![];
    let mut breaks = cfg.spots.breaks.clone();
    breaks.sort_by(|a, b| a.time.cmp(&b.time));
    for d in 0..days.clamp(1, 31) {
        let date = from + chrono::Duration::days(d as i64);
        for b in breaks.iter().filter(|b| b.visible) {
            let blk = build_block(db, cfg, date, b, false)?;
            out.push(Availability { date: blk.date, time: blk.time, duration_ms: blk.duration_ms, commercial_ms: blk.commercial_ms, max_s: blk.max_duration_s, count: blk.items.iter().filter(|i| i.is_commercial).count(), warnings: blk.warnings.len() });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Protokol o odvysílání (pro klienta) – HTML k tisku / uložení do PDF

fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

#[derive(Debug, Clone, Serialize)]
pub struct PlayRow { pub started_at: String, pub campaign: String, pub spot: String, pub played_ms: i64, pub length_ms: i64 }

/// Odvysílané spoty klienta / kampaně za období (místní data "YYYY-MM-DD").
pub fn plays_for(db: &Db, client_id: Option<i64>, campaign_id: Option<i64>, from: &str, to: &str) -> Result<Vec<PlayRow>> {
    let camps: Vec<_> = list_campaigns(db, None, "")?.into_iter()
        .filter(|c| campaign_id.map(|id| c.id == id).unwrap_or(true) && client_id.map(|id| c.client_id == Some(id)).unwrap_or(true)).collect();
    let f = NaiveDate::parse_from_str(from, "%Y-%m-%d").map_err(|_| anyhow!("neplatné datum od"))?;
    let t = NaiveDate::parse_from_str(to, "%Y-%m-%d").map_err(|_| anyhow!("neplatné datum do"))?;
    let lo = crate::spots::day_bounds_pub(f).0;
    let hi = crate::spots::day_bounds_pub(t).1;
    let mut rows = vec![];
    db.with_conn(|c| -> Result<()> {
        for cp in &camps {
            let ids: Vec<String> = cp.spots.iter().map(|s| s.track_id.to_string()).collect();
            if ids.is_empty() { continue; }
            let since = cp.created_at.clone();
            let mut st = c.prepare(&format!("SELECT l.started_at, l.artist, l.title, l.played_ms, l.duration_ms FROM play_log l WHERE l.track_id IN ({}) AND l.started_at >= ?1 AND l.started_at < ?2 AND l.started_at >= ?3 ORDER BY l.started_at", ids.join(",")))?;
            let r = st.query_map(params![lo, hi, since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, i64>(4)?)))?;
            for x in r {
                let (at, a, ti, p, len) = x?;
                rows.push(PlayRow { started_at: at, campaign: cp.name.clone(), spot: if a.is_empty() { ti } else { format!("{a} - {ti}") }, played_ms: p.unwrap_or(len), length_ms: len });
            }
        }
        Ok(())
    })?;
    rows.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    Ok(rows)
}

pub fn proof_html(db: &Db, cfg: &Config, client_id: Option<i64>, campaign_id: Option<i64>, from: &str, to: &str) -> Result<String> {
    let client = client_id.map(|id| get_client(db, id)).transpose()?.flatten();
    let camp = campaign_id.map(|id| get_campaign(db, id)).transpose()?.flatten();
    let rows = plays_for(db, client_id, campaign_id, from, to)?;
    let fmt_d = |iso: &str| chrono::DateTime::parse_from_rfc3339(iso).map(|d| d.with_timezone(&chrono::Local).format("%d.%m.%Y").to_string()).unwrap_or_default();
    let fmt_t = |iso: &str| chrono::DateTime::parse_from_rfc3339(iso).map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default();
    let fmt_len = |ms: i64| { let s = (ms + 500) / 1000; format!("{}:{:02}", s / 60, s % 60) };
    let logo = std::fs::read(&cfg.meta.logo_path).ok().map(|b| {
        use base64::Engine;
        let mime = if cfg.meta.logo_path.to_lowercase().ends_with(".png") { "image/png" } else { "image/jpeg" };
        format!("<img class=\"logo\" src=\"data:{mime};base64,{}\">", base64::engine::general_purpose::STANDARD.encode(b))
    }).unwrap_or_default();
    // souhrn po kampaních
    let mut sums: Vec<(String, usize, i64, f64, i64)> = vec![];
    let camps = list_campaigns(db, None, "")?;
    for r in &rows {
        let price = camps.iter().find(|c| c.name == r.campaign).map(|c| c.price_per_spot).unwrap_or(0.0);
        let ordered = camps.iter().find(|c| c.name == r.campaign).map(|c| c.ordered_plays).unwrap_or(0);
        match sums.iter_mut().find(|s| s.0 == r.campaign) { Some(s) => { s.1 += 1; s.2 += r.played_ms; } None => sums.push((r.campaign.clone(), 1, r.played_ms, price, ordered)) }
    }
    let title = match (&client, &camp) {
        (_, Some(c)) => format!("Kampaň {}", esc(&c.name)),
        (Some(c), None) => format!("Klient {}", esc(&c.name)),
        _ => "Všechny kampaně".into(),
    };
    let mut h = String::new();
    h += "<!DOCTYPE html><html lang=\"cs\"><head><meta charset=\"utf-8\"><title>Protokol o odvysílání</title><style>";
    h += "body{font-family:-apple-system,Segoe UI,Roboto,Arial,sans-serif;color:#111;margin:32px;font-size:13px}h1{font-size:22px;margin:0}h2{font-size:15px;margin:22px 0 6px}";
    h += ".head{display:flex;justify-content:space-between;align-items:center;border-bottom:3px solid #111;padding-bottom:12px}.logo{max-height:64px;max-width:200px}";
    h += "table{border-collapse:collapse;width:100%}th,td{border-bottom:1px solid #ddd;padding:4px 6px;text-align:left}th{background:#f3f4f6;font-size:11px;text-transform:uppercase}td.n{text-align:right;font-variant-numeric:tabular-nums}";
    h += ".meta{display:grid;grid-template-columns:160px 1fr;gap:2px 12px;margin-top:12px}.muted{color:#666}.sign{margin-top:48px;display:flex;justify-content:space-between}.sign div{border-top:1px solid #111;width:40%;padding-top:4px;text-align:center}";
    h += "@media print{button{display:none}body{margin:12mm}}</style></head><body>";
    h += "<button onclick=\"print()\" style=\"float:right;padding:6px 14px\">Tisk / PDF</button>";
    h += &format!("<div class=\"head\"><div><h1>Protokol o odvysílání reklamy</h1><div class=\"muted\">{}</div></div>{logo}</div>", esc(&cfg.meta.station_long));
    h += "<div class=\"meta\">";
    h += &format!("<b>Předmět</b><span>{title}</span>");
    if let Some(c) = &client {
        h += &format!("<b>Klient</b><span>{}{}</span>", esc(&c.name), if c.company.is_empty() { String::new() } else { format!(" ({})", esc(&c.company)) });
        if !c.ico.is_empty() { h += &format!("<b>IČO / DIČ</b><span>{} {}</span>", esc(&c.ico), esc(&c.dic)); }
        if !c.address.is_empty() { h += &format!("<b>Adresa</b><span>{}</span>", esc(&c.address)); }
    }
    h += &format!("<b>Období</b><span>{} – {}</span>", from, to);
    h += &format!("<b>Vystaveno</b><span>{}</span>", chrono::Local::now().format("%d.%m.%Y %H:%M"));
    h += "</div><h2>Souhrn</h2><table><tr><th>Kampaň</th><th class=\"n\">Odvysíláno</th><th class=\"n\">Objednáno</th><th class=\"n\">Celkem čas</th><th class=\"n\">Cena / spot</th><th class=\"n\">Celkem</th></tr>";
    let mut total = (0usize, 0i64, 0f64);
    for (n, cnt, ms, price, ordered) in &sums {
        h += &format!("<tr><td>{}</td><td class=\"n\">{cnt}×</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td></tr>", esc(n), if *ordered > 0 { format!("{ordered}×") } else { "–".into() }, fmt_len(*ms),
            if *price > 0.0 { format!("{price:.2} Kč") } else { "–".into() }, if *price > 0.0 { format!("{:.2} Kč", price * *cnt as f64) } else { "–".into() });
        total.0 += cnt; total.1 += ms; total.2 += price * *cnt as f64;
    }
    h += &format!("<tr><th>Celkem</th><th class=\"n\">{}×</th><th></th><th class=\"n\">{}</th><th></th><th class=\"n\">{}</th></tr></table>", total.0, fmt_len(total.1), if total.2 > 0.0 { format!("{:.2} Kč", total.2) } else { "–".into() });
    h += "<h2>Výpis odvysílání</h2><table><tr><th>#</th><th>Datum</th><th>Čas</th><th>Kampaň</th><th>Spot</th><th class=\"n\">Délka</th></tr>";
    for (i, r) in rows.iter().enumerate() {
        h += &format!("<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td></tr>", i + 1, fmt_d(&r.started_at), fmt_t(&r.started_at), esc(&r.campaign), esc(&r.spot), fmt_len(r.played_ms));
    }
    if rows.is_empty() { h += "<tr><td colspan=\"6\" class=\"muted\">V období nebyl odvysílán žádný spot.</td></tr>"; }
    h += "</table><p class=\"muted\">Časy jsou podle automatického záznamu vysílacího systému ORCA RadioAIR (skutečný začátek přehrání).</p>";
    h += "<div class=\"sign\"><div>Za vysílatele</div><div>Razítko a podpis</div></div></body></html>";
    let _ = spot_len;
    Ok(h)
}
