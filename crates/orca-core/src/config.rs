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
    pub library: LibraryConfig,
    pub spots: SpotsConfig,
    pub stitcher: StitcherConfig,
    pub recorder: RecorderConfig,
    pub schedule: ScheduleConfig,
    pub relay: RelayConfig,
    pub meta: MetaConfig,
    /// Mikrofon studia: vstup a přimíchání do programu
    pub mic: crate::mic::MicConfig,
    /// AI pro informace o skladbách (poskytovatel, model, styl)
    pub ai: crate::ai::AiConfig,
    pub video: crate::video::VideoConfig,
    pub general: GeneralConfig,
    /// Vstupy z mixážního pultu a ovladačů (fader start, GPI, MIDI, TCP)
    pub gpio: GpioConfig,
    /// Monitoring – nahrávání vysílání (logger, archiv, aircheck, záznamy pořadů)
    pub airlog: crate::airlog::AirLogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GpioConfig {
    pub inputs: Vec<GpiInput>,
    /// TCP port pro textové příkazy (0 = vypnuto), např. „ACTION continue“ nebo „GPI mic1 OFF“
    pub tcp_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GpiInput {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// serial | midi | tcp
    pub source: String,
    /// serial: port (COM3, /dev/tty.usbserial…)
    pub port: String,
    /// serial: cts | dsr | dcd | ri
    pub line: String,
    /// obrácená logika (kontakt rozpojený = zapnuto)
    pub invert: bool,
    /// midi: část názvu zařízení
    pub midi_device: String,
    /// midi: cc | note
    pub midi_type: String,
    /// midi: kanál 1–16 (0 = libovolný)
    pub midi_channel: u8,
    pub midi_number: u8,
    /// midi CC: hodnota, nad kterou je „zapnuto“ (fader otevřen)
    pub threshold: u8,
    /// Akce při sepnutí (šavle nahoru / tlačítko stisknuto)
    pub on_action: String,
    /// Akce při rozepnutí (šavle dolů / tlačítko puštěno)
    pub off_action: String,
    pub debounce_ms: u32,
}
impl Default for GpiInput {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), enabled: true, source: "midi".into(), port: String::new(), line: "cts".into(), invert: false,
            midi_device: String::new(), midi_type: "cc".into(), midi_channel: 0, midi_number: 0, threshold: 10, on_action: "mic_on".into(), off_action: "continue".into(), debounce_ms: 50 }
    }
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
            library: LibraryConfig::default(),
            spots: SpotsConfig::default(),
            stitcher: StitcherConfig::default(),
            recorder: RecorderConfig::default(),
            schedule: ScheduleConfig::default(),
            relay: RelayConfig::default(),
            meta: MetaConfig::default(),
            mic: crate::mic::MicConfig::default(),
            ai: crate::ai::AiConfig::default(),
            video: crate::video::VideoConfig::default(),
            general: GeneralConfig::default(),
            gpio: GpioConfig::default(),
            airlog: crate::airlog::AirLogConfig::default(),
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
    /// Zisk zdrojů podle typu (trim před mixem, dB) – skladby, reklamy, jingly…
    pub kind_trim_db: BTreeMap<TrackKind, f32>,
    /// Předposlech (monitor / PFL) – samostatný výstup; prázdné = výchozí zařízení
    pub monitor_device: String,
    /// Vyrovnání hlasitosti programu na cílovou hlasitost (LUFS) – pro všechny zdroje
    pub leveler: LevelerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelerConfig {
    pub enabled: bool,
    /// Max. zesílení tichého programu (dB)
    pub max_boost_db: f64,
    /// Max. zeslabení hlasitého programu (dB)
    pub max_cut_db: f64,
    /// slow | normal | fast
    pub speed: String,
}
impl Default for LevelerConfig {
    fn default() -> Self { Self { enabled: true, max_boost_db: 10.0, max_cut_db: 12.0, speed: "normal".into() } }
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
            kind_trim_db: BTreeMap::new(),
            monitor_device: String::new(),
            leveler: LevelerConfig::default(),
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
        m.insert(TrackKind::Instant, CueThresholds { start_db: -50.0, end_db: -50.0, mix_db: None });
        m.insert(TrackKind::Bed, CueThresholds { start_db: -50.0, end_db: -50.0, mix_db: None });
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
    /// Sweeper / voice track: pozice přes skladbu (start, before_intro, before_end, bridge, independent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// Krok Stitcheru místo souboru: hooks | time | news
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stitch: Option<String>,
    /// Pevný konkrétní soubor (např. znělka) místo výběru z kategorie
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i64>,
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
        Self { name: "Default".into(), color: "#3b82f6".into(), steps: vec![ClockStep { kind: TrackKind::Song, category: String::new(), position: None, stitch: None, track_id: None }], backup_category: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoConfig {
    /// Spustit v režimu Auto.
    pub start_in_auto: bool,
    /// Kolik položek držet ve frontě (Classic auto).
    pub min_queue: usize,
    /// Mít ve frontě vždy načtenou celou další hodinu (jako Jazler) – vidíte, co se bude vysílat.
    pub fill_next_hour: bool,
    /// Doplňovat frontu i v ručním režimu (položky se nespouštějí samy, jen jsou připravené).
    pub fill_in_manual: bool,
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
            fill_next_hour: true,
            fill_in_manual: false,
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
    /// Odesílání „co hraje“: FTP/FTPS/SFTP, TuneIn, HTTP, TCP, UDP
    pub services: Vec<crate::npservices::NpService>,
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
        m.insert(TrackKind::Instant, "NO-UPDATE".into());
        m.insert(TrackKind::Bed, "NO-UPDATE".into());
        Self { enabled: true, export_dir: PathBuf::new(), per_kind: m, history_size: 10, next_size: 5, export_cover: true, webhooks: vec![], services: vec![] }
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
            let mut c: Config = toml::from_str(&s).with_context(|| format!("chyba v {}", path.display()))?;
            c.normalize();
            Ok(c)
        } else {
            let c = Config::default();
            c.save(path)?;
            Ok(c)
        }
    }

