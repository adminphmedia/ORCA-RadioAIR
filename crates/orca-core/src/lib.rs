//! ORCA RadioAIR – jádro odbavovacího systému pro rádio.

pub mod analysis;
pub mod audio_out;
pub mod automation;
pub mod config;
pub mod db;
pub mod decode;
pub mod encoder;
pub mod engine;
pub mod events;
pub mod library;
pub mod model;
pub mod nowplaying;
pub mod reports;
pub mod rotation;

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
}

impl Orca {
    pub fn start(data_dir: &Path) -> Result<Arc<Orca>> {
        std::fs::create_dir_all(data_dir)?;
        let config_path = data_dir.join("orca.toml");
        let cfg = Arc::new(RwLock::new(Config::load_or_create(&config_path)?));
        let db = Arc::new(db::Db::open(&data_dir.join("orca.db"))?);
        let events = events::EventBus::new();
        let library = library::Library::new(db.clone(), cfg.clone(), events.clone());
        let engine = engine::Engine::start(cfg.clone(), events.clone());
        { let db = db.clone(); engine.set_resolver(Arc::new(move |id| db.get_track(id).ok().flatten())); }
        let encoders = encoder::EncoderManager::new(engine.clone(), cfg.clone(), events.clone());
        let automation = automation::Automation::start(db.clone(), cfg.clone(), engine.clone(), encoders.clone(), events.clone(), data_dir.to_path_buf());
        if let Err(e) = library.start_watching() { events.alert("error", "watch_failed", e.to_string()); }
        encoders.autostart();
        log::info!("ORCA RadioAIR {VERSION} běží, data: {}", data_dir.display());
        Ok(Arc::new(Orca { data_dir: data_dir.to_path_buf(), config_path, cfg, db, events, library, engine, encoders, automation }))
    }

    /// Uloží novou konfiguraci a aplikuje ji (sledované složky, výstup, encodery).
    pub fn apply_config(&self, new: Config) -> Result<()> {
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
        self.events.emit(events::Event::ConfigChanged);
        Ok(())
    }
}
