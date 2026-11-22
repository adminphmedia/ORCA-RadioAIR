//! Konfigurace ORCA RadioAIR (soubor `orca.toml` v datové složce).

use crate::model::TrackKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub station: StationConfig,
    /// Sledované složky – soubory se do knihovny přidávají a aktualizují automaticky.
    pub watch_folders: Vec<WatchFolder>,
    pub audio: AudioConfig,
    pub autocue: AutoCueConfig,
    pub auto: AutoConfig,
    pub encoders: Vec<EncoderConfig>,
    pub nowplaying: NowPlayingConfig,
    pub api: ApiConfig,
    /// Cesta k ffmpeg; prázdné = vedle programu, pak PATH.
    pub ffmpeg_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            station: StationConfig::default(),
            watch_folders: vec![],
            audio: AudioConfig::default(),
            autocue: AutoCueConfig::default(),
            auto: AutoConfig::default(),
            encoders: vec![],
            nowplaying: NowPlayingConfig::default(),
            api: ApiConfig::default(),
            ffmpeg_path: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StationConfig {
    pub name: String,
    /// "cs" nebo "en"
    pub language: String,
    /// "dark", "light" nebo "system"
    pub theme: String,
    /// Posun času vysílání v sekundách (Jazler "Time Offset").
    pub time_offset_s: i64,
}

