//! Exporty "co hraje" (Jazler Internet Updater): NowOnAir.txt/.xml/.json, AirPlayHistory, AirPlayNext,
//! CurrentAlbumImage.jpg a webhooky.

use crate::config::Config;
use crate::library::read_cover;
use crate::model::{QueueItem, Track};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const NO_UPDATE: &str = "NO-UPDATE";

/// Veřejný text pro daný typ: None = neaktualizovat.
pub fn public_text(cfg: &Config, t: &Track) -> Option<String> {
    let rule = cfg.nowplaying.per_kind.get(&t.kind()).cloned().unwrap_or_default();
    if rule == NO_UPDATE { None } else if rule.is_empty() { Some(t.display()) } else { Some(rule) }
}

pub fn export_dir(cfg: &Config, data_dir: &Path) -> PathBuf {
    if cfg.nowplaying.export_dir.as_os_str().is_empty() { data_dir.join("Exports") } else { cfg.nowplaying.export_dir.clone() }
}

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn track_json(t: &Track, text: &str) -> serde_json::Value {
    json!({
        "text": text, "artist": t.artist, "title": t.title, "album": t.album, "year": t.year,
        "kind": t.kind().as_str(), "category": t.category, "duration_ms": t.duration_ms, "isrc": t.isrc,
    })
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub struct NowPlayingWriter { pub data_dir: PathBuf }

impl NowPlayingWriter {
    /// Volá se při startu skladby. `history` = posledních N veřejných položek (nejnovější první).
    pub fn update(&self, cfg: &Config, current: &Track, text: &str, history: &[(String, Track)], next: &[QueueItem]) {
        if !cfg.nowplaying.enabled { return; }
        let dir = export_dir(cfg, &self.data_dir);
        if let Err(e) = std::fs::create_dir_all(&dir) { log::warn!("export složka {}: {e}", dir.display()); return; }
        let now = chrono::Local::now().to_rfc3339();
        let _ = write_atomic(&dir.join("NowOnAir.txt"), text.as_bytes());
        let j = json!({"station": cfg.station.name, "updated_at": now, "now": track_json(current, text)});
        let _ = write_atomic(&dir.join("NowOnAir.json"), serde_json::to_vec_pretty(&j).unwrap_or_default().as_slice());
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Schedule System=\"ORCA RadioAIR\">\n  <Event status=\"happening\" startTime=\"{}\" eventType=\"{}\">\n    <Announcement Display=\"{}\"/>\n    <Song title=\"{}\">\n      <Artist name=\"{}\"/>\n      <Album name=\"{}\"/>\n      <Info duration_ms=\"{}\" category=\"{}\" isrc=\"{}\"/>\n    </Song>\n  </Event>\n</Schedule>\n",
            xml_esc(&now), current.kind().as_str(), xml_esc(text), xml_esc(&current.title), xml_esc(&current.artist), xml_esc(&current.album), current.duration_ms, xml_esc(&current.category), xml_esc(&current.isrc)
        );
        let _ = write_atomic(&dir.join("NowOnAir.xml"), xml.as_bytes());

        let hist: Vec<_> = history.iter().take(cfg.nowplaying.history_size).map(|(at, t)| { let mut v = track_json(t, &t.display()); v["started_at"] = json!(at); v }).collect();
        let _ = write_atomic(&dir.join("AirPlayHistory.json"), serde_json::to_vec_pretty(&json!({"items": hist})).unwrap_or_default().as_slice());
        let mut hx = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<History>\n");
        for (at, t) in history.iter().take(cfg.nowplaying.history_size) {
            hx += &format!("  <Song startTime=\"{}\" title=\"{}\" artist=\"{}\" album=\"{}\"/>\n", xml_esc(at), xml_esc(&t.title), xml_esc(&t.artist), xml_esc(&t.album));
        }
        hx += "</History>\n";
        let _ = write_atomic(&dir.join("AirPlayHistory.xml"), hx.as_bytes());

        let nx: Vec<_> = next.iter().filter_map(|q| public_text(cfg, &q.track).map(|txt| { let mut v = track_json(&q.track, &txt); v["eta"] = json!(q.eta); v })).take(cfg.nowplaying.next_size).collect();
        let _ = write_atomic(&dir.join("AirPlayNext.json"), serde_json::to_vec_pretty(&json!({"items": nx})).unwrap_or_default().as_slice());
        let mut nxml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Next>\n");
        for q in next.iter().filter(|q| public_text(cfg, &q.track).is_some()).take(cfg.nowplaying.next_size) {
            nxml += &format!("  <Song eta=\"{}\" title=\"{}\" artist=\"{}\"/>\n", xml_esc(q.eta.as_deref().unwrap_or("")), xml_esc(&q.track.title), xml_esc(&q.track.artist));
        }
        nxml += "</Next>\n";
        let _ = write_atomic(&dir.join("AirPlayNext.xml"), nxml.as_bytes());

        if cfg.nowplaying.export_cover {
            match read_cover(Path::new(&current.path)) {
                Some((data, mime)) => {
                    let ext = if mime.contains("png") { "png" } else { "jpg" };
                    let _ = std::fs::remove_file(dir.join(if ext == "png" { "CurrentAlbumImage.jpg" } else { "CurrentAlbumImage.png" }));
                    let _ = write_atomic(&dir.join(format!("CurrentAlbumImage.{ext}")), &data);
                }
                None => { let _ = std::fs::remove_file(dir.join("CurrentAlbumImage.jpg")); let _ = std::fs::remove_file(dir.join("CurrentAlbumImage.png")); }
            }
        }

        for url in &cfg.nowplaying.webhooks {
            let body = json!({"event": "track_started", "station": cfg.station.name, "at": now, "now": track_json(current, text)});
            let url = url.clone();
            std::thread::spawn(move || {
                if let Err(e) = ureq::post(&url).timeout(std::time::Duration::from_secs(5)).send_json(body) { log::warn!("webhook {url}: {e}"); }
            });
        }
    }
}
