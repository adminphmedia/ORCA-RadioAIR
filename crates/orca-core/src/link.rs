//! Propojení počítačů ORCA:
//! * **Hlavní** (vysílací) počítač – zdroj dat; zálohu i přípravný počítač obsluhuje přes „linkový“ port.
//! * **Záložní** vysílací počítač – v pohotovosti stahuje databázi, nastavení a soubory z hlavního,
//!   každou sekundu se ho ptá, jestli vysílá, a když vypadne (nebo dlouho ticho), okamžitě převezme vysílání.
//! * **Přípravný** počítač – stáhne si vysílání, připraví plány a pošle je zpět na hlavní.
//!
//! Všechno je zabezpečené společným klíčem (hlavička `x-orca-link`).
use crate::config::Config;
use crate::events::Event;
use crate::Orca;
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LinkConfig {
    /// single | main | backup | prep
    pub role: String,
    /// Popisek tohoto počítače (např. „Studio A“)
    pub name: String,
    /// Společný klíč (stejný na všech propojených počítačích)
    pub key: String,
    /// Port pro propojení (jen hlavní a záložní)
    pub bind: String,
    /// Adresa druhého počítače (např. 192.168.1.10:8770). Záloha a příprava = hlavní; hlavní = záloha.
    pub peer: String,
    /// Převzetí po N sekundách bez odezvy hlavního
    pub takeover_s: u32,
    /// Převzít i při dlouhém tichu na hlavním
    pub takeover_on_silence: bool,
    pub silence_s: u32,
    /// Převzít, když hlavní zastavil vysílání (Stop) na déle než takeover_s × 6
    pub takeover_on_stop: bool,
    /// Synchronizace dat (s)
    pub sync_interval_s: u32,
    /// Stahovat i zvukové soubory
    pub sync_files: bool,
    /// Kam ukládat soubory, které na tomto počítači nejsou (prázdné = data/Link/Soubory)
    pub mirror_dir: String,
    /// Když se hlavní vrátí a 15 s vysílá bez ticha, záloha sama přestane
    pub auto_return: bool,
}
impl Default for LinkConfig {
    fn default() -> Self {
        Self { role: "single".into(), name: String::new(), key: String::new(), bind: "0.0.0.0:8770".into(), peer: String::new(), takeover_s: 5, takeover_on_silence: true, silence_s: 30, takeover_on_stop: false, sync_interval_s: 60, sync_files: true, mirror_dir: String::new(), auto_return: true }
    }
}

/// Co o sobě počítač hlásí druhému.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Hello {
    pub station: String,
    pub name: String,
    pub role: String,
    pub version: String,
    /// Vysílá (engine hraje)
    pub on_air: bool,
    /// Záloha převzala vysílání
    pub took_over: bool,
    pub level_db: f32,
    pub silent_s: u32,
    pub now: String,
    pub rev: String,
    pub time: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LinkStatus {
    pub role: String,
    /// standby | on_air | main | prep | single
    pub state: String,
    pub peer_ok: bool,
    pub peer_last_seen: Option<String>,
    pub peer: Option<Hello>,
    pub peer_error: Option<String>,
    pub took_over_at: Option<String>,
    pub took_over_reason: Option<String>,
    pub last_sync: Option<String>,
    pub sync_message: Option<String>,
    pub syncing: bool,
    pub files_total: usize,
    pub files_done: usize,
    pub files_failed: usize,
    /// Přípravný počítač: změny od posledního stažení / odeslání
    pub local_changes: Option<bool>,
    pub base_rev: Option<String>,
    pub log: Vec<String>,
}

/// Záloha v pohotovosti – nesmí vysílat, streamovat ani spouštět události.
static STANDBY: AtomicBool = AtomicBool::new(false);
pub fn standby() -> bool { STANDBY.load(Ordering::Relaxed) }
/// Hlavní předal streamy záloze (záloha převzala vysílání) – vlastní streamy nespouští.
static YIELDED: AtomicBool = AtomicBool::new(false);
pub fn yielded() -> bool { YIELDED.load(Ordering::Relaxed) }

/// Hlavní: uvolnit stream (mountpoint) pro zálohu.
pub fn yield_streams(o: &Orca, why: &str) {
    if YIELDED.swap(true, Ordering::Relaxed) { return; }
    o.encoders.stop_all();
    o.link.status.write().state = "yielded".into();
    o.link.log(format!("streamy předány záloze: {why}"));
    o.events.alert("error", "link_yield", format!("Záložní počítač převzal vysílání – hlavní počítač uvolnil streamy (důvod zálohy: {why})."));
}
/// Hlavní: znovu spustit vlastní streamy.
pub fn resume_streams(o: &Orca, why: &str) {
    if !YIELDED.swap(false, Ordering::Relaxed) {
        // hlavní nebyl v předání (např. po restartu) – streamy, které čekají na další pokus, připojit hned
        for e in o.encoders.statuses().into_iter().filter(|e| e.state != "live" && e.state != "stopped") { o.encoders.start(&e.id); }
        return;
    }
    o.encoders.autostart();
    o.link.status.write().state = "main".into();
    o.link.log(format!("streamy znovu z hlavního počítače: {why}"));
    o.events.alert("info", "link_resume", format!("Streamy znovu vysílá hlavní počítač ({why})."));
}