impl Default for StationConfig {
    fn default() -> Self {
        Self { name: "ORCA RadioAIR".into(), language: "cs".into(), theme: "dark".into(), time_offset_s: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchFolder {
    pub id: String,
    pub path: PathBuf,
    pub kind: TrackKind,
    /// Pevná kategorie pro všechny soubory ve složce (pokud není kategorie z podsložky).
    pub category: String,
    /// Kategorie = název první podsložky (doporučená praxe z Jazleru: složka Pop, Hot Current…).
    pub category_from_subfolder: bool,
    pub enabled: bool,
}

impl Default for WatchFolder {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            path: PathBuf::new(),
            kind: TrackKind::Song,
            category: String::new(),
            category_from_subfolder: true,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Název výstupního zařízení; prázdné = výchozí, "none" = bez zvukovky (jen stream).
    pub output_device: String,
    /// Samostatný výstup pro instant jingly (prázdné = hlavní výstup).
    pub instants_device: String,
    /// Velikost výstupního bufferu v ms.
    pub buffer_ms: u32,
    /// Plynulé ztlumení odcházející skladby od bodu mixu do konce.
    pub fade_on_segue: bool,
    /// Délka stáhnutí při ručním "Další".
    pub manual_fade_ms: u32,
    /// Délka stáhnutí při Stop.
    pub stop_fade_ms: u32,
    /// Normalizace hlasitosti podle LUFS.
    pub normalize: bool,
    pub target_lufs: f64,
    /// Ztlumení hudby pod voice trackem / sweeperem (dB).
    pub duck_db: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            output_device: String::new(),
            instants_device: String::new(),
            buffer_ms: 150,
            fade_on_segue: true,
            manual_fade_ms: 800,
            stop_fade_ms: 400,
            normalize: true,
            target_lufs: -16.0,
            duck_db: -8.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct CueThresholds {
    /// Začátek: první místo nad touto úrovní (dBFS).
    pub start_db: f32,
    /// Konec: poslední místo nad touto úrovní (dBFS).
    pub end_db: f32,
    /// Bod mixu: kde konec skladby zeslábne o tolik dB vůči její průměrné hlasitosti
    /// (relativně, funguje i u tiše namasterovaných skladeb). None = bez mixu, hraje do konce.
    pub mix_db: Option<f32>,
}

impl Default for CueThresholds {
    fn default() -> Self { Self { start_db: -45.0, end_db: -45.0, mix_db: Some(-12.0) } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoCueConfig {
    pub per_kind: BTreeMap<TrackKind, CueThresholds>,
}

impl Default for AutoCueConfig {
    fn default() -> Self {
        let mut m = BTreeMap::new();
        m.insert(TrackKind::Song, CueThresholds::default());
        m.insert(TrackKind::Jingle, CueThresholds { start_db: -50.0, end_db: -50.0, mix_db: Some(-20.0) });
        // Reklamy celé od začátku do konce, bez mixu (Jazler kap. 7.1.8.5).
        m.insert(TrackKind::Spot, CueThresholds { start_db: -90.0, end_db: -90.0, mix_db: None });
        m.insert(TrackKind::Sweeper, CueThresholds { start_db: -50.0, end_db: -50.0, mix_db: None });
        m.insert(TrackKind::VoiceTrack, CueThresholds { start_db: -50.0, end_db: -50.0, mix_db: None });
        m.insert(TrackKind::Asset, CueThresholds { start_db: -60.0, end_db: -60.0, mix_db: None });
        Self { per_kind: m }
    }
}

impl AutoCueConfig {
    pub fn for_kind(&self, k: TrackKind) -> CueThresholds {
        self.per_kind.get(&k).copied().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClockStep {
    pub kind: TrackKind,
    /// Prázdné = libovolná kategorie.
    #[serde(default)]
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Clock {
    pub name: String,
    pub color: String,
    pub steps: Vec<ClockStep>,
    /// Kategorie kompenzačních skladeb (backup filter).
    pub backup_category: String,
}

impl Default for Clock {
    fn default() -> Self {
        Self { name: "Default".into(), color: "#3b82f6".into(), steps: vec![ClockStep { kind: TrackKind::Song, category: String::new() }], backup_category: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoConfig {
    /// Spustit v režimu Auto.
    pub start_in_auto: bool,
    /// Kolik položek držet ve frontě (Classic auto).
    pub min_queue: usize,
    /// Minimální rozestup stejného interpreta (min).
    pub artist_separation_min: u32,
    /// Minimální rozestup stejné skladby (min).
    pub song_separation_min: u32,
    /// Selection randomness: 0 = striktní rotace, X = náhodně z X kandidátů.
    pub randomness: u32,
    pub clocks: Vec<Clock>,
    /// Týdenní mřížka: klíč "den-hodina" (den 1=Po … 7=Ne), hodnota = název clocku.
    pub clock_grid: BTreeMap<String, String>,
    pub default_clock: String,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            start_in_auto: true,
            min_queue: 6,
            artist_separation_min: 45,
            song_separation_min: 180,
            randomness: 0,
            clocks: vec![Clock::default()],
            clock_grid: BTreeMap::new(),
            default_clock: "Default".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StreamFormat { Mp3, Aac, Ogg, Opus }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EncoderConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub autostart: bool,
    pub host: String,
    pub port: u16,
    pub mount: String,
    pub username: String,
    pub password: String,
    pub format: StreamFormat,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub stream_name: String,
    pub genre: String,
    pub description: String,
    pub url: String,
    pub public: bool,
    pub send_metadata: bool,
    pub reconnect_s: u64,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Icecast".into(),
            enabled: true,
            autostart: true,
            host: "127.0.0.1".into(),
            port: 8000,
            mount: "/live".into(),
            username: "source".into(),
            password: "hackme".into(),
            format: StreamFormat::Mp3,
            bitrate_kbps: 128,
            sample_rate: 44100,
            channels: 2,
            stream_name: "ORCA RadioAIR".into(),
            genre: String::new(),
            description: String::new(),
            url: String::new(),
            public: false,
            send_metadata: true,
            reconnect_s: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NowPlayingConfig {
    pub enabled: bool,
    /// Prázdné = <data>/Exports
    pub export_dir: PathBuf,
    /// Pro každý typ: "" = metadata skladby, "NO-UPDATE" = neaktualizovat, jiný text = vlastní zpráva.
    pub per_kind: BTreeMap<TrackKind, String>,
    pub history_size: usize,
    pub next_size: usize,
    pub export_cover: bool,
    /// HTTP POST s JSON při změně skladby (webhooky).
    pub webhooks: Vec<String>,
}

impl Default for NowPlayingConfig {
    fn default() -> Self {
        let mut m = BTreeMap::new();
        m.insert(TrackKind::Song, String::new());
        m.insert(TrackKind::Jingle, "NO-UPDATE".into());
        m.insert(TrackKind::Spot, "NO-UPDATE".into());
        m.insert(TrackKind::Sweeper, "NO-UPDATE".into());
        m.insert(TrackKind::VoiceTrack, "NO-UPDATE".into());
        m.insert(TrackKind::Asset, String::new());
        Self { enabled: true, export_dir: PathBuf::new(), per_kind: m, history_size: 10, next_size: 5, export_cover: true, webhooks: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub bind: String,
    /// Volitelný token pro vzdálený přístup (hlavička `Authorization: Bearer …`).
    pub token: String,
    /// Složka s webovým UI (build z Lovable). Prázdné = <program>/ui
    pub ui_dir: PathBuf,
}

impl Default for ApiConfig {
    fn default() -> Self { Self { bind: "127.0.0.1:8750".into(), token: String::new(), ui_dir: PathBuf::new() } }
}

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let s = std::fs::read_to_string(path).with_context(|| format!("čtení {}", path.display()))?;
            let c: Config = toml::from_str(&s).with_context(|| format!("chyba v {}", path.display()))?;
            Ok(c)
        } else {
            let c = Config::default();
            c.save(path)?;
            Ok(c)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
        let s = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn clock_by_name(&self, name: &str) -> Option<&Clock> {
        self.auto.clocks.iter().find(|c| c.name == name)
    }
}
