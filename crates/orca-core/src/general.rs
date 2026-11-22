//! Obecná nastavení (Jazler General Settings): hlídání ticha, vlastní logy, e-mailová upozornění,
//! zálohy a údržba databáze, kontrola integrity souborů, mapování složek, priorita vysílání.

use crate::config::{Config, CustomLog, EmailConfig};
use crate::db::{now_utc, Db};
use crate::engine::{Engine, PlayState};
use crate::events::{Event, EventBus};
use crate::model::{Track, TrackKind};
use anyhow::{anyhow, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::params;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Mapování složek (stejná knihovna na Windows i macOS, přesunutý disk / NAS)

/// Přeloží cestu podle mapování složek (první shoda prefixu).
pub fn map_path(cfg: &Config, path: &str) -> String {
    for m in &cfg.general.folder_mappings {
        if m.from.is_empty() { continue; }
        let (f, p) = (m.from.replace('\\', "/").to_lowercase(), path.replace('\\', "/"));
        if p.to_lowercase().starts_with(&f) {
            let rest = &p[f.len()..];
            let mut out = format!("{}{}", m.to.trim_end_matches(['/', '\\']), if rest.starts_with('/') { rest.to_string() } else { format!("/{rest}") });
            if cfg!(windows) { out = out.replace('/', "\\"); }
            return out;
        }
    }
    path.to_string()
}

/// Hromadná změna cest v databázi (např. po přesunu knihovny na jiný disk).
pub fn rewrite_paths(db: &Db, from: &str, to: &str) -> Result<usize> {
    if from.is_empty() { return Err(anyhow!("zadejte původní cestu")); }
    db.with_conn(|c| Ok(c.execute("UPDATE tracks SET path = ?2 || substr(path, length(?1) + 1), missing = 0 WHERE path LIKE ?1 || '%'", params![from, to])?))
}

// ---------------------------------------------------------------------------------------------
// Údržba databáze

#[derive(Debug, Clone, Serialize, Default)]
pub struct IntegrityReport { pub checked: usize, pub missing: Vec<(i64, String, String)>, pub restored: usize, pub report_path: Option<String> }

/// Kontrola dostupnosti všech souborů; action: report | disable | delete
pub fn integrity_check(db: &Db, cfg: &Config, data_dir: &Path, action: &str) -> Result<IntegrityReport> {
    let rows: Vec<(i64, String, String, bool)> = db.with_conn(|c| -> Result<_> {
        let mut st = c.prepare("SELECT id, path, artist || ' - ' || title, missing FROM tracks")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut rep = IntegrityReport { checked: rows.len(), ..Default::default() };
    for (id, path, name, was_missing) in rows {
        let ok = Path::new(&map_path(cfg, &path)).is_file();
        if !ok {
            rep.missing.push((id, path.clone(), name));
            match action {
                "disable" => { db.with_conn(|c| c.execute("UPDATE tracks SET missing=1, enabled=0 WHERE id=?1", [id]))?; }
                "delete" => { db.delete_track(id)?; }
                _ => { db.set_missing(id, true)?; }
            }
        } else if was_missing { db.set_missing(id, false)?; rep.restored += 1; }
    }
    let dir = data_dir.join("Reports");
    std::fs::create_dir_all(&dir)?;
    let p = dir.join(format!("integrita_{}.txt", chrono::Local::now().format("%Y%m%d_%H%M")));
    let mut f = std::fs::File::create(&p)?;
    writeln!(f, "ORCA RadioAIR – kontrola integrity {}\r\nZkontrolováno: {}, chybí: {}\r\n", chrono::Local::now().format("%d.%m.%Y %H:%M"), rep.checked, rep.missing.len())?;
    for (id, path, name) in &rep.missing { writeln!(f, "#{id}\t{name}\t{path}\r")?; }
    rep.report_path = Some(p.to_string_lossy().to_string());
    Ok(rep)
}

/// Údržba: smazání starých záznamů vysílání, VACUUM (zmenšení databáze).
pub fn maintenance(db: &Db, older_than_days: u32, vacuum: bool) -> Result<serde_json::Value> {
    let deleted = if older_than_days > 0 {
        db.with_conn(|c| c.execute("DELETE FROM play_log WHERE started_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)", [format!("-{older_than_days} days")]))?
    } else { 0 };
    if vacuum { db.with_conn(|c| c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;"))?; }
    Ok(serde_json::json!({ "deleted_log": deleted, "vacuum": vacuum }))
}

/// Reset vybraných knihoven (odebere záznamy z databáze, soubory na disku zůstanou).
pub fn reset_libraries(db: &Db, kinds: &[TrackKind]) -> Result<usize> {
    let mut n = 0;
    for k in kinds { n += db.with_conn(|c| c.execute("DELETE FROM tracks WHERE kind=?1", [k.as_str()]))?; }
    Ok(n)
}

pub fn backup_dir(cfg: &Config, data_dir: &Path) -> PathBuf {
    if cfg.general.backup.dir.as_os_str().is_empty() { data_dir.join("Backups") } else { cfg.general.backup.dir.clone() }
}

/// Záloha databáze (konzistentní kopie VACUUM INTO) + konfigurace; ponechá posledních N záloh.
pub fn backup_now(db: &Db, cfg: &Config, data_dir: &Path) -> Result<String> {
    let dir = backup_dir(cfg, data_dir);
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let mut p = dir.join(format!("orca_{stamp}.db"));
    let mut n = 1;
    while p.exists() { p = dir.join(format!("orca_{stamp}_{n}.db")); n += 1; }
    db.with_conn(|c| c.execute("VACUUM INTO ?1", [p.to_string_lossy().to_string()]))?;
    let _ = std::fs::copy(data_dir.join("orca.toml"), dir.join(format!("orca_{stamp}.toml")));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("orca_") && n.ends_with(".db")).unwrap_or(false)).collect();
    files.sort();
    let keep = cfg.general.backup.keep.max(1) as usize;
    if files.len() > keep {
        for f in &files[..files.len() - keep] { let _ = std::fs::remove_file(f); let _ = std::fs::remove_file(f.with_extension("toml")); }
    }
    Ok(p.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------------------------
// Vlastní logy (rekonciliace s traffic softwarem, autorské svazy)

fn fmt_dur(ms: i64) -> String { let s = ms.max(0) / 1000; format!("{:02}:{:02}", s / 60, s % 60) }

pub fn custom_log_line(l: &CustomLog, t: &Track, source: &str, code: &str, now: chrono::DateTime<chrono::Local>) -> String {
    l.line.replace("{DATE}", &now.format("%Y-%m-%d").to_string()).replace("{TIME}", &now.format("%H:%M:%S").to_string())
        .replace("{ARTIST}", &t.artist).replace("{TITLE}", &t.title).replace("{ALBUM}", &t.album)
        .replace("{DURATION_S}", &(t.duration_ms / 1000).to_string()).replace("{DURATION}", &fmt_dur(t.duration_ms))
        .replace("{KIND}", t.kind().as_str()).replace("{CATEGORY}", &t.category).replace("{CODE}", code).replace("{ISRC}", &t.isrc)
        .replace("{COMPOSER}", &t.composer).replace("{PUBLISHER}", &t.publisher).replace("{FILENAME}", &t.filename).replace("{SOURCE}", source)
}

fn write_custom_logs(cfg: &Config, data_dir: &Path, t: &Track, source: &str) {
    let now = crate::automation::now_local(cfg);
    for l in cfg.general.custom_logs.iter().filter(|l| l.enabled && l.kinds.contains(&t.kind())) {
        let dir = if l.dir.as_os_str().is_empty() { data_dir.join("Logs") } else { l.dir.clone() };
        if std::fs::create_dir_all(&dir).is_err() { continue; }
        let name = l.filename.replace("{DATE}", &now.format("%Y-%m-%d").to_string()).replace("{YYYY}", &now.format("%Y").to_string()).replace("{MM}", &now.format("%m").to_string()).replace("{DD}", &now.format("%d").to_string());
        let p = dir.join(name);
        let new = !p.exists();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            if new && !l.header.is_empty() { let _ = write!(f, "{}\r\n", l.header); }
            let code = crate::library::playlister_code(cfg, t);
            let _ = write!(f, "{}\r\n", custom_log_line(l, t, source, &code, now));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// E-mailová upozornění

pub fn send_email(e: &EmailConfig, subject: &str, body: &str) -> Result<()> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};
    if e.smtp_host.is_empty() || e.to.is_empty() { return Err(anyhow!("chybí SMTP server nebo příjemce")); }
    let from = if e.from.is_empty() { e.username.clone() } else { e.from.clone() };
    let mut b = Message::builder().from(from.parse().map_err(|x| anyhow!("odesílatel: {x}"))?).subject(subject);
    for to in e.to.iter().filter(|x| !x.trim().is_empty()) { b = b.to(to.trim().parse().map_err(|x| anyhow!("příjemce {to}: {x}"))?); }
    let msg = b.header(ContentType::TEXT_PLAIN).body(body.to_string())?;
    let builder = match e.security.as_str() {
        "tls" => SmtpTransport::relay(&e.smtp_host)?,
        "none" => SmtpTransport::builder_dangerous(&e.smtp_host),
        _ => SmtpTransport::starttls_relay(&e.smtp_host)?,
    };
    let mut builder = builder.port(e.port).timeout(Some(Duration::from_secs(15)));
    if !e.username.is_empty() { builder = builder.credentials(Credentials::new(e.username.clone(), e.password.clone())); }
    builder.build().send(&msg)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Služby na pozadí

pub struct General {
    pub silence_since: Mutex<Option<Instant>>,
    pub last_backup: RwLock<Option<String>>,
    pub email_status: RwLock<Option<String>>,
}

impl General {
    pub fn start(db: Arc<Db>, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, events: EventBus, data_dir: PathBuf, automation: Arc<crate::automation::Automation>) -> Arc<Self> {
        let g = Arc::new(Self { silence_since: Mutex::new(None), last_backup: RwLock::new(None), email_status: RwLock::new(None) });
        // události: vlastní logy, e-maily
        {
            let (cfg, data_dir, g2) = (cfg.clone(), data_dir.clone(), g.clone());
            let rx = events.subscribe();
            std::thread::Builder::new().name("orca-general-events".into()).spawn(move || {
                let mut sent: HashMap<String, Instant> = HashMap::new();
                while let Ok(ev) = rx.recv() {
                    match ev {
                        Event::TrackStarted { item, channel } if channel == "main" => { let c = cfg.read().clone(); write_custom_logs(&c, &data_dir, &item.track, &item.source); }
                        Event::Alert { level, code, message } => {
                            let c = cfg.read().clone();
                            let e = &c.general.email;
                            let lvl_ok = level == "error" || (e.min_level == "warning" && level == "warning");
                            if !e.enabled || !lvl_ok { continue; }
                            let key = format!("{code}:{message}");
                            if sent.get(&key).map(|t| t.elapsed() < Duration::from_secs(e.repeat_min.max(1) as u64 * 60)).unwrap_or(false) { continue; }
                            sent.insert(key, Instant::now());
                            let (e, st, g3) = (e.clone(), c.meta.station_long.clone(), g2.clone());
                            let subject = format!("[{st}] {} – {code}", if level == "error" { "CHYBA" } else { "Upozornění" });
                            let body = format!("{message}\n\nČas: {}\nStanice: {st}\nORCA RadioAIR", chrono::Local::now().format("%d.%m.%Y %H:%M:%S"));
                            std::thread::spawn(move || { let r = send_email(&e, &subject, &body); *g3.email_status.write() = Some(match r { Ok(_) => format!("odesláno {}", chrono::Local::now().format("%H:%M:%S")), Err(x) => format!("chyba: {x}") }); });
                        }
                        _ => {}
                    }
                }
            }).ok();
        }
        // hlídání ticha (dead air) + zálohy + údržba logu
        {
            let (db, cfg, engine, events, data_dir, g2) = (db.clone(), cfg.clone(), engine.clone(), events.clone(), data_dir.clone(), g.clone());
            std::thread::Builder::new().name("orca-watchdog".into()).spawn(move || {
                let mut last_daily = String::new();
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let c = cfg.read().clone();
                    // ticho
                    let s = &c.general.silence;
                    let st = engine.snapshot();
                    let (l, r) = engine.meters();
                    let db_lvl = 20.0 * l.max(r).max(1e-9).log10();
                    let playing = st.state == PlayState::Playing;
                    let relay = st.current.as_ref().map(|x| x.item.relay.is_some()).unwrap_or(false);
                    if s.enabled && playing && !relay && db_lvl < s.threshold_db {
                        let mut since = g2.silence_since.lock();
                        let t0 = *since.get_or_insert_with(Instant::now);
                        if t0.elapsed() > Duration::from_secs(s.seconds.max(3) as u64) {
                            *since = Some(Instant::now());
                            drop(since);
                            events.alert("error", "dead_air", format!("Ticho ve vysílání déle než {} s (úroveň {:.0} dBFS) – akce: {}", s.seconds, db_lvl, s.action));
                            match s.action.as_str() {
                                "next" => engine.send(crate::engine::Cmd::Next),
                                "playlist" => if let Some(pl) = s.playlist_id.and_then(|id| crate::sched::get::<crate::sched::Playlist>(&db, id).ok().flatten()) {
                                    if let Ok((items, _, _)) = crate::sched::expand(&db, &c, &pl.items, "playlist", "záchranný playlist (ticho)", 0) {
                                        for (i, it) in items.into_iter().enumerate() { engine.queue_push(it, Some(i)); }
                                        engine.send(crate::engine::Cmd::Next);
                                    }
                                },
                                "relay" => if let Some(q) = s.relay_id.as_ref().and_then(|id| crate::relay::queue_item(&c, id, 0, "záložní zdroj (ticho)")) { engine.queue_push(q, Some(0)); engine.send(crate::engine::Cmd::Next); },
                                _ => {}
                            }
                            if automation.mode() != crate::automation::Mode::Auto && s.action != "alert" { log::warn!("ticho v režimu RUČNĚ"); }
                        }
                    } else { *g2.silence_since.lock() = None; }
                    // denní úlohy
                    let now = chrono::Local::now();
                    let day = now.format("%Y-%m-%d").to_string();
                    if chrono::Timelike::hour(&now) == c.general.backup.hour && last_daily != day {
                        last_daily = day;
                        if c.general.backup.enabled {
                            match backup_now(&db, &c, &data_dir) { Ok(p) => { *g2.last_backup.write() = Some(format!("{} ({})", p, now_utc())); log::info!("záloha: {p}"); } Err(e) => events.alert("error", "backup_failed", format!("Záloha databáze selhala: {e}")) }
                        }
                        if c.general.log_retention_days > 0 { let _ = maintenance(&db, c.general.log_retention_days, false); }
                    }
                }
            }).ok();
        }
        g
    }
}

// ---------------------------------------------------------------------------------------------
// Priorita vysílání: vlákno mixeru s nejvyšší prioritou, počítač neusne

pub fn boost_current_thread() {
    match thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max) {
        Ok(_) => log::info!("vlákno {} má nejvyšší prioritu", std::thread::current().name().unwrap_or("?")),
        Err(e) => log::warn!("prioritu vlákna nelze zvýšit: {e:?}"),
    }
}

pub fn keep_awake() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, ES_AWAYMODE_REQUIRED};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS};
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_AWAYMODE_REQUIRED);
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
        log::info!("Windows: uspání zakázáno, priorita procesu HIGH");
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id().to_string();
        let _ = std::process::Command::new("caffeinate").args(["-i", "-s", "-m", "-w", &pid]).spawn();
        log::info!("macOS: caffeinate – uspání zakázáno po dobu běhu");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FolderMapping;
    #[test]
    fn folder_mapping() {
        let mut c = Config::default();
        c.general.folder_mappings = vec![FolderMapping { from: "Z:\\Hudba".into(), to: "/Volumes/NAS/Hudba".into() }];
        let p = map_path(&c, "z:\\Hudba\\Rock\\a.mp3");
        if cfg!(windows) { assert_eq!(p, "\\Volumes\\NAS\\Hudba\\Rock\\a.mp3"); } else { assert_eq!(p, "/Volumes/NAS/Hudba/Rock/a.mp3"); }
        assert_eq!(map_path(&c, "/jinde/b.mp3"), "/jinde/b.mp3");
    }
    #[test]
    fn backup_and_rewrite() {
        let dir = std::env::temp_dir().join(format!("orca-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("orca.db")).unwrap();
        let mut c = Config::default();
        c.general.backup.keep = 2;
        for _ in 0..3 { backup_now(&db, &c, &dir).unwrap(); }
        assert_eq!(std::fs::read_dir(dir.join("Backups")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".db")).count(), 2);
        assert_eq!(rewrite_paths(&db, "/a/", "/b/").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
