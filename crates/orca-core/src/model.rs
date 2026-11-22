use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Song,
    Jingle,
    Spot,
    Sweeper,
    #[serde(rename = "voicetrack")]
    VoiceTrack,
    Asset,
    /// Instant jingly (palety) – samostatná knihovna, na pad lze ale dát jakýkoli soubor.
    Instant,
    /// Hudební podkres – hraje pod položkami playlistu od značky ON do značky OFF.
    Bed,
}

impl TrackKind {
    pub const ALL: [TrackKind; 8] = [TrackKind::Song, TrackKind::Jingle, TrackKind::Spot, TrackKind::Sweeper, TrackKind::VoiceTrack, TrackKind::Asset, TrackKind::Instant, TrackKind::Bed];

    pub fn as_str(&self) -> &'static str {
        match self {
            TrackKind::Song => "song",
            TrackKind::Jingle => "jingle",
            TrackKind::Spot => "spot",
            TrackKind::Sweeper => "sweeper",
            TrackKind::VoiceTrack => "voicetrack",
            TrackKind::Asset => "asset",
            TrackKind::Instant => "instant",
            TrackKind::Bed => "bed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "song" => TrackKind::Song,
            "jingle" => TrackKind::Jingle,
            "spot" => TrackKind::Spot,
            "sweeper" => TrackKind::Sweeper,
            "voicetrack" => TrackKind::VoiceTrack,
            "asset" => TrackKind::Asset,
            "instant" => TrackKind::Instant,
            "bed" => TrackKind::Bed,
            _ => return None,
        })
    }

    /// Hraje přes jinou stopu (sweeper, voice track) – nepřeruší hlavní stopu.
    pub fn is_overlay_capable(&self) -> bool { matches!(self, TrackKind::Sweeper | TrackKind::VoiceTrack) }

    /// Název podsložky v kořenové složce knihovny.
    pub fn folder_name(&self) -> &'static str {
        match self {
            TrackKind::Song => "Songs",
            TrackKind::Jingle => "Jingles",
            TrackKind::Spot => "Spots",
            TrackKind::Sweeper => "Sweepers",
            TrackKind::VoiceTrack => "Voice Tracks",
            TrackKind::Asset => "Assets",
            TrackKind::Instant => "Instant Jingles",
            TrackKind::Bed => "Beds",
        }
    }

    /// Výchozí prefix playlister kódu (Jazler: "library settings → prefix").
    pub fn default_prefix(&self) -> &'static str {
        match self {
            TrackKind::Song => "S",
            TrackKind::Jingle => "J",
            TrackKind::Spot => "C",
            TrackKind::Sweeper => "W",
            TrackKind::VoiceTrack => "V",
            TrackKind::Asset => "A",
            TrackKind::Instant => "I",
            TrackKind::Bed => "B",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Track {
    pub id: i64,
    pub kind: Option<TrackKind>,
    pub path: String,
    pub filename: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub year: Option<i32>,
    pub genre: String,
    pub category: String,
    pub category2: String,
    pub category3: String,
    pub duration_ms: i64,
    pub cue_in_ms: i64,
    pub intro_ms: i64,
    pub mix_ms: i64,
    pub cue_out_ms: i64,
    pub hook_in_ms: Option<i64>,
    pub hook_out_ms: Option<i64>,
    /// Ruční korekce hlasitosti (dB).
    pub gain_db: f64,
    pub lufs: Option<f64>,
    pub analyzed: bool,
    pub manual_cues: bool,
    pub enabled: bool,
    pub missing: bool,
    pub playlister_code: Option<String>,
    pub isrc: String,
    pub composer: String,
    pub publisher: String,
    /// Vocal (Jazler: muž / žena / duet / instrumentál …) – hodnoty z číselníku kategorií.
    pub vocal: String,
    /// Properties – volné štítky oddělené čárkou (tempo, nálada …).
    pub properties: String,
    /// Barva položky v seznamech (#rrggbb), prázdné = barva kategorie/typu.
    pub color: String,
    pub comment: String,
    /// Vlastní rozestup této skladby v minutách (přebíjí globální pravidlo).
    pub separation_min: Option<i64>,
    /// Vlastní obal (cesta k obrázku), jinak se bere z tagu.
    pub cover_path: Option<String>,
    pub bookmarked: bool,
    pub added_at: String,
    pub updated_at: String,
    pub last_played_at: Option<String>,
    pub play_count: i64,
    /// Podkres: jak dlouho hraje sám, než začne první položka (ms)
    #[serde(default = "d_bed_intro")]
    pub bed_intro_ms: i64,
    /// Podkres: jak dlouho dohrává po poslední položce před značkou OFF (ms)
    #[serde(default = "d_bed_outro")]
    pub bed_outro_ms: i64,
    /// Podkres: hlasitost pod položkami (dB vůči normalizované úrovni)
    #[serde(default = "d_bed_level")]
    pub bed_level_db: f64,
    /// Podkres: délka náběhu a doznění (ms)
    #[serde(default = "d_bed_fade")]
    pub bed_fade_ms: i64,
    /// Outro: začátek instrumentálního konce (ms od začátku souboru, 0 = nenastaveno)
    #[serde(default)]
    pub outro_ms: i64,
}
fn d_bed_intro() -> i64 { 3000 }
fn d_bed_outro() -> i64 { 3000 }
fn d_bed_level() -> f64 { -10.0 }
fn d_bed_fade() -> i64 { 1500 }

impl Track {
    pub fn kind(&self) -> TrackKind { self.kind.unwrap_or(TrackKind::Song) }

    pub fn display(&self) -> String {
        match (self.artist.is_empty(), self.title.is_empty()) {
            (false, false) => format!("{} - {}", self.artist, self.title),
            (true, false) => self.title.clone(),
            _ => self.filename.clone(),
        }
    }

    /// Efektivní body (ošetřené proti nesmyslným hodnotám).
    pub fn effective_cues(&self) -> (i64, i64, i64) {
        let dur = self.duration_ms.max(0);
        let cue_out = if self.cue_out_ms > 0 { self.cue_out_ms.min(dur.max(self.cue_out_ms)) } else { dur };
        let cue_in = self.cue_in_ms.clamp(0, cue_out.max(0));
        let mix = if self.mix_ms > 0 { self.mix_ms.clamp(cue_in, cue_out) } else { cue_out };
        (cue_in, mix, cue_out)
    }

    /// Celková hlasitostní korekce v dB.
    pub fn playback_gain_db(&self, normalize: bool, target_lufs: f64) -> f64 {
        let norm = if normalize { self.lufs.map(|l| (target_lufs - l).clamp(-12.0, 12.0)).unwrap_or(0.0) } else { 0.0 };
        norm + self.gain_db
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub track: Track,
    /// "auto", "manual", "playlist", "instant", "break", "stitcher"
    pub source: String,
    /// Proč byla položka vybrána (Jazler Log Viewer: "proč se to hrálo").
    pub reason: String,
    /// Odhadovaný čas začátku (ISO).
    pub eta: Option<String>,
    /// Sweeper / voice track přes předchozí položku: start | before_intro | before_end | bridge | independent | custom:<ms>
    #[serde(default)]
    pub position: Option<String>,
    /// Stitcher: hooks | time | news
    #[serde(default)]
    pub stitch: Option<String>,
    /// Reklamní blok, ke kterému položka patří ("YYYY-MM-DD HH:MM").
    #[serde(default)]
    pub break_ref: Option<String>,
    /// Rebroadcast: id zdroje (stream / vstup zvukové karty)
    #[serde(default)]
    pub relay: Option<String>,
    /// Hudební podkres: "on" (začátek, track = soubor podkresu) | "off" (konec)
    #[serde(default)]
    pub bed: Option<String>,
    /// Živý vstup: přehrávání se zastaví a čeká na moderátora (Pokračovat, klávesa, šavle na pultu – GPI)
    #[serde(default)]
    pub live: bool,
    /// Živý vstup: po plánované délce pokračovat automaticky (jinak čekat neomezeně)
    #[serde(default)]
    pub live_auto: bool,
}

impl QueueItem {
    /// Bude hrát přes předchozí položku (ne samostatně v řadě)?
    pub fn is_attached_overlay(&self) -> bool {
        self.track.kind().is_overlay_capable() && self.position.as_deref().map(|p| p != "independent").unwrap_or(false)
    }

    /// Doba, o kterou položka posune vysílání (pro odhad časů).
    pub fn slot_ms(&self) -> i64 {
        if self.live { return self.track.duration_ms.max(0); }
        match self.bed.as_deref() {
            Some("off") => 3000,
            Some(_) => self.track.bed_intro_ms.max(0),
            None if self.track.kind() == TrackKind::Bed => self.track.bed_intro_ms.max(0),
            None => { let (ci, mix, _) = self.track.effective_cues(); (mix - ci).max(0) }
        }
    }

    /// Živý vstup (moderátor, telefon, host) – playlist se přeruší a pokračuje na pokyn.
    pub fn live_break(label: &str, planned_ms: i64, auto_continue: bool, source: &str, reason: impl Into<String>) -> Self {
        let dur = planned_ms.max(0);
        let t = Track { kind: Some(TrackKind::Asset), title: if label.trim().is_empty() { "ŽIVÝ VSTUP".into() } else { label.trim().to_string() }, filename: "live".into(),
            duration_ms: dur, cue_out_ms: dur, mix_ms: dur, enabled: true, analyzed: true, ..Default::default() };
        let mut q = Self::new(t, source, reason);
        q.live = true;
        q.live_auto = auto_continue && dur > 0;
        q
    }

    /// Značka začátku podkresu (soubor z knihovny Podkresy).
    pub fn bed_on(track: Track, source: &str, reason: impl Into<String>) -> Self {
        let mut q = Self::new(track, source, reason);
        q.bed = Some("on".into());
        q
    }

    /// Značka konce podkresu (doznění podle nastavení právě hrajícího podkresu).
    pub fn bed_off(source: &str, reason: impl Into<String>) -> Self {
        let t = Track { kind: Some(TrackKind::Bed), title: "Podkres – konec (OFF)".into(), filename: "bed-off".into(), enabled: true, analyzed: true, ..Default::default() };
        let mut q = Self::new(t, source, reason);
        q.bed = Some("off".into());
        q
    }

    pub fn new(track: Track, source: &str, reason: impl Into<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), track, source: source.into(), reason: reason.into(), eta: None, position: None, stitch: None, break_ref: None, relay: None, bed: None, live: false, live_auto: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: i64,
    pub track_id: Option<i64>,
    pub kind: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub path: String,
    pub category: String,
    pub isrc: String,
    pub composer: String,
    pub publisher: String,
    pub duration_ms: i64,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub played_ms: Option<i64>,
    pub source: String,
    pub reason: String,
}
