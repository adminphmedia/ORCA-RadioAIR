//! SPI / EPG pro DAB+ a RadioDNS (ETSI TS 102 818 v3.1): SI.xml (informace o službě) a PI (program na den).

use crate::config::Config;
use crate::db::Db;
use crate::sched::{program_for_day, ProgramSlot};
use anyhow::Result;
use chrono::{Local, NaiveDate};

pub fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

fn cut(s: &str, n: usize) -> String { s.chars().take(n).collect() }

/// GCC = první znak SId + ECC (např. 2 + E2 = 2e2)
pub fn gcc(cfg: &Config) -> String {
    let sid = cfg.meta.dab.sid.trim();
    format!("{}{}", sid.chars().next().unwrap_or('0'), cfg.meta.dab.ecc.trim()).to_lowercase()
}

pub fn dab_bearer(cfg: &Config) -> String {
    let d = &cfg.meta.dab;
    format!("dab:{}.{}.{}.{}", gcc(cfg), d.eid.trim().to_lowercase(), d.sid.trim().to_lowercase(), d.scids)
}

pub fn fm_bearer(cfg: &Config) -> Option<String> {
    let f: f64 = cfg.meta.dab.fm_freq.replace(',', ".").trim().parse().ok()?;
    let pi = cfg.meta.rds.pi.trim().to_lowercase();
    Some(format!("fm:{}.{}.{:05}", gcc(cfg), pi, (f * 100.0).round() as i64))
}

/// Cesta souboru PI podle RadioDNS: dab/2e2/1001/2001/0
pub fn bearer_path(cfg: &Config) -> String {
    dab_bearer(cfg).replacen("dab:", "dab/", 1).replace('.', "/")
}

fn base(cfg: &Config) -> String { cfg.meta.dab.public_url.trim_end_matches('/').to_string() }

fn img(cfg: &Config, path: &str, w: u32, h: u32) -> String {
    format!("<multimedia url=\"{}\" type=\"logo_unrestricted\" mimeValue=\"{}\" width=\"{w}\" height=\"{h}\"/>",
        esc(&format!("{}{path}{}w={w}&h={h}", base(cfg), if path.contains('?') { "&" } else { "?" })), if w == h && w <= 128 { "image/png" } else { "image/jpeg" })
}

pub fn si_xml(cfg: &Config) -> String {
    let m = &cfg.meta;
    let now = Local::now().to_rfc3339();
    let mut s = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<serviceInformation xmlns=\"http://www.worlddab.org/schemas/spi\" version=\"1\" creationTime=\"{now}\" originator=\"ORCA RadioAIR\">\n  <services>\n    <service>\n");
    s += &format!("      <shortName>{}</shortName>\n      <mediumName>{}</mediumName>\n      <longName>{}</longName>\n", esc(&cut(&m.station_short, 8)), esc(&cut(&m.station_medium, 16)), esc(&cut(&m.station_long, 128)));
    if !m.description.is_empty() || !m.slogan.is_empty() {
        let d = if m.description.is_empty() { &m.slogan } else { &m.description };
        s += &format!("      <mediaDescription><shortDescription>{}</shortDescription></mediaDescription>\n", esc(&cut(d, 180)));
    }
    if !m.logo_path.is_empty() && !base(cfg).is_empty() {
        for (w, h) in [(32, 32), (112, 32), (128, 128), (320, 240), (600, 600)] {
            s += &format!("      <mediaDescription>{}</mediaDescription>\n", img(cfg, "/meta/image/logo", w, h));
        }
    }
    if !m.url.is_empty() { let u = if m.url.contains("://") { m.url.clone() } else { format!("https://{}", m.url) }; s += &format!("      <link uri=\"{}\"/>\n", esc(&u)); }
    s += &format!("      <bearer id=\"{}\" cost=\"20\"/>\n", dab_bearer(cfg));
    if let Some(fm) = fm_bearer(cfg) { s += &format!("      <bearer id=\"{fm}\" cost=\"40\"/>\n"); }
    for e in cfg.encoders.iter().filter(|e| e.enabled && e.public) {
        let mime = match e.format { crate::config::StreamFormat::Mp3 => "audio/mpeg", crate::config::StreamFormat::Aac => "audio/aacp", _ => "audio/ogg" };
        s += &format!("      <bearer id=\"{}\" cost=\"60\" mimeValue=\"{mime}\" bitrate=\"{}\"/>\n", esc(&crate::encoder::public_url(e)), e.bitrate_kbps);
    }
    s += "    </service>\n  </services>\n</serviceInformation>\n";
    s
}

fn iso_dur(ms: i64) -> String {
    let m = (ms / 60_000).max(1);
    format!("PT{}H{}M", m / 60, m % 60).replace("PT0H", "PT").replace("H0M", "H")
}

