//! ORCA RadioAIR – jádro odbavovacího systému pro rádio.

pub mod ads;
pub mod ai;
pub mod analysis;
pub mod audio_out;
pub mod automation;
pub mod catalog;
pub mod cmo;
pub mod config;
pub mod db;
pub mod decode;
pub mod dsp;
pub mod encoder;
pub mod engine;
pub mod epg;
pub mod events;
pub mod library;
pub mod meta;
pub mod mic;
pub mod general;
pub mod import;
pub mod info;
pub mod station;
pub mod gpio;
pub mod model;
pub mod monitor;
pub mod nowplaying;
pub mod processing;
pub mod rds;
pub mod recorder;
pub mod relay;
pub mod reports;
pub mod rotation;
pub mod sched;
pub mod selector;
pub mod slides;
pub mod spots;
pub mod stitcher;
pub mod users;
pub mod video;
pub mod business;
pub mod countdown;
pub mod airlog;
pub mod npservices;
pub mod fx;
pub mod podcast;
pub mod transition;
pub mod storage;
pub mod weather;

use anyhow::Result;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use config::Config;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Výchozí datová složka:
/// macOS `~/Library/Application Support/ORCA RadioAIR`, Windows `%APPDATA%\ORCA RadioAIR`.
pub fn default_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("ORCA RadioAIR")
}

/// Otevření samostatného okna s panelem studia (desktopová aplikace – více obrazovek): (panel, titulek)
pub mod update;
pub mod link;

/// Nativní okna panelů (desktop): (akce "open" | "close", panel, titulek, owner = id okna studia, které panel vyjmulo)
pub type WindowHook = Arc<dyn Fn(&str, &str, &str, &str) -> std::result::Result<(), String> + Send + Sync>;

/// Celá běžící aplikace.
pub struct Orca {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub cfg: Arc<RwLock<Config>>,
    pub db: Arc<db::Db>,
    pub events: events::EventBus,
    pub library: Arc<library::Library>,
    pub engine: Arc<engine::Engine>,
    pub encoders: Arc<encoder::EncoderManager>,
    pub automation: Arc<automation::Automation>,
    pub recorder: Arc<recorder::Recorder>,
    pub meta: Arc<meta::MetaHub>,
    pub processing: Arc<processing::Processing>,
    pub window_hook: RwLock<Option<WindowHook>>,
    pub general: Arc<general::General>,
    pub users: users::Users,
    pub monitor: monitor::Monitor,
    pub gpio: Arc<gpio::Gpio>,
    pub inbox: Arc<station::Inbox>,
    pub video: Arc<video::VideoManager>,
    pub storage: storage::Storage,
    pub airlog: Arc<airlog::AirLogger>,
    pub update_hook: RwLock<Option<update::UpdateHook>>,
    pub update_state: Arc<parking_lot::Mutex<update::UpdateState>>,
    pub link: link::Link,
}

impl Orca {
    /// Akce studia podle názvu (klávesové zkratky – i globální z desktopové aplikace, GPIO/MIDI).
    pub fn studio_action(&self, action: &str) -> bool {
        use engine::Cmd;
        let st = self.engine.snapshot();
        match action {
            "play" => self.engine.send(Cmd::Play),
            "next" => self.engine.send(Cmd::Next),
            "pause" => self.engine.send(Cmd::Pause),
            "stop" => self.engine.send(Cmd::Stop),
            "stop_after" => self.engine.send(Cmd::StopAfterCurrent(!st.stop_after_current)),
            "loop" => self.engine.send(Cmd::Loop(!st.loop_current)),
            "restart" => self.engine.send(Cmd::Restart),
            "mic" => self.engine.send(Cmd::TalkOver(!st.talkover)),
            "stop_instants" => self.engine.send(Cmd::StopInstants),
            "continue" => { self.engine.live_continue(); }
            "auto" => self.automation.set_mode(automation::Mode::Auto),
            "manual" => self.automation.set_mode(automation::Mode::Manual),
            _ => return false,
        }
        true
    }

