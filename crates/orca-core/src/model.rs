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
}

impl TrackKind {
    pub const ALL: [TrackKind; 7] = [TrackKind::Song, TrackKind::Jingle, TrackKind::Spot, TrackKind::Sweeper, TrackKind::VoiceTrack, TrackKind::Asset, TrackKind::Instant];

    pub fn as_str(&self) -> &'static str {
        match self {
            TrackKind::Song => "song",
            TrackKind::Jingle => "jingle",
            TrackKind::Spot => "spot",
            TrackKind::Sweeper => "sweeper",
            TrackKind::VoiceTrack => "voicetrack",
            TrackKind::Asset => "asset",
            TrackKind::Instant => "instant",
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
}

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
}

impl QueueItem {
    /// Bude hrát přes předchozí položku (ne samostatně v řadě)?
    pub fn is_attached_overlay(&self) -> bool {
        self.track.kind().is_overlay_capable() && self.position.as_deref().map(|p| p != "independent").unwrap_or(false)
    }

    pub fn new(track: Track, source: &str, reason: impl Into<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), track, source: source.into(), reason: reason.into(), eta: None, position: None, stitch: None, break_ref: None, relay: None }
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
