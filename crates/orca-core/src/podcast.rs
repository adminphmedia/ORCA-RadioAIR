//! Editor nahrávek a podcast.
//!
//! Úprava (projekt) je nedestruktivní: původní nahrávka zůstává, ukládá se jen seznam úprav –
//! ořez začátku a konce, vystřižené úseky, úvodní a závěrečná znělka, sweeper přes začátek
//! (se ztlumením), prolnutí, roztmívání/stmívání a cílová hlasitost. Epizoda se z úprav vyrobí
//! přes ffmpeg. Podcast = seznam epizod + RSS feed (Apple Podcasts / Spotify), zveřejnění
//! přes FTP / FTPS / SFTP (služby z Now playing) a/nebo přímo z ORCA.

use crate::config::Config;
use crate::db::Db;
use crate::encoder::resolve_ffmpeg;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PodcastConfig {
    pub title: String,
    pub description: String,
    pub author: String,
    pub email: String,
    /// cs, en, sk…
    pub language: String,
    /// kategorie Apple Podcasts (např. News, Music, Society & Culture)
    pub category: String,
    pub explicit: bool,
    /// veřejná adresa, kde budou soubory ležet (např. https://mojeradio.cz/podcast) – z ní se skládají odkazy ve feedu
    pub base_url: String,
    /// id služby FTP/FTPS/SFTP z Now playing (prázdné = nenahrávat)
    pub upload_service: String,
    /// podsložka na serveru (připojí se ke složce služby)
    pub remote_dir: String,
    /// výchozí úpravy nových nahrávek
    pub default_intro: Option<i64>,
    pub default_outro: Option<i64>,
    pub default_sweeper: Option<i64>,
    pub loudness_lufs: f32,
    pub bitrate_kbps: u32,
    /// obrázek podcastu (soubor ve složce Podcast)
    pub image: String,
}