pub fn pi_xml_for(cfg: &Config, date: NaiveDate, slots: &[ProgramSlot]) -> String {
    let bearer = dab_bearer(cfg);
    let start = date.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).single().map(|d| d.to_rfc3339()).unwrap_or_default();
    let end = date.succ_opt().unwrap_or(date).and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).single().map(|d| d.to_rfc3339()).unwrap_or_default();
    let lang = &cfg.station.language;
    let mut s = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<programmeInformation xmlns=\"http://www.worlddab.org/schemas/spi\" xml:lang=\"{lang}\" creationTime=\"{}\" originator=\"ORCA RadioAIR\">\n  <schedule>\n    <scope startTime=\"{start}\" stopTime=\"{end}\">\n      <serviceScope id=\"{bearer}\"/>\n    </scope>\n", Local::now().to_rfc3339());
    for (i, p) in slots.iter().enumerate() {
        let sid = p.show.id.unwrap_or(0);
        let crid = format!("crid://{}/show/{sid}/{}", cfg.meta.station_short.to_lowercase().replace(' ', ""), p.start.format("%Y%m%dT%H%M"));
        let short = if p.show.short_name.is_empty() { cut(&p.show.name, 8) } else { cut(&p.show.short_name, 8) };
        s += &format!("    <programme shortId=\"{}\" id=\"{}\" version=\"1\">\n", (date.format("%j").to_string().parse::<u32>().unwrap_or(1) * 100 + i as u32 + 1), esc(&crid));
        s += &format!("      <shortName>{}</shortName>\n      <mediumName>{}</mediumName>\n      <longName>{}</longName>\n", esc(&short), esc(&cut(&p.show.name, 16)), esc(&cut(&p.show.name, 128)));
        s += &format!("      <location>\n        <time time=\"{}\" duration=\"{}\"/>\n        <bearer id=\"{bearer}\"/>\n      </location>\n", p.start.to_rfc3339(), iso_dur((p.end - p.start).num_milliseconds()));
        let hosts: Vec<String> = p.presenters.iter().map(|x| x.name.clone()).collect();
        let mut long = p.description.clone();
        if !long.is_empty() && !long.ends_with(['.', '!', '?']) { long.push('.'); }
        if !hosts.is_empty() { long = format!("{}{}{}: {}", long, if long.is_empty() { "" } else { " " }, if lang == "en" { "Presented by" } else { "Moderuje" }, hosts.join(", ")); }
        if !p.description.is_empty() { s += &format!("      <mediaDescription><shortDescription>{}</shortDescription></mediaDescription>\n", esc(&cut(&p.description, 180))); }
        if !long.is_empty() { s += &format!("      <mediaDescription><longDescription>{}</longDescription></mediaDescription>\n", esc(&cut(&long, 1200))); }
        if !base(cfg).is_empty() {
            if !p.show.image_path.is_empty() {
                for (w, h) in [(320, 240), (600, 600)] { s += &format!("      <mediaDescription>{}</mediaDescription>\n", img(cfg, &format!("/meta/image/show/{sid}"), w, h)); }
            } else if let Some(pr) = p.presenters.iter().find(|x| !x.photo_path.is_empty()) {
                s += &format!("      <mediaDescription>{}</mediaDescription>\n", img(cfg, &format!("/meta/image/presenter/{}", pr.id.unwrap_or(0)), 320, 240));
            }
        }
        if !p.show.genre.is_empty() { s += &format!("      <genre href=\"urn:tva:metadata:cs:ContentCS:2011:3\"><name>{}</name></genre>\n", esc(&p.show.genre)); }
        if !p.show.url.is_empty() { s += &format!("      <link uri=\"{}\"/>\n", esc(&p.show.url)); }
        s += "    </programme>\n";
    }
    s += "  </schedule>\n</programmeInformation>\n";
    s
}

pub fn pi_xml(cfg: &Config, db: &Db, date: NaiveDate) -> Result<String> {
    Ok(pi_xml_for(cfg, date, &program_for_day(db, date)?))
}

/// Zapíše SPI soubory do složky (pro EPG kodér multiplexu): SI.xml, YYYYMMDD_PI.xml a strukturu RadioDNS.
pub fn write_files(cfg: &Config, db: &Db, dir: &std::path::Path) -> Result<usize> {
    let epg = dir.join("epg");
    std::fs::create_dir_all(&epg)?;
    let write = |p: std::path::PathBuf, s: &str| -> Result<()> { if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; } let t = p.with_extension("tmp"); std::fs::write(&t, s)?; std::fs::rename(&t, &p)?; Ok(()) };
    write(epg.join("SI.xml"), &si_xml(cfg))?;
    let bp = bearer_path(cfg);
    let today = Local::now().date_naive();
    let mut n = 1;
    for i in 0..cfg.meta.dab.epg_days.clamp(1, 14) {
        let d = today + chrono::Duration::days(i as i64);
        let x = pi_xml(cfg, db, d)?;
        let name = format!("{}_PI.xml", d.format("%Y%m%d"));
        write(epg.join(&name), &x)?;
        write(epg.join("radiodns/spi/3.1/id").join(&bp).join(&name), &x)?;
        n += 1;
    }
    write(epg.join("radiodns/spi/3.1/SI.xml"), &si_xml(cfg))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bearers() {
        let mut c = Config::default();
        c.meta.dab.sid = "2F01".into(); c.meta.dab.eid = "1001".into(); c.meta.dab.ecc = "E2".into();
        c.meta.rds.pi = "2F01".into(); c.meta.dab.fm_freq = "96.3".into();
        assert_eq!(dab_bearer(&c), "dab:2e2.1001.2f01.0");
        assert_eq!(fm_bearer(&c).unwrap(), "fm:2e2.2f01.09630");
        assert_eq!(bearer_path(&c), "dab/2e2/1001/2f01/0");
        assert_eq!(iso_dur(3 * 3_600_000), "PT3H");
        assert_eq!(iso_dur(90 * 60_000), "PT1H30M");
        assert!(si_xml(&c).contains("<bearer id=\"dab:2e2.1001.2f01.0\""));
    }
}