    /// Doplní chybějící hodnoty pro nové typy knihoven (starší konfigurace).
    pub fn normalize(&mut self) {
        let ac = AutoCueConfig::default();
        let np = NowPlayingConfig::default();
        // klávesy: doplnit nové akce a alternativy ke starým výchozím F-klávesám
        for (a, d) in DEFAULT_HOTKEYS {
            let old_default = d.split(',').next().unwrap_or("").trim();
            match self.general.hotkeys.get(a) {
                None => { self.general.hotkeys.insert(a.to_string(), d.to_string()); }
                Some(v) if v.trim() == old_default => { self.general.hotkeys.insert(a.to_string(), d.to_string()); }
                _ => {}
            }
        }
        for k in TrackKind::ALL {
            if let Some(v) = ac.per_kind.get(&k) { self.autocue.per_kind.entry(k).or_insert(*v); }
            self.library.prefixes.entry(k).or_insert_with(|| k.default_prefix().to_string());
            if let Some(v) = np.per_kind.get(&k) { self.nowplaying.per_kind.entry(k).or_insert_with(|| v.clone()); }
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


// ---------------------------------------------------------------------------------------------
// Knihovna

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
    /// Kořenová složka knihovny. Prázdné = <data>/Library. Pro každý typ v ní vznikne podsložka
    /// (Songs, Spots, Jingles …) a ta se automaticky sleduje.
    pub root: PathBuf,
    /// Automaticky vytvořit a sledovat podsložky pro všechny typy.
    pub auto_folders: bool,
    /// Prefix playlister kódu pro každý typ (Jazler library settings).
    pub prefixes: BTreeMap<TrackKind, String>,
    /// Šablona exportu pro playlister: {CODE} {ARTIST} {TITLE} {ALBUM} {CATEGORY} {CATEGORY2} {CATEGORY3}
    /// {DURATION} {DURATION_S} {FILENAME} {PATH} {ISRC} {YEAR}
    pub export_template: String,
    /// Při přidání souboru se stejným názvem jako už v knihovně je: "rename" | "replace" | "skip"
    pub on_duplicate: String,
    /// Data stanice (obrázky moderátorů a pořadů, obaly, obrázky ke spotům) ukládat do `<stanice>/Data`.
    pub data_in_station: bool,
    /// Při vytváření struktury založit podsložku pro každou kategorii.
    pub category_folders: bool,
    /// Automatická složka (inbox): co se do ní nakopíruje, samo se zařadí do knihovny.
    pub inbox: InboxConfig,
    /// Soubory po platnosti (ne hudbu) automaticky přesunout do koše knihovny po tolika dnech od konce platnosti; 0 = jen ručně
    pub expired_cleanup_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InboxConfig {
    pub enabled: bool,
    /// Prázdné = `<stanice>/Import`.
    pub dir: PathBuf,
    /// Typ pro soubory položené přímo do kořene automatické složky.
    pub default_kind: TrackKind,
    /// Soubor se zpracuje, až se jeho velikost tolik sekund nemění (dokončené kopírování).
    pub settle_s: u32,
    /// Převzít značky (cart chunk, RadioDJ, mAirList .mmd) uložené v souboru nebo vedle něj.
    pub read_markers: bool,
}

impl Default for InboxConfig {
    fn default() -> Self { Self { enabled: true, dir: PathBuf::new(), default_kind: TrackKind::Song, settle_s: 5, read_markers: true } }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            auto_folders: true,
            prefixes: TrackKind::ALL.iter().map(|k| (*k, k.default_prefix().to_string())).collect(),
            export_template: "{CODE};{ARTIST};{TITLE};{DURATION};{CATEGORY};{FILENAME}".into(),
            on_duplicate: "rename".into(),
            data_in_station: true,
            category_folders: true,
            inbox: InboxConfig::default(), expired_cleanup_days: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Spoty / reklamy

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BreakDef {
    /// "HH:MM"
    pub time: String,
    /// start = blok začíná v daný čas, end = blok v daný čas končí,
    /// time = položky s prioritou "ohlášení času" hrají co nejblíž danému času
    pub position: String,
    /// Viditelný při programování kampaní.
    pub visible: bool,
    /// Start immediately (Po…Ne): v daný čas stáhne hrající skladbu a blok začne přesně.
    pub start_immediately: [bool; 7],
    /// Limit délky bloku v sekundách (0 = bez limitu) – pro přehled bloků.
    pub max_duration_s: u32,
    pub name: String,
}

impl Default for BreakDef {
    fn default() -> Self {
        Self { time: "00:00".into(), position: "start".into(), visible: true, start_immediately: [false; 7], max_duration_s: 0, name: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PriorityDef {
    pub num: u32,
    pub name: String,
    /// "" | "time" (ohlášení času) | "playlister" | "optional" (hraje jen když v bloku je reklama)
    pub special: String,
    pub color: String,
}

impl Default for PriorityDef {
    fn default() -> Self { Self { num: 1, name: String::new(), special: String::new(), color: String::new() } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotsConfig {
    pub breaks: Vec<BreakDef>,
    pub priorities: Vec<PriorityDef>,
    /// Jak dlouho dopředu se bloky vkládají do decků (min).
    pub lookahead_min: u32,
    /// Vkládat bloky automaticky v režimu Auto.
    pub auto_insert: bool,
    /// Složka pro automatický import TXT plánů z traffic softwaru (YYYYMMDD.txt).
    pub playlister_import_dir: PathBuf,
    /// Kategorie kampaní (Reklama, Station ID, Upoutávka …).
    pub campaign_categories: Vec<String>,
}

impl Default for SpotsConfig {
    fn default() -> Self {
        let mut breaks = vec![];
        for h in 0..24 {
            for m in [0, 30] {
                breaks.push(BreakDef { time: format!("{h:02}:{m:02}"), ..Default::default() });
            }
        }
        let p = |num: u32, name: &str, special: &str| PriorityDef { num, name: name.into(), special: special.into(), color: String::new() };
        Self {
            breaks,
            priorities: vec![
                p(1, "Station ID", ""),
                p(2, "Začátek reklam", "optional"),
                p(3, "Ohlášení času", "time"),
                p(4, "Reklamy", ""),
                p(5, "Reklamy (traffic)", "playlister"),
                p(6, "Počasí", ""),
                p(7, "Zprávy", ""),
                p(8, "Konec reklam", "optional"),
            ],
            lookahead_min: 60,
            auto_insert: true,
            playlister_import_dir: PathBuf::new(),
            campaign_categories: vec!["Reklama".into(), "Station ID".into(), "Upoutávka".into(), "Sponzoring".into(), "Ohlášení".into()],
        }
    }
}

impl SpotsConfig {
    pub fn priority(&self, num: u32) -> Option<&PriorityDef> { self.priorities.iter().find(|p| p.num == num) }
    pub fn special_priority(&self, special: &str) -> Option<u32> { self.priorities.iter().find(|p| p.special == special).map(|p| p.num) }
}

// ---------------------------------------------------------------------------------------------
// Stitcher

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StitchHooksConfig {
    pub opening_track_id: Option<i64>,
    pub separator_track_id: Option<i64>,
    pub closing_track_id: Option<i64>,
    pub default_track_id: Option<i64>,
    /// Kolik následujících skladeb ohlásit.
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StitchTimeConfig {
    pub opening_track_id: Option<i64>,
    pub bed_track_id: Option<i64>,
    pub closing_track_id: Option<i64>,
    pub default_track_id: Option<i64>,
    /// Kategorie se soubory hodin: název souboru obsahuje číslo hodiny (0–23), např. "HOD_16.wav".
    pub hours_kind: Option<TrackKind>,
    pub hours_category: String,
    /// Kategorie se soubory minut (0–59), např. "MIN_04.wav". Minuta 0 se vynechá, pokud soubor chybí.
    pub minutes_kind: Option<TrackKind>,
    pub minutes_category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StitchNewsConfig {
    pub opening_track_id: Option<i64>,
    pub bed_track_id: Option<i64>,
    pub closing_track_id: Option<i64>,
    pub default_track_id: Option<i64>,
    /// Zprávy = nejnovější soubor v této knihovně a kategorii.
    pub news_kind: Option<TrackKind>,
    pub news_category: String,
    /// Ztlumení podkresu pod hlasem (dB).
    pub bed_db: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StitcherConfig {
    pub hooks: StitchHooksConfig,
    pub time: StitchTimeConfig,
    pub news: StitchNewsConfig,
}

impl Default for StitcherConfig {
    fn default() -> Self {
        Self {
            hooks: StitchHooksConfig { count: 3, ..Default::default() },
            time: StitchTimeConfig { hours_kind: Some(TrackKind::Asset), hours_category: "Hodiny".into(), minutes_kind: Some(TrackKind::Asset), minutes_category: "Minuty".into(), ..Default::default() },
            news: StitchNewsConfig { news_kind: Some(TrackKind::Asset), news_category: "Zprávy".into(), bed_db: -12.0, ..Default::default() },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Studio – nahrávání moderátorských vstupů

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecorderConfig {
    /// Vstupní zařízení; prázdné = výchozí.
    pub input_device: String,
    pub default_kind: TrackKind,
    pub default_category: String,
    /// Mono nahrávka (mikrofon) – menší soubory.
    pub mono: bool,
}

impl Default for RecorderConfig {
    fn default() -> Self { Self { input_device: String::new(), default_kind: TrackKind::VoiceTrack, default_category: "Studio".into(), mono: true } }
}

// ---------------------------------------------------------------------------------------------
// Plánování

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    /// Automatika hraje podle vygenerovaného plánu vysílání (Jazler SOHO), jinak přímo z clocků.
    pub use_plan: bool,
    /// Plán generovat automaticky na tolik hodin dopředu (0 = jen ručně).
    pub auto_generate_hours: u32,
    /// Spouštět naplánované playlisty, programovou mřížku a události.
    pub enabled: bool,
    /// Jak dlouho dopředu se naplánované položky vkládají do decků (min).
    pub lookahead_min: u32,
    /// Složka pro automatický import playlistů (TXT/M3U, název = YYYYMMDD-HHMM.txt) → Imported/, _Errors.txt
    pub playlist_import_dir: PathBuf,
}

impl Default for ScheduleConfig {
    fn default() -> Self { Self { use_plan: false, auto_generate_hours: 0, enabled: true, lookahead_min: 30, playlist_import_dir: PathBuf::new() } }
}

// ---------------------------------------------------------------------------------------------
// Rebroadcast (převzetí vysílání z jiného studia / streamu / vstupu zvukové karty)

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RelaySource {
    pub id: String,
    pub name: String,
    /// url (Icecast/SHOUTcast/HLS/SRT/RTP/UDP – cokoli, co umí ffmpeg) | device (vstup zvukové karty)
    #[serde(rename = "type")]
    pub typ: String,
    pub url: String,
    pub device: String,
    /// Záložní zdroje v pořadí (id) – při výpadku nebo tichu se přepne na další.
    pub backup_ids: Vec<String>,
    /// Ticho pod touto úrovní (dBFS) …
    pub silence_db: f32,
    /// … déle než tolik sekund = výpadek (0 = nehlídat).
    pub silence_timeout_s: u32,
    /// Max. čekání na připojení (s).
    pub connect_timeout_s: u32,
    pub gain_db: f32,
    /// Když všechny zdroje selžou: vrátit se k automatice (true), jinak zkoušet znovu do konce převzetí.
    pub fallback_to_auto: bool,
}

impl Default for RelaySource {
    fn default() -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), name: "Studio 2".into(), typ: "url".into(), url: String::new(), device: String::new(), backup_ids: vec![],
            silence_db: -50.0, silence_timeout_s: 20, connect_timeout_s: 10, gain_db: 0.0, fallback_to_auto: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RelayConfig {
    pub sources: Vec<RelaySource>,
}

// ---------------------------------------------------------------------------------------------
// Metadata: RDS (FM), DAB+ (DLS / DL Plus, MOT Slideshow, SPI/EPG), stream, počasí

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MetaConfig {
    /// Krátký název (8 znaků – RDS PS, DAB short label)
    pub station_short: String,
    /// Střední název (16 znaků – DAB service label)
    pub station_medium: String,
    pub station_long: String,
    pub slogan: String,
    pub description: String,
    pub url: String,
    pub phone: String,
    pub email: String,
    pub sms: String,
    /// Logo stanice (PNG/JPG) – slideshow, EPG
    pub logo_path: String,
    /// Barva pozadí slidů (#rrggbb)
    pub brand_color: String,
    /// Výchozí žánr / PTY stanice (0–31, ČR: 10 = Pop music)
    pub pty: u8,
    /// Texty (klíčová slova: {ARTIST} {TITLE} {ALBUM} {YEAR} {COMPOSER} {SHOW} {SHOW_NEXT} {SHOW_TIME} {HOST}
    /// {NEXT} {NEXT_ARTIST} {NEXT_TITLE} {STATION} {SLOGAN} {URL} {PHONE} {SMS} {EMAIL}
    /// {WEATHER_PLACE} {WEATHER_TEMP} {WEATHER_TEXT} {WEATHER_MIN} {WEATHER_MAX} {WEATHER_WIND} {TIME} {DATE})
    pub tpl_song: String,
    pub tpl_show: String,
    pub tpl_next: String,
    pub tpl_weather: String,
    pub tpl_station: String,
    /// Text během reklam, jinglů a ostatních položek (prázdné = text stanice)
    pub tpl_other: String,
    /// Stream (Icecast/SHOUTcast StreamTitle)
    pub stream_song: String,
    pub stream_other: String,
    pub weather: WeatherConfig,
    pub rds: RdsConfig,
    pub dab: DabConfig,
    /// Informace na displeji: sady, playlisty sdělení a rozvrh
    pub info: crate::info::InfoConfig,
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            station_short: "ORCA".into(), station_medium: "ORCA RadioAIR".into(), station_long: "ORCA RadioAIR".into(), slogan: String::new(), description: String::new(),
            url: String::new(), phone: String::new(), email: String::new(), sms: String::new(), logo_path: String::new(), brand_color: "#0f172a".into(), pty: 10,
            tpl_song: "{ARTIST} - {TITLE}".into(),
            tpl_show: "Právě vysíláme: {SHOW} {HOST_WITH}".into(),
            tpl_next: "Dále uslyšíte: {NEXT}".into(),
            tpl_weather: "Počasí {WEATHER_PLACE}: {WEATHER_TEMP} °C, {WEATHER_TEXT}".into(),
            tpl_station: "{STATION} {SLOGAN}".into(),
            tpl_other: String::new(),
            stream_song: "{ARTIST} - {TITLE}".into(),
            stream_other: "{STATION} – {SHOW}".into(),
            weather: WeatherConfig::default(), rds: RdsConfig::default(), dab: DabConfig::default(), info: crate::info::InfoConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    pub enabled: bool,
    pub place: String,
    pub lat: f64,
    pub lon: f64,
    pub refresh_min: u32,
    /// Ruční text, když není internet / API (prázdné = nepoužít)
    pub manual_text: String,
}

impl Default for WeatherConfig {
    fn default() -> Self { Self { enabled: false, place: "Jihlava".into(), lat: 49.3961, lon: 15.5912, refresh_min: 15, manual_text: String::new() } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RdsConfig {
    pub enabled: bool,
    /// PI kód (hex, např. 2A01)
    pub pi: String,
    pub tp: bool,
    pub ta: bool,
    pub ms_music: bool,
    pub di_stereo: bool,
    /// static = jen název, song = interpret/název po 8 znacích (pozor: PS není určené pro text),
    /// rotate = střídání textů v ps_texts
    pub ps_mode: String,
    pub ps_texts: Vec<String>,
    pub ps_interval_s: u32,
    /// Šablony RadioTextu, střídají se (song, show, next, weather, station nebo vlastní text s klíčovými slovy)
    pub rt_rotation: Vec<String>,
    pub rt_interval_s: u32,
    pub rtplus: bool,
    /// Přepis do ASCII (bez diakritiky) – bezpečné pro všechny přijímače
    pub ascii: bool,
    /// PTY podle pořadu z programové mřížky
    pub pty_from_show: bool,
    pub outputs: Vec<RdsOutput>,
}

impl Default for RdsConfig {
    fn default() -> Self {
        Self { enabled: false, pi: "2000".into(), tp: false, ta: false, ms_music: true, di_stereo: true, ps_mode: "static".into(), ps_texts: vec!["{STATION}".into()], ps_interval_s: 4,
            rt_rotation: vec!["song".into(), "show".into(), "weather".into()], rt_interval_s: 15, rtplus: true, ascii: true, pty_from_show: true, outputs: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RdsOutput {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// uecp (EBU SPB 490) | ascii (textové příkazy – šablony)
    pub protocol: String,
    /// ascii: generic | pira | custom
    pub preset: String,
    /// tcp | udp | serial | file
    pub transport: String,
    pub host: String,
    pub port: u16,
    pub serial_port: String,
    pub baud: u32,
    pub file_path: String,
    pub uecp_site: u16,
    pub uecp_encoder: u8,
    /// Vlastní příkazy (ascii/custom): {PS} {RT} {PI} {PTY} {TP} {TA} {RTP_T1} {RTP_S1} {RTP_L1} {RTP_T2} {RTP_S2} {RTP_L2}
    pub cmd_ps: String,
    pub cmd_rt: String,
    pub cmd_pi: String,
    pub cmd_pty: String,
    pub cmd_rtplus: String,
    pub line_end: String,
}

impl Default for RdsOutput {
    fn default() -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), name: "RDS kodér".into(), enabled: true, protocol: "ascii".into(), preset: "generic".into(), transport: "tcp".into(),
            host: "192.168.1.50".into(), port: 1024, serial_port: String::new(), baud: 19200, file_path: String::new(), uecp_site: 0, uecp_encoder: 0,
            cmd_ps: "PS={PS}".into(), cmd_rt: "RT={RT}".into(), cmd_pi: "PI={PI}".into(), cmd_pty: "PTY={PTY}".into(), cmd_rtplus: "RT+TAG={RTP_T1},{RTP_S1},{RTP_L1},{RTP_T2},{RTP_S2},{RTP_L2},1,0".into(), line_end: "\\r\\n".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DabConfig {
    pub enabled: bool,
    /// Složka pro ODR-PadEnc a další DAB kodéry (prázdné = <data>/DAB)
    pub output_dir: PathBuf,
    /// Střídání DLS: song | show | next | weather | station | vlastní text s klíčovými slovy
    pub dls_rotation: Vec<String>,
    pub dls_interval_s: u32,
    pub dl_plus: bool,
    /// MOT Slideshow
    pub sls_enabled: bool,
    /// Které slidy generovat: cover, presenter, show, weather, station, next
    pub sls_slides: Vec<String>,
    pub sls_width: u32,
    pub sls_height: u32,
    /// Max. velikost slidu v kB (DAB doporučení 50 kB)
    pub sls_max_kb: u32,
    /// Jak dlouho držet jeden slide v current.jpg (s) – pro kodéry, které čtou jediný soubor
    pub sls_interval_s: u32,
    /// SPI / EPG (ETSI TS 102 818)
    pub epg_enabled: bool,
    pub ecc: String,
    pub eid: String,
    pub sid: String,
    pub scids: u8,
    /// FM frekvence pro bearer v SPI (MHz, prázdné = bez FM)
    pub fm_freq: String,
    /// Veřejná adresa serveru pro odkazy na obrázky v SPI (např. http://radio.example.cz:8750)
    pub public_url: String,
    pub epg_days: u32,
    /// HTTP push (komerční DAB headendy): URL šablona s {DLS} {DLS_URLENC}, metoda GET/POST
    pub http_push: Vec<String>,
}

impl Default for DabConfig {
    fn default() -> Self {
        Self { enabled: false, output_dir: PathBuf::new(), dls_rotation: vec!["song".into(), "show".into(), "next".into(), "weather".into()], dls_interval_s: 8, dl_plus: true,
            sls_enabled: true, sls_slides: vec!["cover".into(), "presenter".into(), "show".into(), "weather".into(), "station".into()], sls_width: 320, sls_height: 240, sls_max_kb: 50, sls_interval_s: 15,
            epg_enabled: true, ecc: "E2".into(), eid: "1001".into(), sid: "2001".into(), scids: 0, fm_freq: String::new(), public_url: String::new(), epg_days: 7, http_push: vec![] }
    }
}

// ---------------------------------------------------------------------------------------------
// Obecná nastavení (Jazler General Settings)

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct FolderMapping { pub from: String, pub to: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    pub enabled: bool,
    pub smtp_host: String,
    pub port: u16,
    /// starttls | tls | none
    pub security: String,
    pub username: String,
    pub password: String,
    pub from: String,
    pub to: Vec<String>,
    /// error | warning
    pub min_level: String,
    /// Stejné upozornění znovu nejdřív po (min)
    pub repeat_min: u32,
}
impl Default for EmailConfig {
    fn default() -> Self { Self { enabled: false, smtp_host: String::new(), port: 587, security: "starttls".into(), username: String::new(), password: String::new(), from: String::new(), to: vec![], min_level: "error".into(), repeat_min: 30 } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CustomLog {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub dir: PathBuf,
    /// {DATE} = YYYY-MM-DD, {YYYY} {MM} {DD}
    pub filename: String,
    /// {DATE} {TIME} {ARTIST} {TITLE} {ALBUM} {DURATION} {DURATION_S} {KIND} {CATEGORY} {CODE} {ISRC} {COMPOSER} {PUBLISHER} {FILENAME} {SOURCE}
    pub line: String,
    pub header: String,
    pub kinds: Vec<TrackKind>,
}
impl Default for CustomLog {
    fn default() -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), name: "Vlastní log".into(), enabled: false, dir: PathBuf::new(), filename: "log_{DATE}.txt".into(),
            line: "{DATE};{TIME};{ARTIST};{TITLE};{DURATION};{KIND};{CODE}".into(), header: String::new(), kinds: vec![TrackKind::Song, TrackKind::Spot] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SilenceConfig {
    pub enabled: bool,
    pub threshold_db: f32,
    pub seconds: u32,
    /// alert | next | playlist | relay
    pub action: String,
    pub playlist_id: Option<i64>,
    pub relay_id: Option<String>,
}
impl Default for SilenceConfig { fn default() -> Self { Self { enabled: true, threshold_db: -50.0, seconds: 15, action: "next".into(), playlist_id: None, relay_id: None } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig { pub enabled: bool, pub dir: PathBuf, pub keep: u32, pub hour: u32 }
impl Default for BackupConfig { fn default() -> Self { Self { enabled: true, dir: PathBuf::new(), keep: 14, hour: 4 } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Spustit ORCA po přihlášení do systému
    pub start_with_os: bool,
    /// Vyžadovat přihlášení uživatele (role a oprávnění)
    pub require_login: bool,
    /// Automatické odhlášení po nečinnosti (min, 0 = nikdy)
    pub auto_logout_min: u32,
    pub folder_mappings: Vec<FolderMapping>,
    pub email: EmailConfig,
    pub custom_logs: Vec<CustomLog>,
    pub silence: SilenceConfig,
    pub backup: BackupConfig,
    /// Mazat záznamy vysílání starší než (dní, 0 = nikdy)
    pub log_retention_days: u32,
    /// Externí audio editor (cesta k aplikaci)
    pub editor_path: String,
    /// Klávesové zkratky: akce → klávesa (např. "play" → "F1")
    pub hotkeys: BTreeMap<String, String>,
    /// Bránit uspání počítače a zvýšit prioritu vysílání
    pub keep_awake: bool,
    /// Zkratky fungují i když je ORCA v pozadí / minimalizovaná (jen desktopová aplikace)
    pub global_hotkeys_enabled: bool,
    /// Globální zkratky: akce → kombinace (musí obsahovat Ctrl/Alt/Cmd, např. "Ctrl+Alt+1")
    pub global_hotkeys: BTreeMap<String, String>,
}
/// Výchozí globální zkratky (Ctrl+Alt+…, aby nebraly klávesy jiným programům).
pub const DEFAULT_GLOBAL_HOTKEYS: [(&str, &str); 8] = [("play", "Ctrl+Alt+1"), ("next", "Ctrl+Alt+2"), ("pause", "Ctrl+Alt+3"), ("stop_after", "Ctrl+Alt+5"),
    ("mic", "Ctrl+Alt+8"), ("stop_instants", "Ctrl+Alt+Escape"), ("auto", "Ctrl+Alt+9"), ("manual", "Ctrl+Alt+0")];
/// Výchozí klávesy: F-klávesy + alternativa Alt(⌥)+číslo (na Macu jsou F1–F12 systémové klávesy).
pub const DEFAULT_HOTKEYS: [(&str, &str); 12] = [("play", "F1, Alt+1"), ("next", "F2, Alt+2"), ("pause", "F3, Alt+3"), ("stop", "F4, Alt+4"), ("stop_after", "F5, Alt+5"), ("loop", "F6, Alt+6"),
    ("restart", "F7, Alt+7"), ("mic", "F8, Alt+8"), ("stop_instants", "Escape"), ("auto", "F9, Alt+9"), ("manual", "F10, Alt+0"), ("continue", "F12, Alt+Enter")];

impl Default for GeneralConfig {
    fn default() -> Self {
        let hk = DEFAULT_HOTKEYS
            .into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        Self { start_with_os: false, require_login: false, auto_logout_min: 0, folder_mappings: vec![], email: EmailConfig::default(), custom_logs: vec![], silence: SilenceConfig::default(),
            backup: BackupConfig::default(), log_retention_days: 0, editor_path: String::new(), hotkeys: hk, keep_awake: true,
            global_hotkeys_enabled: false, global_hotkeys: DEFAULT_GLOBAL_HOTKEYS.into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect() }
    }
}