fn post_peer(c: &LinkConfig, path: &str, body: serde_json::Value, timeout_ms: u64) -> Result<()> {
    let url = peer_url(c, path)?;
    agent(timeout_ms).post(&url).set("x-orca-link", &c.key).send_json(body).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}
pub fn init_standby(cfg: &Config) { STANDBY.store(cfg.link.role == "backup", Ordering::Relaxed); }

#[derive(Default)]
pub struct Link {
    pub status: RwLock<LinkStatus>,
    silent_since: Mutex<Option<Instant>>,
    sync_lock: Mutex<()>,
}

/// Tabulky „přípravy vysílání“ – posílají se z přípravného počítače na hlavní.
pub const PLAN_TABLES: &[&str] = &["tracks", "categories", "music_attrs", "music_aliases", "palettes", "pads", "plan_items", "sched_docs", "sel_docs", "sel_doc_revisions", "sel_logs", "sel_items", "campaigns", "campaign_slots", "campaign_spots", "clients", "spot_meta", "spot_presets", "break_playlist", "episodes", "memos", "proc_presets"];
/// Sloupce, které se na hlavním mění samy během vysílání (nepřepisují se ani se nepočítají do otisku).
const VOLATILE: &[&str] = &["last_played_at", "play_count", "rotation_seq", "missing", "updated_at", "file_mtime"];

pub fn now() -> String { chrono::Local::now().format("%d.%m. %H:%M:%S").to_string() }

impl Link {
    fn log(&self, s: impl Into<String>) {
        let s = format!("{} {}", now(), s.into());
        log::info!("propojení: {s}");
        let mut st = self.status.write();
        st.log.insert(0, s);
        st.log.truncate(60);
    }
}

pub fn gen_key() -> String {
    let s = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    s.as_bytes()[..24].chunks(6).map(|c| String::from_utf8_lossy(c).to_string()).collect::<Vec<_>>().join("-")
}

/// Zvýší se po každém obnovení databáze (stažení) – součást otisku dat.
static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn peer_url(c: &LinkConfig, path: &str) -> Result<String> {
    let p = c.peer.trim().trim_end_matches('/');
    if p.is_empty() { bail!("není zadaná adresa druhého počítače"); }
    let base = if p.starts_with("http://") || p.starts_with("https://") { p.to_string() } else if p.contains(':') { format!("http://{p}") } else { format!("http://{p}:8770") };
    Ok(format!("{base}/link{path}"))
}

fn agent(timeout_ms: u64) -> ureq::Agent { ureq::AgentBuilder::new().timeout_connect(Duration::from_millis(timeout_ms.min(3000))).timeout(Duration::from_millis(timeout_ms)).build() }

fn get(c: &LinkConfig, path: &str, timeout_ms: u64) -> Result<ureq::Response> {
    let url = peer_url(c, path)?;
    agent(timeout_ms).get(&url).set("x-orca-link", &c.key).call().map_err(|e| match e {
        ureq::Error::Status(401, _) => anyhow!("druhý počítač odmítl klíč – zkontrolujte, že je na obou stejný"),
        ureq::Error::Status(code, r) => anyhow!("chyba {code}: {}", r.into_string().unwrap_or_default()),
        ureq::Error::Transport(t) => anyhow!("nedostupný ({})", t.kind()),
    })
}

/// Zkouška spojení s druhým počítačem.
pub fn hello_of(c: &LinkConfig) -> Result<Hello> { Ok(get(c, "/hello", 4000)?.into_json()?) }

/// Stav tohoto počítače pro druhý.
pub fn hello(o: &Orca) -> Hello {
    let c = o.cfg.read().clone();
    let st = o.engine.snapshot();
    let (l, r) = o.engine.meters();
    let level = 20.0 * l.max(r).max(1e-9).log10();
    let silent_s = o.link.silent_since.lock().map(|t| t.elapsed().as_secs() as u32).unwrap_or(0);
    let now = st.current.as_ref().map(|x| format!("{} – {}", x.item.track.artist, x.item.track.title)).unwrap_or_default();
    Hello {
        station: c.meta.station_long.clone(), name: c.link.name.clone(), role: c.link.role.clone(), version: crate::VERSION.into(),
        on_air: st.state == crate::engine::PlayState::Playing, took_over: c.link.role == "backup" && !standby(),
        level_db: level, silent_s, now, rev: rev(o), time: chrono::Local::now().to_rfc3339(),
    }
}

/// Otisk dat (změní se při každé změně databáze nebo nastavení).
pub fn rev(o: &Orca) -> String {
    let n = o.db.with_conn(|c| c.total_changes());
    let cfg = toml::to_string(&*o.cfg.read()).unwrap_or_default();
    format!("{}-{n}-{:x}", EPOCH.load(Ordering::Relaxed), fxhash(cfg.as_bytes()))
}