    pub fn start(data_dir: &Path) -> Result<Arc<Orca>> {
        std::fs::create_dir_all(data_dir)?;
        // nová verze: záloha databáze a nastavení dřív, než je migrace změní
        let (updated_from, pre_update_backup) = update::backup_on_version_change(data_dir);
        let config_path = data_dir.join("orca.toml");
        let mut cfg0 = Config::load_or_create(&config_path)?;
        if library::ensure_kind_folders(&mut cfg0, data_dir) { cfg0.save(&config_path)?; }
        let cfg = Arc::new(RwLock::new(cfg0));
        let db = Arc::new(db::Db::open(&data_dir.join("orca.db"))?);
        let events = events::EventBus::new();
        let library = library::Library::new(db.clone(), cfg.clone(), events.clone(), data_dir.to_path_buf());
        let engine = engine::Engine::start(cfg.clone(), events.clone());
        { let db = db.clone(); engine.set_resolver(Arc::new(move |id| db.get_track(id).ok().flatten())); }
        {
            let (db, cfg) = (db.clone(), cfg.clone());
            engine.set_stitch_resolver(Arc::new(move |kind, upcoming| {
                let c = cfg.read().clone();
                stitcher::resolve(&db, &c, kind, upcoming, automation::now_local(&c))
            }));
        }
        if let Err(e) = catalog::ensure_default_palette(&db) { log::warn!("palety: {e}"); }
        match ads::migrate_text_clients(&db) { Ok(n) if n > 0 => log::info!("reklama: převedeno {n} klientů do databáze klientů"), Err(e) => log::warn!("klienti: {e}"), _ => {} }
        let encoders = encoder::EncoderManager::new(engine.clone(), cfg.clone(), events.clone());
        let _fx = fx::FxManager::start(cfg.clone(), engine.clone(), events.clone());
        let airlog = airlog::AirLogger::start(data_dir.to_path_buf(), cfg.clone(), engine.clone(), events.clone());
        {
            // záznamy pořadů určené do knihovny (repríza)
            let (a, lib) = (airlog.clone(), library.clone());
            let (cfg2, db2, dd2, ev2) = (cfg.clone(), db.clone(), data_dir.to_path_buf(), events.clone());
            std::thread::Builder::new().name("orca-airlog-lib".into()).spawn(move || loop {
                for (p, k, cat) in a.take_library_items() { let r = lib.add_paths(&[p], k, &cat); if !r.errors.is_empty() { log::warn!("záznam pořadu do knihovny: {:?}", r.errors); } }
                for (p, title, auto) in a.take_podcast_items() {
                    let c = cfg2.read().clone();
                    match podcast::from_show(&c, &db2, &dd2, &p, &title, auto) {
                        Ok(m) => log::info!("podcast: {m}"),
                        Err(e) => ev2.alert("warning", "podcast", format!("Podcast z pořadu {title}: {e:#}")),
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(20));
            }).ok();
        }
        let automation = automation::Automation::start(db.clone(), cfg.clone(), engine.clone(), encoders.clone(), events.clone(), data_dir.to_path_buf());
        let processing = processing::Processing::start(db.clone(), cfg.clone(), engine.clone(), events.clone(), data_dir.to_path_buf());
        let meta = meta::MetaHub::start(db.clone(), cfg.clone(), engine.clone(), encoders.clone(), data_dir.to_path_buf());
        if let Err(e) = library.start_watching() { events.alert("error", "watch_failed", e.to_string()); }
        let general = general::General::start(db.clone(), cfg.clone(), engine.clone(), events.clone(), data_dir.to_path_buf(), automation.clone());
        if cfg.read().general.keep_awake { general::keep_awake(); }
        let gpio = gpio::Gpio::start(cfg.clone(), engine.clone(), automation.clone());
        let inbox = station::Inbox::start(cfg.clone(), library.clone(), db.clone(), data_dir.to_path_buf());
        { let e = engine.clone(); mic::start(cfg.clone(), Arc::new(move || e.rate.load(std::sync::atomic::Ordering::Relaxed))); }
        {
            // automatický úklid souborů po platnosti (jednou za hodinu)
            let (c, lib, ev) = (cfg.clone(), library.clone(), events.clone());
            std::thread::Builder::new().name("orca-expired".into()).spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(120));
                let days = c.read().library.expired_cleanup_days;
                if days > 0 {
                    match lib.delete_expired(None, days) { Ok(n) if n > 0 => ev.alert("info", "expired_cleanup", format!("Úklid knihovny: {n} souborů po platnosti přesunuto do koše")), Err(e) => log::warn!("úklid po platnosti: {e}"), _ => {} }
                }
                std::thread::sleep(std::time::Duration::from_secs(3480));
            }).ok();
        }
        countdown::set_data_dir(data_dir);
        storage::start_watch(cfg.clone(), data_dir.to_path_buf(), events.clone());
        let video = video::VideoManager::start(cfg.clone(), data_dir.to_path_buf(), library.clone(), events.clone());
        link::init_standby(&cfg.read());
        encoders.autostart();
        update::mark_version(data_dir);
        if let Some(v) = &updated_from { events.alert("info", "updated", format!("ORCA RadioAIR aktualizována z verze {v} na {VERSION}. Nastavení, knihovna i plány zůstaly zachované.")); }
        let update_state = Arc::new(parking_lot::Mutex::new(update::UpdateState { current: VERSION.into(), updated_from, pre_update_backup, ..Default::default() }));
        log::info!("ORCA RadioAIR {VERSION} běží, data: {}", data_dir.display());
        Ok(Arc::new(Orca { data_dir: data_dir.to_path_buf(), config_path, cfg, db, events, library, engine, encoders, automation, recorder: Arc::new(recorder::Recorder::new()), meta, processing, window_hook: RwLock::new(None), general, users: users::Users::default(), monitor: monitor::Monitor::default(), gpio, inbox, video, storage: storage::Storage::default(), airlog, update_hook: RwLock::new(None), update_state, link: link::Link::default() }))
    }

    /// Zjistí, zda je k dispozici nová verze (jen v desktopové aplikaci).
    pub fn update_check(&self) -> update::UpdateState {
        let Some(h) = self.update_hook.read().clone() else { return self.update_state.lock().clone() };
        { let mut s = self.update_state.lock(); if s.checking || s.installing { return s.clone(); } s.checking = true; s.supported = true; }
        let r = (h.check)();
        let mut s = self.update_state.lock();
        s.checking = false;
        s.checked_at = Some(chrono::Local::now().to_rfc3339());
        match r {
            Ok(a) => {
                let notify = a.as_ref().map(|x| s.available.as_ref().map(|y| y.version != x.version).unwrap_or(true)).unwrap_or(false);
                if notify { if let Some(x) = &a { self.events.alert("info", "update_available", format!("K dispozici je nová verze ORCA RadioAIR {} – Obecné → Aktualizace.", x.version)); } }
                s.available = a; s.error = None;
            }
            Err(e) => s.error = Some(e),
        }
        s.clone()
    }

    /// Stáhne a nainstaluje novou verzi; aplikace se poté sama restartuje.
    pub fn update_install(&self) -> Result<(), String> {
        let h = self.update_hook.read().clone().ok_or("Aktualizace jsou dostupné jen v desktopové aplikaci.")?;
        { let mut s = self.update_state.lock(); if s.installing { return Err("Aktualizace už probíhá.".into()); } s.installing = true; s.progress = Some(0); s.error = None; }
        // pojistka navíc: záloha databáze a nastavení
        let c = self.cfg.read().clone();
        if let Err(e) = general::backup_now(&self.db, &c, &self.data_dir) { log::warn!("záloha před aktualizací: {e}"); }
        let r = (h.install)(self.update_state.clone());
        if let Err(e) = &r { let mut s = self.update_state.lock(); s.installing = false; s.progress = None; s.error = Some(e.clone()); }
        r
    }

    /// Uloží novou konfiguraci a aplikuje ji (sledované složky, výstup, encodery).
    pub fn apply_config(&self, mut new: Config) -> Result<()> {
        new.normalize();
        library::ensure_kind_folders(&mut new, &self.data_dir);
        let old = self.cfg.read().clone();
        new.save(&self.config_path)?;
        *self.cfg.write() = new.clone();
        let wf_changed = serde_json::to_string(&old.watch_folders)? != serde_json::to_string(&new.watch_folders)?;
        if wf_changed { self.library.start_watching()?; }
        if old.audio.output_device != new.audio.output_device || old.audio.buffer_ms != new.audio.buffer_ms {
            self.engine.send(engine::Cmd::ReopenOutput);
        }
        if serde_json::to_string(&old.encoders)? != serde_json::to_string(&new.encoders)? {
            self.encoders.stop_all();
            self.encoders.autostart();
        }
        if serde_json::to_string(&old.autocue)? != serde_json::to_string(&new.autocue)? {
            log::info!("změna autocue – nové prahy platí pro nově analyzované soubory");
        }
        if serde_json::to_string(&old.gpio.inputs)? != serde_json::to_string(&new.gpio.inputs)? { self.gpio.restart(); }
        if serde_json::to_string(&old.meta)? != serde_json::to_string(&new.meta)? || serde_json::to_string(&old.relay)? != serde_json::to_string(&new.relay)? { self.meta.refresh(); }
        self.events.emit(events::Event::ConfigChanged);
        Ok(())
    }

    /// Změna složky stanice. `move_files`: přesunout soubory ze staré složky; `rewrite_db`: přepsat cesty v databázi
    /// (vypněte jen tehdy, když už cesty odpovídají – např. po ruční kopii a přemapování).
    pub fn relocate_station(&self, new_root: &Path, move_files: bool, rewrite_db: bool) -> Result<station::RelocateReport> {
        let cfg = self.cfg.read().clone();
        let old = library::library_root(&cfg, &self.data_dir);
        if new_root.as_os_str().is_empty() { anyhow::bail!("zadejte novou složku stanice"); }
        if new_root == old { anyhow::bail!("nová složka je stejná jako současná"); }
        if new_root.starts_with(&old) { anyhow::bail!("nová složka nesmí být uvnitř současné složky stanice"); }
        if old.starts_with(new_root) { anyhow::bail!("nová složka nesmí obsahovat současnou složku stanice"); }
        std::fs::create_dir_all(new_root)?;
        let mut rep = station::RelocateReport { from: old.display().to_string(), to: new_root.display().to_string(), ..Default::default() };
        self.library.stop_watching();
        if move_files && old.is_dir() { station::move_tree(&old, new_root, &mut rep); }
        if rewrite_db { rep.rewritten = station::rewrite_all_paths(&self.db, &station::with_sep(&old), &station::with_sep(new_root))?; }
        let mut new = cfg.clone();
        new.library.root = new_root.to_path_buf();
        for wf in new.watch_folders.iter_mut() { wf.path = station::remap(&wf.path, &old, new_root); }
        if !new.library.inbox.dir.as_os_str().is_empty() { new.library.inbox.dir = station::remap(&new.library.inbox.dir, &old, new_root); }
        self.apply_config(new)?;
        self.library.start_watching()?;
        log::info!("složka stanice přesunuta: {} → {} ({} souborů, {} záznamů)", rep.from, rep.to, rep.moved_files, rep.rewritten);
        Ok(rep)
    }
}