impl Default for PodcastConfig {
    fn default() -> Self {
        Self { title: String::new(), description: String::new(), author: String::new(), email: String::new(), language: "cs".into(), category: "News".into(), explicit: false,
            base_url: String::new(), upload_service: String::new(), remote_dir: String::new(), default_intro: None, default_outro: None, default_sweeper: None, loudness_lufs: -16.0, bitrate_kbps: 128, image: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Cut { pub a_ms: i64, pub b_ms: i64 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Project {
    pub id: String,
    pub title: String,
    pub description: String,
    /// původní nahrávka (absolutní cesta)
    pub source: String,
    pub source_ms: i64,
    pub trim_in_ms: i64,
    /// 0 = do konce
    pub trim_out_ms: i64,
    /// vystřižené úseky (v čase původní nahrávky)
    pub cuts: Vec<Cut>,
    pub intro_track_id: Option<i64>,
    pub outro_track_id: Option<i64>,
    pub sweeper_track_id: Option<i64>,
    /// kdy po začátku pořadu sweeper zazní (ms)
    pub sweeper_offset_ms: i64,
    /// o kolik dB ztlumit pořad pod sweeperem
    pub sweeper_duck_db: f32,
    /// prolnutí znělek s pořadem (ms)
    pub crossfade_ms: i64,
    pub fade_in_ms: i64,
    pub fade_out_ms: i64,
    /// vyrobená epizoda (název souboru ve složce Podcast/episodes)
    pub episode: String,
    pub created: String,
    pub updated: String,
}

impl Default for Project {
    fn default() -> Self {
        Self { id: String::new(), title: String::new(), description: String::new(), source: String::new(), source_ms: 0, trim_in_ms: 0, trim_out_ms: 0, cuts: vec![], intro_track_id: None, outro_track_id: None,
            sweeper_track_id: None, sweeper_offset_ms: 2000, sweeper_duck_db: -12.0, crossfade_ms: 1500, fade_in_ms: 300, fade_out_ms: 1500, episode: String::new(), created: String::new(), updated: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Episode {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub file: String,
    pub bytes: u64,
    pub duration_ms: i64,
    pub pub_date: String,
    pub published: bool,
    pub uploaded: bool,
}

pub fn root(data_dir: &Path) -> PathBuf { data_dir.join("Podcast") }
fn projects_dir(d: &Path) -> PathBuf { root(d).join("projects") }
pub fn episodes_dir(d: &Path) -> PathBuf { root(d).join("episodes") }
fn episodes_file(d: &Path) -> PathBuf { root(d).join("episodes.json") }

pub fn list_projects(d: &Path) -> Vec<Project> {
    let mut v: Vec<Project> = std::fs::read_dir(projects_dir(d)).map(|rd| rd.flatten().filter_map(|e| std::fs::read_to_string(e.path()).ok()).filter_map(|t| serde_json::from_str(&t).ok()).collect()).unwrap_or_default();
    v.sort_by(|a, b| b.created.cmp(&a.created));
    v
}
pub fn get_project(d: &Path, id: &str) -> Result<Project> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') { return Err(anyhow!("neplatné id")); }
    Ok(serde_json::from_str(&std::fs::read_to_string(projects_dir(d).join(format!("{id}.json"))).context("úprava neexistuje")?)?)
}
pub fn save_project(d: &Path, p: &mut Project) -> Result<()> {
    std::fs::create_dir_all(projects_dir(d))?;
    if p.id.is_empty() { p.id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string(); p.created = chrono::Local::now().to_rfc3339(); }
    p.updated = chrono::Local::now().to_rfc3339();
    p.cuts.retain(|c| c.b_ms > c.a_ms);
    p.cuts.sort_by_key(|c| c.a_ms);
    std::fs::write(projects_dir(d).join(format!("{}.json", p.id)), serde_json::to_vec_pretty(p)?)?;
    Ok(())
}
pub fn delete_project(d: &Path, id: &str) -> Result<()> { let _ = get_project(d, id)?; std::fs::remove_file(projects_dir(d).join(format!("{id}.json")))?; Ok(()) }

pub fn list_episodes(d: &Path) -> Vec<Episode> { std::fs::read_to_string(episodes_file(d)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default() }
pub fn save_episodes(d: &Path, v: &[Episode]) -> Result<()> { std::fs::create_dir_all(root(d))?; std::fs::write(episodes_file(d), serde_json::to_vec_pretty(v)?)?; Ok(()) }

/// Délka souboru (ms) přes ffmpeg.
pub fn probe_ms(cfg: &Config, path: &Path) -> Result<i64> {
    let o = Command::new(resolve_ffmpeg(cfg)).args(["-hide_banner", "-i"]).arg(path).output()?;
    let err = String::from_utf8_lossy(&o.stderr);
    let d = err.lines().find_map(|l| l.trim().strip_prefix("Duration: ").map(|x| x.split(',').next().unwrap_or("").to_string())).ok_or_else(|| anyhow!("délku souboru nelze zjistit"))?;
    let parts: Vec<f64> = d.split(':').filter_map(|x| x.parse().ok()).collect();
    if parts.len() != 3 { return Err(anyhow!("délku souboru nelze zjistit")); }
    Ok(((parts[0] * 3600.0 + parts[1] * 60.0 + parts[2]) * 1000.0) as i64)
}

/// Obálka (špičky) pro editor: `n` hodnot 0..1 pro úsek [from, to) ms.
pub fn peaks(cfg: &Config, path: &Path, from_ms: i64, to_ms: i64, n: usize) -> Result<Vec<f32>> {
    let n = n.clamp(10, 8000);
    let len = (to_ms - from_ms).max(1);
    // vzorkovací frekvence tak, aby na jeden bod připadlo aspoň ~8 vzorků
    let rate = ((n as i64 * 8 * 1000) / len).clamp(200, 8000);
    let o = Command::new(resolve_ffmpeg(cfg)).args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{:.3}", from_ms as f64 / 1000.0), "-t", &format!("{:.3}", len as f64 / 1000.0), "-i"]).arg(path)
        .args(["-vn", "-ac", "1", "-ar", &rate.to_string(), "-f", "s16le", "-"]).output()?;
    if !o.status.success() { return Err(anyhow!("soubor nelze načíst: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""))); }
    let samples: Vec<i16> = o.stdout.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    let total = (len as f64 / 1000.0 * rate as f64).max(1.0);
    let mut out = vec![0f32; n];
    for (i, s) in samples.iter().enumerate() {
        let b = ((i as f64 / total) * n as f64) as usize;
        if b < n { let v = (*s as f32).abs() / 32768.0; if v > out[b] { out[b] = v; } }
    }
    Ok(out)
}

/// Zachované úseky pořadu (sekundy v původní nahrávce).
pub fn kept_ranges(p: &Project) -> Vec<(f64, f64)> {
    let end = if p.trim_out_ms > 0 { p.trim_out_ms } else { p.source_ms.max(p.trim_in_ms + 1) };
    let mut v = vec![]; let mut cur = p.trim_in_ms;
    for c in &p.cuts {
        let (a, b) = (c.a_ms.max(p.trim_in_ms), c.b_ms.min(end));
        if b <= a { continue; }
        if a > cur { v.push((cur as f64 / 1000.0, a as f64 / 1000.0)); }
        cur = cur.max(b);
    }
    if end > cur { v.push((cur as f64 / 1000.0, end as f64 / 1000.0)); }
    v
}

/// Výsledná délka pořadu po úpravách (bez znělek).
pub fn body_ms(p: &Project) -> i64 { (kept_ranges(p).iter().map(|(a, b)| b - a).sum::<f64>() * 1000.0) as i64 }

/// Vyrobí epizodu (nebo náhled prvních `preview_s` sekund) a vrátí cestu.
pub fn render(cfg: &Config, db: &Db, data_dir: &Path, p: &Project, preview_s: Option<u32>) -> Result<PathBuf> {
    let src = PathBuf::from(&p.source);
    if !src.is_file() { return Err(anyhow!("původní nahrávka chybí: {}", p.source)); }
    let ranges = kept_ranges(p);
    if ranges.is_empty() { return Err(anyhow!("po úpravách nic nezbylo")); }
    let track_path = |id: Option<i64>| -> Result<Option<(PathBuf, i64)>> {
        let Some(id) = id else { return Ok(None) };
        let t = db.get_track(id)?.ok_or_else(|| anyhow!("znělka {id} není v knihovně"))?;
        if !Path::new(&t.path).is_file() { return Err(anyhow!("soubor znělky chybí: {}", t.path)); }
        Ok(Some((PathBuf::from(&t.path), t.duration_ms)))
    };
    let intro = track_path(p.intro_track_id)?; let outro = track_path(p.outro_track_id)?; let sweep = track_path(p.sweeper_track_id)?;
    let pc = &cfg.podcast;
    let dir = if preview_s.is_some() { std::env::temp_dir() } else { episodes_dir(data_dir) };
    std::fs::create_dir_all(&dir)?;
    let stem = slug(&format!("{}-{}", chrono::Local::now().format("%Y-%m-%d"), if p.title.is_empty() { "epizoda" } else { &p.title }));
    let out = if preview_s.is_some() { dir.join(format!("orca-podcast-preview-{}.mp3", p.id)) } else { crate::library::unique_path(&dir, &format!("{stem}.mp3"), "rename").ok_or_else(|| anyhow!("nelze vytvořit název"))? };

    let mut cmd = Command::new(resolve_ffmpeg(cfg));
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(&src);
    let mut idx = 1; let (mut ii, mut oi, mut si) = (None, None, None);
    if let Some((pth, _)) = &intro { cmd.arg("-i").arg(pth); ii = Some(idx); idx += 1; }
    if let Some((pth, _)) = &outro { cmd.arg("-i").arg(pth); oi = Some(idx); idx += 1; }
    if let Some((pth, _)) = &sweep { cmd.arg("-i").arg(pth); si = Some(idx); }
    let fmt = "aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=stereo";
    let sel = ranges.iter().map(|(a, b)| format!("between(t,{a:.3},{b:.3})")).collect::<Vec<_>>().join("+");
    let body_s = ranges.iter().map(|(a, b)| b - a).sum::<f64>();
    let mut f = format!("[0:a]{fmt},aselect='{sel}',asetpts=N/SR/TB");
    if p.fade_in_ms > 0 { f += &format!(",afade=t=in:d={:.2}", p.fade_in_ms as f64 / 1000.0); }
    if p.fade_out_ms > 0 { f += &format!(",afade=t=out:st={:.2}:d={:.2}", (body_s - p.fade_out_ms as f64 / 1000.0).max(0.0), p.fade_out_ms as f64 / 1000.0); }
    // sweeper přes začátek pořadu se ztlumením pořadu
    if let (Some(s), Some((_, sd))) = (si, &sweep) {
        let off = p.sweeper_offset_ms.max(0) as f64 / 1000.0; let dur = *sd as f64 / 1000.0;
        let duck = 10f64.powf(p.sweeper_duck_db.min(0.0) as f64 / 20.0);
        f += &format!(",volume=enable='between(t,{off:.2},{:.2})':volume={duck:.3}[body0];[{s}:a]{fmt},adelay={}|{}[sw];[body0][sw]amix=inputs=2:duration=first:normalize=0[body]", off + dur, (off * 1000.0) as i64, (off * 1000.0) as i64);
    } else { f += "[body]"; }
    let xf = (p.crossfade_ms.max(0) as f64 / 1000.0).min(10.0);
    let mut last = "body".to_string();
    if let Some(i) = ii {
        f += &format!(";[{i}:a]{fmt}[intro]");
        if xf > 0.05 { f += &format!(";[intro][{last}]acrossfade=d={xf:.2}:c1=tri:c2=tri[a1]"); } else { f += &format!(";[intro][{last}]concat=n=2:v=0:a=1[a1]"); }
        last = "a1".into();
    }
    if let Some(o) = oi {
        f += &format!(";[{o}:a]{fmt}[outro]");
        if xf > 0.05 { f += &format!(";[{last}][outro]acrossfade=d={xf:.2}:c1=tri:c2=tri[a2]"); } else { f += &format!(";[{last}][outro]concat=n=2:v=0:a=1[a2]"); }
        last = "a2".into();
    }
    f += &format!(";[{last}]loudnorm=I={:.1}:TP=-1.5:LRA=11[out]", pc.loudness_lufs.clamp(-30.0, -10.0));
    cmd.args(["-filter_complex", &f, "-map", "[out]"]);
    if let Some(s) = preview_s { cmd.args(["-t", &s.to_string()]); }
    cmd.args(["-c:a", "libmp3lame", "-b:a", &format!("{}k", pc.bitrate_kbps.clamp(64, 320)), "-ar", "44100"]);
    // metadata epizody
    cmd.args(["-metadata", &format!("title={}", p.title), "-metadata", &format!("artist={}", if pc.author.is_empty() { &cfg.station.name } else { &pc.author }), "-metadata", &format!("album={}", if pc.title.is_empty() { &cfg.station.name } else { &pc.title })]);
    cmd.arg(&out);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
    let o = cmd.output()?;
    if !o.status.success() { return Err(anyhow!("výroba epizody selhala: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""))); }
    Ok(out)
}

/// Název souboru vhodný pro web: bez diakritiky a mezer (Ranní show 26. 11. → ranni-show-26-11).
pub fn slug(s: &str) -> String {
    const FROM: &str = "áäčďéěëíľĺňóôöŕřšťúůüýžÁÄČĎÉĚËÍĽĹŇÓÔÖŔŘŠŤÚŮÜÝŽ";
    const TO: &str = "aacdeeeillnooorrstuuuyzAACDEEEILLNOOORRSTUUUYZ";
    let mut out = String::new();
    for c in s.chars() {
        let c = FROM.chars().position(|x| x == c).and_then(|i| TO.chars().nth(i)).unwrap_or(c);
        if c.is_ascii_alphanumeric() { out.push(c.to_ascii_lowercase()); } else if !out.ends_with('-') { out.push('-'); }
    }
    let t = out.trim_matches('-').to_string();
    if t.is_empty() { "epizoda".into() } else { t.chars().take(80).collect() }
}

/// Krátké náhodné id.
pub fn new_id() -> String { uuid::Uuid::new_v4().simple().to_string()[..12].to_string() }

fn xml(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

/// RSS feed (RSS 2.0 + iTunes/Apple Podcasts značky) ze zveřejněných epizod.
pub fn feed_xml(cfg: &Config, episodes: &[Episode]) -> String {
    let pc = &cfg.podcast;
    let base = pc.base_url.trim().trim_end_matches('/');
    let title = if pc.title.is_empty() { cfg.station.name.clone() } else { pc.title.clone() };
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n<channel>\n");
    x += &format!("  <title>{}</title>\n  <link>{}</link>\n  <atom:link href=\"{}/feed.xml\" rel=\"self\" type=\"application/rss+xml\"/>\n  <description>{}</description>\n  <language>{}</language>\n", xml(&title), xml(base), xml(base), xml(&pc.description), xml(&pc.language));
    x += &format!("  <itunes:author>{}</itunes:author>\n  <itunes:explicit>{}</itunes:explicit>\n  <itunes:category text=\"{}\"/>\n", xml(&pc.author), if pc.explicit { "true" } else { "false" }, xml(&pc.category));
    if !pc.email.is_empty() { x += &format!("  <itunes:owner><itunes:name>{}</itunes:name><itunes:email>{}</itunes:email></itunes:owner>\n", xml(&pc.author), xml(&pc.email)); }
    if !pc.image.is_empty() { x += &format!("  <itunes:image href=\"{}/{}\"/>\n", xml(base), xml(&urlencoding::encode(&pc.image))); }
    for e in episodes.iter().filter(|e| e.published) {
        let date = chrono::DateTime::parse_from_rfc3339(&e.pub_date).map(|d| d.to_rfc2822()).unwrap_or_default();
        let s = e.duration_ms / 1000;
        x += &format!("  <item>\n    <title>{}</title>\n    <description>{}</description>\n    <guid isPermaLink=\"false\">orca-{}</guid>\n    <pubDate>{}</pubDate>\n    <enclosure url=\"{}/{}\" length=\"{}\" type=\"audio/mpeg\"/>\n    <itunes:duration>{}:{:02}:{:02}</itunes:duration>\n  </item>\n",
            xml(&e.title), xml(&e.description), e.id, date, xml(base), xml(&urlencoding::encode(&e.file)), e.bytes, s / 3600, (s / 60) % 60, s % 60);
    }
    x += "</channel>\n</rss>\n";
    x
}

/// Zapíše feed.xml a nahraje epizody (dosud nenahrané) + feed + obrázek přes zvolenou službu.
pub fn publish(cfg: &Config, data_dir: &Path) -> Result<String> {
    let mut eps = list_episodes(data_dir);
    let feed = feed_xml(cfg, &eps);
    let fp = root(data_dir).join("feed.xml");
    std::fs::write(&fp, &feed)?;
    let pc = &cfg.podcast;
    if pc.upload_service.is_empty() { return Ok("feed.xml uložen (nahrávání na web není nastavené – podcast je dostupný z ORCA)".into()); }
    let mut svc = cfg.nowplaying.services.iter().find(|s| s.id == pc.upload_service).cloned().ok_or_else(|| anyhow!("zvolená služba pro nahrávání neexistuje"))?;
    if !pc.remote_dir.trim().is_empty() { svc.remote_dir = format!("{}/{}", svc.remote_dir.trim_end_matches('/'), pc.remote_dir.trim().trim_matches('/')); }
    let mut files: Vec<PathBuf> = eps.iter().filter(|e| e.published && !e.uploaded).map(|e| episodes_dir(data_dir).join(&e.file)).filter(|p| p.is_file()).collect();
    if !pc.image.is_empty() { let ip = root(data_dir).join(&pc.image); if ip.is_file() { files.push(ip); } }
    files.push(fp);
    let msg = crate::npservices::upload_files(&svc, &files)?;
    for e in eps.iter_mut().filter(|e| e.published) { e.uploaded = true; }
    save_episodes(data_dir, &eps)?;
    Ok(msg)
}

/// Záznam pořadu → úprava pro podcast (výchozí znělky), volitelně rovnou epizoda a zveřejnění.
pub fn from_show(cfg: &Config, db: &Db, data_dir: &Path, source: &Path, title: &str, auto_publish: bool) -> Result<String> {
    let ms = probe_ms(cfg, source)?;
    let mut p = Project { title: title.to_string(), source: source.to_string_lossy().to_string(), source_ms: ms,
        intro_track_id: cfg.podcast.default_intro, outro_track_id: cfg.podcast.default_outro, sweeper_track_id: cfg.podcast.default_sweeper, ..Default::default() };
    save_project(data_dir, &mut p)?;
    if !auto_publish { return Ok(format!("úprava {title} připravena")); }
    let out = render(cfg, db, data_dir, &p, None)?;
    let file = out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut eps = list_episodes(data_dir);
    eps.insert(0, Episode { id: new_id(), project_id: p.id.clone(), title: p.title.clone(), description: String::new(), file: file.clone(), bytes: std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0),
        duration_ms: probe_ms(cfg, &out).unwrap_or(0), pub_date: chrono::Local::now().to_rfc3339(), published: true, uploaded: false });
    save_episodes(data_dir, &eps)?;
    p.episode = file; save_project(data_dir, &mut p)?;
    publish(cfg, data_dir)
}