fn fxhash(b: &[u8]) -> u64 { let mut h: u64 = 0xcbf29ce484222325; for x in b { h ^= *x as u64; h = h.wrapping_mul(0x100000001b3); } h }

/// Otisk tabulek přípravy (bez sloupců, které se mění při vysílání).
pub fn plan_fingerprint(conn: &rusqlite::Connection, schema: &str) -> Result<String> {
    let mut h: u64 = 0xcbf29ce484222325;
    for t in PLAN_TABLES {
        let cols = table_cols(conn, schema, t)?;
        if cols.is_empty() { continue; }
        let keep: Vec<String> = cols.into_iter().filter(|c| !VOLATILE.contains(&c.as_str())).map(|c| format!("quote(\"{c}\")")).collect();
        let sql = format!("SELECT {} FROM {schema}.\"{t}\" ORDER BY rowid", keep.join("||'|'||"));
        let mut st = conn.prepare(&sql)?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? { let s: String = r.get::<_, Option<String>>(0)?.unwrap_or_default(); for x in s.as_bytes() { h ^= *x as u64; h = h.wrapping_mul(0x100000001b3); } h ^= 0xff; h = h.wrapping_mul(0x100000001b3); }
    }
    Ok(format!("{h:016x}"))
}

fn table_cols(conn: &rusqlite::Connection, schema: &str, t: &str) -> Result<Vec<String>> {
    let mut st = conn.prepare(&format!("PRAGMA {schema}.table_info(\"{t}\")"))?;
    let v = st.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

/// Místní cesta pro soubor druhého počítače, který tady není (zrcadlo podle původní cesty).
pub fn mirror_path(mirror_dir: &Path, remote: &str) -> PathBuf {
    let rel: String = remote.replace('\\', "/").replace(':', "").trim_start_matches('/').to_string();
    let mut p = mirror_dir.to_path_buf();
    for part in rel.split('/').filter(|x| !x.is_empty() && *x != ".." && *x != ".") { p.push(part); }
    p
}
pub fn mirror_dir(cfg: &Config) -> PathBuf {
    if cfg.link.mirror_dir.trim().is_empty() { crate::countdown::data_dir().unwrap_or_default().join("Link").join("Soubory") } else { PathBuf::from(cfg.link.mirror_dir.trim()) }
}

/// Místní existující cesta k souboru (mapování složek → zrcadlo).
pub fn local_file(cfg: &Config, path: &str) -> Option<PathBuf> {
    let p = PathBuf::from(crate::general::map_path(cfg, path));
    if p.is_file() { return Some(p); }
    let m = mirror_path(&mirror_dir(cfg), path);
    m.is_file().then_some(m)
}

// ---------------------------------------------------------------------------------------------
// Stažení z hlavního (záloha i přípravný počítač)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestItem { pub id: i64, pub path: String, pub size: u64 }

/// Nastavení, která jsou vázaná na konkrétní počítač – při synchronizaci se nepřepisují.
fn merge_config(local: &Config, remote: &Config) -> Config {
    let mut n = remote.clone();
    n.link = local.link.clone();
    n.audio = local.audio.clone();
    n.fx = local.fx.clone();
    n.mic = local.mic.clone();
    n.gpio = local.gpio.clone();
    n.api = local.api.clone();
    n.ffmpeg_path = local.ffmpeg_path.clone();
    n.general.folder_mappings = local.general.folder_mappings.clone();
    n.general.start_with_os = local.general.start_with_os;
    n.general.backup = local.general.backup.clone();
    n.remote_vt.bind = local.remote_vt.bind.clone();
    n.airlog = local.airlog.clone();
    n.watch_folders = local.watch_folders.clone();
    n.station = { let mut s = remote.station.clone(); s.theme = local.station.theme.clone(); s };
    n
}

/// Stáhne databázi, nastavení a (volitelně) soubory z hlavního počítače.
pub fn pull(o: &Arc<Orca>, files: bool) -> Result<String> {
    let _g = o.link.sync_lock.try_lock().ok_or_else(|| anyhow!("synchronizace už probíhá"))?;
    o.link.status.write().syncing = true;
    let r = pull_inner(o, files);
    let mut st = o.link.status.write();
    st.syncing = false;
    match &r { Ok(m) => { st.last_sync = Some(now()); st.sync_message = Some(m.clone()); } Err(e) => st.sync_message = Some(format!("chyba: {e:#}")) }
    drop(st);
    match &r { Ok(m) => o.link.log(m.clone()), Err(e) => o.link.log(format!("stažení selhalo: {e:#}")) }
    r
}

fn pull_inner(o: &Arc<Orca>, files: bool) -> Result<String> {
    let c = o.cfg.read().clone();
    let h: Hello = get(&c.link, "/hello", 4000)?.into_json()?;
    if h.version != crate::VERSION { bail!("na hlavním počítači běží ORCA {} a tady {} – aktualizujte oba na stejnou verzi", h.version, crate::VERSION); }
    let dir = o.data_dir.join("Link");
    std::fs::create_dir_all(&dir)?;
    // databáze
    let snap = dir.join("stazeno.db");
    {
        let r = get(&c.link, "/snapshot", 600_000)?;
        let mut f = std::fs::File::create(&snap)?;
        std::io::copy(&mut r.into_reader(), &mut f)?;
    }
    if c.link.role == "prep" {
        // neodeslané změny nejdřív do zálohy
        if o.link.status.read().local_changes == Some(true) {
            if let Ok(b) = crate::general::backup_now(&o.db, &c, &o.data_dir) { o.link.log(format!("neodeslané změny uloženy do zálohy {b}")); }
        }
    }
    if c.link.role == "backup" && !standby() { bail!("záloha právě vysílá – databáze se nestahuje"); }
    o.db.with_conn_mut(|conn| conn.restore(rusqlite::MAIN_DB, &snap, None::<fn(rusqlite::backup::Progress)>)).context("obnovení databáze")?;
    EPOCH.fetch_add(1, Ordering::Relaxed);
    let _ = std::fs::remove_file(&snap);
    // nastavení
    let remote: Config = get(&c.link, "/config", 15000)?.into_json()?;
    let merged = merge_config(&c, &remote);
    o.apply_config(merged)?;
    // otisk přípravy = základ pro pozdější odeslání
    let fp = o.db.with_conn(|conn| plan_fingerprint(conn, "main"))?;
    { let mut st = o.link.status.write(); st.base_rev = Some(fp.clone()); st.local_changes = Some(false); }
    std::fs::write(dir.join("zaklad.txt"), &fp).ok();
    o.events.emit(Event::LibraryChanged { added: 0, updated: 0, missing: 0 });
    o.events.emit(Event::QueueChanged);
    let mut msg = format!("staženo z {} ({}): databáze a nastavení", if h.name.is_empty() { "hlavního" } else { &h.name }, h.station);
    if files && c.link.sync_files { msg.push_str(&format!(", {}", pull_files(o, &c)?)); }
    Ok(msg)
}

fn pull_files(o: &Orca, c: &Config) -> Result<String> {
    let list: Vec<ManifestItem> = get(&c.link, "/manifest", 60000)?.into_json()?;
    let mdir = mirror_dir(c);
    let todo: Vec<&ManifestItem> = list.iter().filter(|m| local_file(c, &m.path).map(|p| std::fs::metadata(p).map(|x| x.len() != m.size).unwrap_or(true)).unwrap_or(true)).collect();
    { let mut st = o.link.status.write(); st.files_total = todo.len(); st.files_done = 0; st.files_failed = 0; }
    let (mut ok, mut bad, mut bytes) = (0usize, 0usize, 0u64);
    for m in &todo {
        let dst = match local_file(c, &m.path) { Some(p) => p, None => mirror_path(&mdir, &m.path) };
        let r = (|| -> Result<u64> {
            if let Some(par) = dst.parent() { std::fs::create_dir_all(par)?; }
            let tmp = dst.with_extension("orca-part");
            let resp = get(&c.link, &format!("/file?id={}", m.id), 900_000)?;
            let mut f = std::fs::File::create(&tmp)?;
            let n = std::io::copy(&mut resp.into_reader().take(m.size + 1), &mut f)?;
            drop(f);
            std::fs::rename(&tmp, &dst)?;
            Ok(n)
        })();
        match r { Ok(n) => { ok += 1; bytes += n; } Err(e) => { bad += 1; log::warn!("propojení: soubor {}: {e:#}", m.path); } }
        let mut st = o.link.status.write(); st.files_done = ok; st.files_failed = bad;
    }
    Ok(if todo.is_empty() { format!("soubory aktuální ({})", list.len()) } else { format!("soubory {ok}/{} ({:.0} MB){}", todo.len(), bytes as f64 / 1e6, if bad > 0 { format!(", {bad} selhalo") } else { String::new() }) })
}

// ---------------------------------------------------------------------------------------------
// Odeslání z přípravného počítače na hlavní

pub fn push(o: &Arc<Orca>, force: bool) -> Result<String> {
    let _g = o.link.sync_lock.try_lock().ok_or_else(|| anyhow!("synchronizace už probíhá"))?;
    o.link.status.write().syncing = true;
    let r = push_inner(o, force);
    let mut st = o.link.status.write();
    st.syncing = false;
    match &r { Ok(m) => { st.last_sync = Some(now()); st.sync_message = Some(m.clone()); } Err(e) => st.sync_message = Some(format!("chyba: {e:#}")) }
    drop(st);
    match &r { Ok(m) => o.link.log(m.clone()), Err(e) => o.link.log(format!("odeslání selhalo: {e:#}")) }
    r
}

fn push_inner(o: &Arc<Orca>, force: bool) -> Result<String> {
    let c = o.cfg.read().clone();
    if c.link.role != "prep" { bail!("odesílat do vysílání lze jen z přípravného počítače"); }
    let h: Hello = get(&c.link, "/hello", 4000)?.into_json()?;
    if h.version != crate::VERSION { bail!("na vysílacím počítači běží ORCA {} a tady {} – aktualizujte oba na stejnou verzi", h.version, crate::VERSION); }
    let base = o.link.status.read().base_rev.clone().or_else(|| std::fs::read_to_string(o.data_dir.join("Link/zaklad.txt")).ok()).ok_or_else(|| anyhow!("nejdřív si vysílání stáhněte (Stáhnout z vysílání)"))?;
    let remote_fp: String = get(&c.link, "/fingerprint", 60000)?.into_string()?;
    if remote_fp.trim() != base.trim() && !force {
        bail!("na vysílacím počítači se plán nebo knihovna od vašeho stažení změnily. Stáhněte si vysílání znovu (vaše neodeslané změny se uloží do zálohy), nebo zvolte Odeslat i tak – přepíše změny na vysílacím počítači.");
    }
    // 1) soubory, které hlavní nemá
    let items: Vec<ManifestItem> = o.db.with_conn(|conn| -> Result<Vec<ManifestItem>> {
        let mut st = conn.prepare("SELECT id, path FROM tracks")?;
        let v = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v.into_iter().filter_map(|(id, path)| local_file(&c, &path).map(|p| ManifestItem { id, size: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0), path })).collect())
    })?;
    let url = peer_url(&c.link, "/missing")?;
    let missing: Vec<i64> = agent(60000).post(&url).set("x-orca-link", &c.link.key).send_json(&items)?.into_json()?;
    { let mut st = o.link.status.write(); st.files_total = missing.len(); st.files_done = 0; st.files_failed = 0; }
    let (mut ok, mut bad) = (0, 0);
    for id in &missing {
        let Some(it) = items.iter().find(|x| x.id == *id) else { continue };
        let Some(p) = local_file(&c, &it.path) else { bad += 1; continue };
        let r = (|| -> Result<()> {
            let f = std::fs::File::open(&p)?;
            let url = format!("{}?path={}", peer_url(&c.link, "/upload")?, urlencode(&it.path));
            agent(900_000).post(&url).set("x-orca-link", &c.link.key).set("Content-Length", &it.size.to_string()).send(f)?;
            Ok(())
        })();
        if r.is_ok() { ok += 1 } else { bad += 1 }
        let mut st = o.link.status.write(); st.files_done = ok; st.files_failed = bad;
    }
    if bad > 0 { bail!("{bad} souborů se nepodařilo odeslat – plán neodeslán (zkuste znovu)"); }
    // 2) tabulky přípravy
    let dir = o.data_dir.join("Link");
    std::fs::create_dir_all(&dir)?;
    let out = dir.join("odeslat.db");
    let _ = std::fs::remove_file(&out);
    o.db.with_conn(|conn| conn.execute("VACUUM INTO ?1", [out.to_string_lossy().to_string()]))?;
    let url = format!("{}?base={}&force={}", peer_url(&c.link, "/push")?, urlencode(&base), force);
    let f = std::fs::File::open(&out)?;
    let len = std::fs::metadata(&out)?.len();
    let r: serde_json::Value = agent(600_000).post(&url).set("x-orca-link", &c.link.key).set("Content-Length", &len.to_string()).send(f).map_err(|e| match e {
        ureq::Error::Status(code, r) => anyhow!("vysílací počítač odmítl ({code}): {}", r.into_string().unwrap_or_default()),
        e => anyhow!("{e}"),
    })?.into_json()?;
    let _ = std::fs::remove_file(&out);
    let fp = r["fingerprint"].as_str().unwrap_or_default().to_string();
    { let mut st = o.link.status.write(); st.base_rev = Some(fp.clone()); st.local_changes = Some(false); }
    std::fs::write(dir.join("zaklad.txt"), &fp).ok();
    Ok(format!("odesláno do vysílání: {}{}", r["message"].as_str().unwrap_or(""), if ok > 0 { format!(", {ok} nových souborů") } else { String::new() }))
}

fn urlencode(s: &str) -> String { s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect() }

/// Hlavní: přijme tabulky přípravy (soubor SQLite) a nahradí jimi svoje – v jedné transakci.
pub fn apply_push(o: &Orca, file: &Path, base: &str, force: bool) -> Result<String> {
    let fp = o.db.with_conn(|c| plan_fingerprint(c, "main"))?;
    if fp != base && !force { bail!("plán se na vysílacím počítači mezitím změnil – stáhněte si vysílání znovu"); }
    // pojistka: záloha před přepsáním
    let c = o.cfg.read().clone();
    if let Err(e) = crate::general::backup_now(&o.db, &c, &o.data_dir) { log::warn!("záloha před přijetím přípravy: {e}"); }
    let msg = o.db.with_conn(|conn| -> Result<String> {
        conn.execute("ATTACH DATABASE ?1 AS src", [file.to_string_lossy().to_string()])?;
        let r = (|| -> Result<String> {
            conn.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;")?;
            let mut parts = vec![];
            let res = (|| -> Result<()> {
                for t in PLAN_TABLES {
                    let a = table_cols(conn, "main", t)?; let b = table_cols(conn, "src", t)?;
                    if a.is_empty() || b.is_empty() { continue; }
                    if a != b { bail!("tabulka {t} má jinou strukturu – aktualizujte oba počítače na stejnou verzi"); }
                    let before: i64 = conn.query_row(&format!("SELECT count(*) FROM main.\"{t}\""), [], |r| r.get(0))?;
                    if *t == "tracks" {
                        let cols = a.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
                        let set = a.iter().filter(|c| *c != "id" && !VOLATILE.contains(&c.as_str())).map(|c| format!("\"{c}\"=excluded.\"{c}\"")).collect::<Vec<_>>().join(",");
                        conn.execute_batch(&format!("DELETE FROM main.tracks WHERE id NOT IN (SELECT id FROM src.tracks); INSERT INTO main.tracks({cols}) SELECT {cols} FROM src.tracks WHERE true ON CONFLICT(id) DO UPDATE SET {set};"))?;
                    } else {
                        conn.execute_batch(&format!("DELETE FROM main.\"{t}\"; INSERT INTO main.\"{t}\" SELECT * FROM src.\"{t}\";"))?;
                    }
                    let after: i64 = conn.query_row(&format!("SELECT count(*) FROM main.\"{t}\""), [], |r| r.get(0))?;
                    if before != after { parts.push(format!("{t} {before}→{after}")); }
                }
                Ok(())
            })();
            match res { Ok(()) => conn.execute_batch("COMMIT; PRAGMA foreign_keys=ON;")?, Err(e) => { let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys=ON;"); return Err(e); } }
            Ok(if parts.is_empty() { "plány aktualizovány".into() } else { format!("plány aktualizovány ({})", parts.join(", ")) })
        })();
        let _ = conn.execute("DETACH DATABASE src", []);
        r
    })?;
    o.events.emit(Event::LibraryChanged { added: 0, updated: 0, missing: 0 });
    o.events.alert("info", "link_push", format!("Příprava vysílání přijata z přípravného počítače: {msg}"));
    Ok(msg)
}

// ---------------------------------------------------------------------------------------------
// Hlídání (běží v samostatném vlákně)

pub fn start(o: Arc<Orca>) {
    std::thread::Builder::new().name("orca-link".into()).spawn(move || watch(o)).ok();
}

fn watch(o: Arc<Orca>) {
    let mut fails: u32 = 0;
    let mut bad_since: Option<Instant> = None;
    let mut stop_since: Option<Instant> = None;
    let mut main_good_since: Option<Instant> = None;
    let mut last_sync: Option<Instant> = None;
    let last_rev = Arc::new(Mutex::new(String::new()));
    let mut peer_was_ok = true;
    let mut peer_down_alerted = false;
    let mut tick: u64 = 0;
    // záloha převezme jen po tom, co hlavní aspoň jednou viděla (jinak by při současném startu převzala hned)
    let mut armed = false;
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        tick += 1;
        // ticho na tomto počítači
        {
            let (l, r) = o.engine.meters();
            let db = 20.0 * l.max(r).max(1e-9).log10();
            let playing = o.engine.play_state() == crate::engine::PlayState::Playing;
            let mut s = o.link.silent_since.lock();
            if playing && db < -50.0 { s.get_or_insert_with(Instant::now); } else { *s = None; }
        }
        if tick % 4 != 0 { continue; } // 1× za sekundu
        let c = o.cfg.read().clone();
        let role = c.link.role.clone();
        if role != "backup" && STANDBY.load(Ordering::Relaxed) { STANDBY.store(false, Ordering::Relaxed); }
        if role != "main" && yielded() { YIELDED.store(false, Ordering::Relaxed); }
        {
            let mut st = o.link.status.write();
            st.role = role.clone();
            st.state = match role.as_str() { "backup" => if standby() { "standby" } else { "on_air" }, "main" if yielded() => "yielded", r => r }.to_string();
        }
        if role == "single" || c.link.peer.trim().is_empty() { continue; }
        if role == "prep" {
            // jen sledování, zda je vysílací počítač dosažitelný, a zda tu jsou neodeslané změny (každých 10 s)
            if tick % 40 == 0 {
                let r = get(&c.link, "/hello", 3000).and_then(|r| Ok(r.into_json::<Hello>()?));
                let mut st = o.link.status.write();
                match r { Ok(h) => { st.peer_ok = true; st.peer_last_seen = Some(now()); st.peer = Some(h); st.peer_error = None; } Err(e) => { st.peer_ok = false; st.peer_error = Some(e.to_string()); } }
                if let Some(b) = st.base_rev.clone() { drop(st); let fp = o.db.with_conn(|cn| plan_fingerprint(cn, "main")).unwrap_or_default(); o.link.status.write().local_changes = Some(fp != b); }
            }
            continue;
        }
        let timeout = 1500;
        let r = get(&c.link, "/hello", timeout).and_then(|r| Ok(r.into_json::<Hello>()?));
        match &r {
            Ok(h) => {
                fails = 0;
                let mut st = o.link.status.write();
                st.peer_ok = true; st.peer_last_seen = Some(now()); st.peer = Some(h.clone()); st.peer_error = None;
            }
            Err(e) => { fails += 1; let mut st = o.link.status.write(); st.peer_ok = false; st.peer_error = Some(e.to_string()); }
        }
        let ok = r.is_ok();
        if role == "main" {
            // streamy předané záloze: vzít zpět, když záloha už nevysílá (vrátila vysílání nebo vypadla)
            if yielded() {
                match &r {
                    Ok(h) if !h.took_over => { let t = *main_good_since.get_or_insert_with(Instant::now); if t.elapsed() >= Duration::from_secs(5) { resume_streams(&o, "záloha už nevysílá"); main_good_since = None; } }
                    Err(_) if fails >= 10 => { resume_streams(&o, "záloha neodpovídá – streamy přebírá zpět hlavní"); main_good_since = None; }
                    _ => main_good_since = None,
                }
            }
            // hlavní hlídá zálohu: když je 30 s nedostupná, upozorní (jednou)
            if ok {
                if !peer_was_ok && peer_down_alerted { o.events.alert("info", "link_backup_back", format!("Záložní počítač je zase dostupný ({}).", c.link.peer)); }
                peer_down_alerted = false; peer_was_ok = true;
                if let Ok(h) = &r { if h.took_over && tick % 120 == 0 && o.engine.play_state() == crate::engine::PlayState::Playing {
                    o.events.alert("error", "link_both_on_air", "Hlavní i záložní počítač vysílají současně – na záložním klikněte Vrátit vysílání hlavnímu (nebo zapněte automatický návrat).");
                } }
            } else {
                peer_was_ok = false;
                if fails >= 30 && !peer_down_alerted { peer_down_alerted = true; o.events.alert("error", "link_backup_down", format!("Záložní počítač ({}) neodpovídá déle než 30 s – při výpadku hlavního by vysílání nepřevzal. Chyba: {}", c.link.peer, r.as_ref().err().map(|e| e.to_string()).unwrap_or_default())); }
            }
            continue;
        }
        // ---- záloha
        if standby() {
            let mut reason: Option<String> = None;
            if ok { armed = true; }
            match &r {
                Ok(h) => {
                    if c.link.takeover_on_silence && h.on_air && h.silent_s >= c.link.silence_s.max(5) {
                        let t = *bad_since.get_or_insert_with(Instant::now);
                        if t.elapsed() >= Duration::from_secs(1) { reason = Some(format!("na hlavním počítači je ticho {} s", h.silent_s)); }
                    } else { bad_since = None; }
                    if c.link.takeover_on_stop && !h.on_air {
                        let t = *stop_since.get_or_insert_with(Instant::now);
                        if t.elapsed() >= Duration::from_secs(c.link.takeover_s.max(1) as u64 * 6) { reason = Some("hlavní počítač zastavil vysílání".into()); }
                    } else { stop_since = None; }
                    if peer_down_alerted { peer_down_alerted = false; }
                }
                Err(e) => {
                    let limit = if armed { c.link.takeover_s.max(2) } else { 120 };
                    if fails >= limit && (armed || started.elapsed() >= Duration::from_secs(120)) {
                        reason = Some(if armed { format!("hlavní počítač neodpovídá {fails} s ({e})") } else { format!("hlavní počítač je nedostupný už 2 minuty od spuštění zálohy ({e})") });
                    }
                }
            }
            if let Some(why) = reason { takeover(&o, &why); fails = 0; bad_since = None; stop_since = None; main_good_since = None; continue; }
            // synchronizace v pohotovosti
            if let Ok(h) = &r {
                let due = last_sync.map(|t| t.elapsed() >= Duration::from_secs(c.link.sync_interval_s.max(10) as u64)).unwrap_or(true);
                if due && h.rev != *last_rev.lock() && !o.link.status.read().syncing {
                    last_sync = Some(Instant::now());
                    let (o2, rev, lr) = (o.clone(), h.rev.clone(), last_rev.clone());
                    // stahuje se v jiném vlákně, aby hlídání běželo dál (každou sekundu)
                    std::thread::Builder::new().name("orca-link-sync".into()).spawn(move || { if pull(&o2, true).is_ok() { *lr.lock() = rev; } }).ok();
                } else if due { last_sync = Some(Instant::now()); }
            }
        } else {
            // záloha vysílá – čeká na návrat hlavního
            match &r {
                Ok(h) if h.on_air && h.silent_s < 5 && !h.took_over => {
                    let t = *main_good_since.get_or_insert_with(Instant::now);
                    if c.link.auto_return && t.elapsed() >= Duration::from_secs(15) { handback(&o, "hlavní počítač zase vysílá"); main_good_since = None; last_rev.lock().clear(); }
                    else if !c.link.auto_return && tick % 120 == 0 { o.events.alert("warning", "link_main_back", "Hlavní počítač je zpět a vysílá – vysílají oba. Na záložním klikněte Vrátit vysílání hlavnímu."); }
                }
                _ => main_good_since = None,
            }
        }
    }
}

/// Záloha převezme vysílání.
pub fn takeover(o: &Arc<Orca>, why: &str) {
    if !standby() { return; }
    STANDBY.store(false, Ordering::Relaxed);
    { let mut st = o.link.status.write(); st.took_over_at = Some(now()); st.took_over_reason = Some(why.to_string()); st.state = "on_air".into(); }
    // když hlavní ještě žije (ticho, zaseknutý zvuk), ať uvolní stream – jinak by Icecast zálohu nepustil na stejný mountpoint
    let lc = o.cfg.read().link.clone();
    match post_peer(&lc, "/yield", serde_json::json!({ "why": why }), 1500) {
        Ok(()) => o.link.log("hlavní počítač uvolnil streamy"),
        Err(_) => o.link.log("hlavní počítač nedostupný – streamy se připojí, jakmile Icecast uvolní mountpoint"),
    }
    o.encoders.autostart();
    o.automation.set_mode(crate::automation::Mode::Auto);
    o.engine.send(crate::engine::Cmd::Play);
    let c = o.cfg.read().clone();
    o.link.log(format!("PŘEVZETÍ VYSÍLÁNÍ: {why}"));
    o.events.alert("error", "link_takeover", format!("ZÁLOŽNÍ POČÍTAČ{} PŘEVZAL VYSÍLÁNÍ – důvod: {why}. Zkontrolujte hlavní počítač ({}).", if c.link.name.is_empty() { String::new() } else { format!(" „{}“", c.link.name) }, c.link.peer));
}

/// Záloha vrátí vysílání hlavnímu a přejde do pohotovosti.
pub fn handback(o: &Arc<Orca>, why: &str) {
    if standby() { return; }
    STANDBY.store(true, Ordering::Relaxed);
    o.engine.send(crate::engine::Cmd::Stop);
    o.encoders.stop_all();
    // hlavní hned znovu spustí streamy (nečeká na své hlídání)
    let lc = o.cfg.read().link.clone();
    for _ in 0..3 { if post_peer(&lc, "/resume", serde_json::json!({}), 2000).is_ok() { break; } std::thread::sleep(Duration::from_millis(500)); }
    { let mut st = o.link.status.write(); st.state = "standby".into(); }
    o.link.log(format!("vysílání vráceno hlavnímu: {why}"));
    o.events.alert("info", "link_handback", format!("Záložní počítač vrátil vysílání hlavnímu ({why}) a je znovu v pohotovosti."));
}

// ---------------------------------------------------------------------------------------------
// Telegram

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token: String,
    pub chat_id: String,
    /// error | warning
    pub min_level: String,
}

pub fn telegram_send(t: &TelegramConfig, text: &str) -> Result<()> {
    if t.bot_token.trim().is_empty() || t.chat_id.trim().is_empty() { bail!("chybí token bota nebo ID chatu"); }
    let url = format!("https://api.telegram.org/bot{}/sendMessage", t.bot_token.trim());
    match ureq::post(&url).timeout(Duration::from_secs(15)).send_json(serde_json::json!({ "chat_id": t.chat_id.trim(), "text": text, "disable_web_page_preview": true })) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => Err(anyhow!("Telegram {code}: {}", r.into_json::<serde_json::Value>().ok().and_then(|v| v["description"].as_str().map(String::from)).unwrap_or_default())),
        Err(e) => Err(anyhow!("Telegram: {e}")),
    }
}

/// Najde ID chatu: vrátí chaty, které botovi naposledy napsaly.
pub fn telegram_chats(token: &str) -> Result<Vec<(String, String)>> {
    let url = format!("https://api.telegram.org/bot{}/getUpdates", token.trim());
    let v: serde_json::Value = ureq::get(&url).timeout(Duration::from_secs(15)).call().map_err(|e| anyhow!("Telegram: {e}"))?.into_json()?;
    let mut out: Vec<(String, String)> = vec![];
    for u in v["result"].as_array().cloned().unwrap_or_default() {
        let ch = if u["message"].is_object() { &u["message"]["chat"] } else if u["channel_post"].is_object() { &u["channel_post"]["chat"] } else if u["my_chat_member"].is_object() { &u["my_chat_member"]["chat"] } else { continue };
        let id = ch["id"].as_i64().map(|x| x.to_string()).unwrap_or_default();
        let name = ch["title"].as_str().map(String::from).unwrap_or_else(|| format!("{} {}", ch["first_name"].as_str().unwrap_or(""), ch["last_name"].as_str().unwrap_or("")).trim().to_string());
        if !id.is_empty() && !out.iter().any(|x| x.0 == id) { out.push((id, name)); }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mirror() {
        assert_eq!(mirror_path(Path::new("/m"), "C:\\Hudba\\a.mp3"), PathBuf::from("/m/C/Hudba/a.mp3"));
        assert_eq!(mirror_path(Path::new("/m"), "/Users/x/../a.mp3"), PathBuf::from("/m/Users/x/a.mp3"));
    }
}
