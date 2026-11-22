//! Metadata hub: z aktuální skladby, programové mřížky, moderátorů a počasí skládá texty a obrázky pro
//! DAB+ (DLS s DL Plus, MOT Slideshow, SPI/EPG), RDS (PS, RT, RT+, PTY), metadata streamů a JSON pro web.

use crate::config::Config;
use crate::db::Db;
use crate::encoder::EncoderManager;
use crate::engine::Engine;
use crate::model::{Track, TrackKind};
use crate::rds::{RdsData, RdsSender};
use crate::sched::{now_next, Presenter, ProgramSlot};
use crate::weather::Weather;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Typy DL Plus / RT+ (ETSI TS 102 980, IEC 62106)
pub mod tag {
    pub const TITLE: u8 = 1;
    pub const ALBUM: u8 = 2;
    pub const ARTIST: u8 = 4;
    pub const COMPOSER: u8 = 8;
    pub const NEWS: u8 = 12;
    pub const WEATHER: u8 = 25;
    pub const ADVERTISEMENT: u8 = 28;
    pub const URL: u8 = 29;
    pub const STATION_SHORT: u8 = 31;
    pub const STATION_LONG: u8 = 32;
    pub const PROGRAMME_NOW: u8 = 33;
    pub const PROGRAMME_NEXT: u8 = 34;
    pub const HOST: u8 = 36;
    pub const PHONE_STUDIO: u8 = 42;
    pub const SMS_STUDIO: u8 = 44;
    pub const EMAIL_STUDIO: u8 = 47;
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Tagged {
    pub text: String,
    /// (typ, začátek ve znacích, délka ve znacích)
    pub tags: Vec<(u8, usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Ctx {
    pub vars: HashMap<String, String>,
    pub kind: Option<TrackKind>,
    pub is_song: bool,
    pub track: Option<Track>,
    pub next_track: Option<Track>,
    pub show: Option<ProgramSlot>,
    pub next_show: Option<ProgramSlot>,
    pub weather: Option<Weather>,
}

fn tag_for(var: &str) -> Option<u8> {
    Some(match var {
        "ARTIST" => tag::ARTIST, "TITLE" => tag::TITLE, "ALBUM" => tag::ALBUM, "COMPOSER" => tag::COMPOSER,
        "SHOW" => tag::PROGRAMME_NOW, "SHOW_NEXT" => tag::PROGRAMME_NEXT, "HOST" => tag::HOST,
        "STATION" => tag::STATION_LONG, "STATION_SHORT" => tag::STATION_SHORT, "URL" => tag::URL,
        "PHONE" => tag::PHONE_STUDIO, "SMS" => tag::SMS_STUDIO, "EMAIL" => tag::EMAIL_STUDIO,
        _ => return None,
    })
}

/// Vyplní šablonu klíčovými slovy a spočítá pozice DL Plus / RT+ tagů.
pub fn render(tpl: &str, ctx: &Ctx) -> Tagged {
    let mut text = String::new();
    let mut tags = vec![];
    let mut rest = tpl;
    while let Some(i) = rest.find('{') {
        text.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('}') {
            Some(j) => {
                let key = &after[..j];
                let val = ctx.vars.get(key).cloned().unwrap_or_default();
                if let (Some(t), false) = (tag_for(key), val.is_empty()) { tags.push((t, text.chars().count(), val.chars().count())); }
                text.push_str(&val);
                rest = &after[j + 1..];
            }
            None => { text.push('{'); rest = after; }
        }
    }
    text.push_str(rest);
    // úklid mezer a osiřelých oddělovačů – posuny tagů přepočítáme
    let (clean, map) = squeeze(&text);
    let tags = tags.into_iter().filter_map(|(t, s, l)| {
        let ns = *map.get(s)?;
        let ne = *map.get(s + l - 1)?;
        Some((t, ns, ne - ns + 1))
    }).collect();
    Tagged { text: clean, tags }
}

/// Odstraní vícenásobné mezery a mezery na okrajích; vrací mapu staré pozice → nové.
fn squeeze(s: &str) -> (String, Vec<usize>) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut map = vec![0usize; chars.len() + 1];
    let mut n = 0usize;
    let mut prev_space = true;
    for (i, c) in chars.iter().enumerate() {
        map[i] = n;
        if c.is_whitespace() { if prev_space { continue; } prev_space = true; out.push(' '); n += 1; } else { prev_space = false; out.push(*c); n += 1; }
    }
    while out.ends_with(' ') { out.pop(); }
    let trimmed = out.trim_end_matches([' ', '-', '–', ':', ',', '·']).to_string();
    (trimmed, map)
}

/// Oříznutí na max. počet znaků (DLS 128, RT 64) – tagy mimo text se zahodí.
pub fn limit(t: &Tagged, n: usize, max_tags: usize) -> Tagged {
    let text: String = t.text.chars().take(n).collect();
    let len = text.chars().count();
    let tags = t.tags.iter().filter(|(_, s, l)| *l > 0 && s + l <= len).take(max_tags).copied().collect();
    Tagged { text, tags }
}

pub fn build_ctx(cfg: &Config, track: Option<&Track>, next: Option<&Track>, show: Option<&ProgramSlot>, next_show: Option<&ProgramSlot>, weather: Option<&Weather>) -> Ctx {
    let en = cfg.station.language == "en";
    let m = &cfg.meta;
    let mut v: HashMap<String, String> = HashMap::new();
    let mut put = |k: &str, val: String| { v.insert(k.to_string(), val); };
    put("STATION", m.station_long.clone());
    put("STATION_SHORT", m.station_short.clone());
    put("SLOGAN", m.slogan.clone());
    put("URL", m.url.clone());
    put("PHONE", m.phone.clone());
    put("SMS", m.sms.clone());
    put("EMAIL", m.email.clone());
    let now = crate::automation::now_local(cfg);
    put("TIME", now.format("%H:%M").to_string());
    put("DATE", now.format("%-d. %-m. %Y").to_string());
    if let Some(t) = track {
        put("ARTIST", t.artist.clone());
        put("TITLE", if t.title.is_empty() { t.filename.clone() } else { t.title.clone() });
        put("ALBUM", t.album.clone());
        put("YEAR", t.year.map(|y| y.to_string()).unwrap_or_default());
        put("COMPOSER", t.composer.clone());
        put("CATEGORY", t.category.clone());
    }
    if let Some(n) = next {
        put("NEXT", n.display());
        put("NEXT_ARTIST", n.artist.clone());
        put("NEXT_TITLE", n.title.clone());
    }
    if let Some(s) = show {
        put("SHOW", s.show.name.clone());
        put("SHOW_TIME", format!("{}–{}", s.start.format("%H:%M"), s.end.format("%H:%M")));
        let hosts = s.presenters.iter().map(|p| p.name.clone()).collect::<Vec<_>>().join(if en { " and " } else { " a " });
        put("HOST_WITH", if hosts.is_empty() { String::new() } else if en { format!("with {hosts}") } else { format!("· moderuje {hosts}") });
        put("HOST", hosts);
        put("SHOW_DESC", s.description.clone());
    }
    if let Some(s) = next_show {
        put("SHOW_NEXT", s.show.name.clone());
        put("SHOW_NEXT_TIME", s.start.format("%H:%M").to_string());
    }
    if let Some(w) = weather {
        put("WEATHER_PLACE", w.place.clone());
        put("WEATHER_TEMP", crate::weather::fmt_temp(w.temp));
        put("WEATHER_TEXT", w.text.clone());
        put("WEATHER_MIN", w.min.map(crate::weather::fmt_temp).unwrap_or_default());
        put("WEATHER_MAX", w.max.map(crate::weather::fmt_temp).unwrap_or_default());
        put("WEATHER_WIND", w.wind_kmh.map(|x| format!("{} km/h", x.round() as i64)).unwrap_or_default());
    }
    let kind = track.map(|t| t.kind());
    Ctx { vars: v, kind, is_song: kind == Some(TrackKind::Song), track: track.cloned(), next_track: next.cloned(), show: show.cloned(), next_show: next_show.cloned(), weather: weather.cloned() }
}

/// Jedna zpráva rotace (song | show | next | weather | station | vlastní šablona). None = přeskočit.
pub fn message(cfg: &Config, ctx: &Ctx, item: &str) -> Option<Tagged> {
    let m = &cfg.meta;
    let t = match item {
        "song" => {
            if ctx.is_song { render(&m.tpl_song, ctx) }
            else {
                let tpl = if m.tpl_other.is_empty() { &m.tpl_station } else { &m.tpl_other };
                let mut r = render(tpl, ctx);
                if ctx.kind == Some(TrackKind::Spot) && !r.text.is_empty() { r.tags = vec![(tag::ADVERTISEMENT, 0, r.text.chars().count())]; }
                r
            }
        }
        "show" => { ctx.show.as_ref()?; render(&m.tpl_show, ctx) }
        "next" => { let n = ctx.next_track.as_ref()?; if n.kind() != TrackKind::Song { return None; } render(&m.tpl_next, ctx) }
        "weather" => {
            let w = ctx.weather.as_ref()?;
            let mut r = if w.manual { render(if cfg.station.language == "en" { "Weather {WEATHER_PLACE}: {WEATHER_TEXT}" } else { "Počasí {WEATHER_PLACE}: {WEATHER_TEXT}" }, ctx) } else { render(&m.tpl_weather, ctx) };
            let n = r.text.chars().count();
            r.tags = vec![(tag::WEATHER, 0, n)];
            r
        }
        "station" => render(&m.tpl_station, ctx),
        other => render(other, ctx),
    };
    if t.text.trim().is_empty() { None } else { Some(t) }
}

/// Text pro metadata streamu (Icecast StreamTitle).
pub fn stream_text(cfg: &Config, ctx: &Ctx) -> Option<String> {
    let t = ctx.track.as_ref()?;
    let rule = cfg.nowplaying.per_kind.get(&t.kind()).cloned().unwrap_or_default();
    if rule == crate::nowplaying::NO_UPDATE { return None; }
    let tpl = if !rule.is_empty() { rule } else if ctx.is_song { cfg.meta.stream_song.clone() } else { cfg.meta.stream_other.clone() };
    let s = render(&tpl, ctx).text;
    if s.is_empty() { Some(t.display()) } else { Some(s) }
}

// ---------------------------------------------------------------------------------------------
// DLS soubor pro ODR-PadEnc (DL Plus parametry)

pub fn dls_file(t: &Tagged, toggle: bool, running: bool, dl_plus: bool) -> String {
    let mut s = String::new();
    if dl_plus && !t.tags.is_empty() {
        s.push_str("##### parameters { #####\nDL_PLUS=1\n");
        s.push_str(&format!("DL_PLUS_ITEM_TOGGLE={}\nDL_PLUS_ITEM_RUNNING={}\n", toggle as u8, running as u8));
        for (ty, st, len) in &t.tags { s.push_str(&format!("DL_PLUS_TAG={ty} {st} {}\n", len.saturating_sub(1))); }
        s.push_str("##### parameters } #####\n");
    }
    s.push_str(&t.text);
    s.push('\n');
    s
}

fn write_atomic(p: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = p.parent() { std::fs::create_dir_all(d)?; }
    let tmp = p.with_extension(format!("{}.tmp", p.extension().and_then(|x| x.to_str()).unwrap_or("x")));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, p)
}

// ---------------------------------------------------------------------------------------------
// Stav pro UI

#[derive(Debug, Clone, Serialize, Default)]
pub struct SlideInfo { pub name: String, pub kind: String, pub bytes: usize, pub updated: String }

#[derive(Debug, Clone, Serialize, Default)]
pub struct MetaState {
    pub dls: Tagged,
    pub dls_index: usize,
    pub rds: RdsData,
    pub stream: String,
    pub slides: Vec<SlideInfo>,
    pub current_slide: Option<String>,
    pub show: Option<ProgramSlot>,
    pub next_show: Option<ProgramSlot>,
    pub weather: Option<Weather>,
    pub weather_error: Option<String>,
    pub dab_dir: String,
    pub override_text: Option<String>,
    pub override_until: Option<String>,
    pub epg_written: Option<String>,
    pub epg_error: Option<String>,
    pub updated: String,
    pub rds_outputs: Vec<crate::rds::OutputStatus>,
}

pub struct MetaHub {
    db: Arc<Db>,
    cfg: Arc<RwLock<Config>>,
    engine: Arc<Engine>,
    encoders: Arc<EncoderManager>,
    data_dir: PathBuf,
    pub state: RwLock<MetaState>,
    pub rds: RdsSender,
    weather: RwLock<Option<Weather>>,
    override_text: Mutex<Option<(String, Instant)>>,
    force: Mutex<bool>,
    epg_force: Mutex<bool>,
}

struct Loop {
    last_item: Option<String>,
    dls_i: usize,
    dls_at: Option<Instant>,
    rt_i: usize,
    rt_at: Option<Instant>,
    rt_text: String,
    rt_ab: bool,
    ps_i: usize,
    ps_at: Option<Instant>,
    ps_list: Vec<String>,
    toggle: bool,
    show_at: Option<Instant>,
    show: (Option<ProgramSlot>, Option<ProgramSlot>),
    slide_sig: String,
    slide_i: usize,
    slide_at: Option<Instant>,
    epg_at: Option<Instant>,
    last_dls: String,
}

pub fn dab_dir(cfg: &Config, data_dir: &Path) -> PathBuf {
    if cfg.meta.dab.output_dir.as_os_str().is_empty() { data_dir.join("DAB") } else { cfg.meta.dab.output_dir.clone() }
}

impl MetaHub {
    pub fn start(db: Arc<Db>, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, encoders: Arc<EncoderManager>, data_dir: PathBuf) -> Arc<Self> {
        let h = Arc::new(Self { db, cfg, engine, encoders, data_dir, state: RwLock::new(MetaState::default()), rds: RdsSender::new(),
            weather: RwLock::new(None), override_text: Mutex::new(None), force: Mutex::new(true), epg_force: Mutex::new(true) });
        { let h = h.clone(); std::thread::Builder::new().name("orca-meta".into()).spawn(move || h.run()).ok(); }
        { let h = h.clone(); std::thread::Builder::new().name("orca-weather".into()).spawn(move || h.weather_loop()).ok(); }
        h
    }

    pub fn refresh(&self) { *self.force.lock() = true; *self.epg_force.lock() = true; self.rds.reset(); }

    pub fn set_override(&self, text: &str, minutes: u32) {
        *self.override_text.lock() = if text.trim().is_empty() { None } else { Some((text.trim().to_string(), Instant::now() + Duration::from_secs(minutes.max(1) as u64 * 60))) };
        *self.force.lock() = true;
    }

    pub fn refresh_weather(&self) -> anyhow::Result<Weather> {
        let cfg = self.cfg.read().clone();
        let w = if !cfg.meta.weather.manual_text.trim().is_empty() {
            Weather { place: cfg.meta.weather.place.clone(), text: cfg.meta.weather.manual_text.clone(), manual: true, updated: chrono::Local::now().to_rfc3339(), ..self.weather.read().clone().unwrap_or_default() }
        } else {
            crate::weather::fetch(&cfg.meta.weather, cfg.station.language == "en")?
        };
        *self.weather.write() = Some(w.clone());
        self.state.write().weather_error = None;
        *self.force.lock() = true;
        Ok(w)
    }

    fn weather_loop(self: Arc<Self>) {
        let mut last: Option<Instant> = None;
        loop {
            let cfg = self.cfg.read().clone();
            let every = Duration::from_secs(cfg.meta.weather.refresh_min.max(5) as u64 * 60);
            if cfg.meta.weather.enabled && last.map(|t| t.elapsed() > every).unwrap_or(true) {
                last = Some(Instant::now());
                if let Err(e) = self.refresh_weather() { self.state.write().weather_error = Some(e.to_string()); log::warn!("počasí: {e}"); }
            }
            if !cfg.meta.weather.enabled && self.weather.read().is_some() { *self.weather.write() = None; }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    /// Aktuální kontext (pro náhled v UI).
    pub fn context(&self) -> Ctx {
        let cfg = self.cfg.read().clone();
        let st = self.state.read();
        let cur = self.engine.snapshot().current.map(|c| c.item.track);
        let next = self.engine.queue_snapshot().into_iter().find(|i| i.track.kind() == TrackKind::Song).map(|i| i.track);
        build_ctx(&cfg, cur.as_ref(), next.as_ref(), st.show.as_ref(), st.next_show.as_ref(), self.weather.read().as_ref())
    }

    fn run(self: Arc<Self>) {
        let mut l = Loop { last_item: None, dls_i: 0, dls_at: None, rt_i: 0, rt_at: None, rt_text: String::new(), rt_ab: false, ps_i: 0, ps_at: None, ps_list: vec![],
            toggle: false, show_at: None, show: (None, None), slide_sig: String::new(), slide_i: 0, slide_at: None, epg_at: None, last_dls: String::new() };
        loop {
            if let Err(e) = self.tick(&mut l) { log::error!("metadata: {e}"); }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn tick(&self, l: &mut Loop) -> anyhow::Result<()> {
        let cfg = self.cfg.read().clone();
        let force = std::mem::take(&mut *self.force.lock());
        let now = crate::automation::now_local(&cfg);
        if force || l.show_at.map(|t| t.elapsed() > Duration::from_secs(20)).unwrap_or(true) {
            l.show = now_next(&self.db, now).unwrap_or((None, None));
            l.show_at = Some(Instant::now());
        }
        let snap = self.engine.snapshot();
        let cur = snap.current.as_ref().map(|c| c.item.clone());
        let next = self.engine.queue_snapshot().into_iter().find(|i| i.track.kind() == TrackKind::Song).map(|i| i.track);
        let weather = self.weather.read().clone();
        let ctx = build_ctx(&cfg, cur.as_ref().map(|c| &c.track), next.as_ref(), l.show.0.as_ref(), l.show.1.as_ref(), weather.as_ref());
        let changed = cur.as_ref().map(|c| c.id.clone()) != l.last_item;
        if changed {
            l.last_item = cur.as_ref().map(|c| c.id.clone());
            l.dls_i = 0; l.rt_i = 0; l.dls_at = None; l.rt_at = None;
            if ctx.is_song { l.toggle = !l.toggle; }
            // metadata streamu
            if cur.is_some() {
                if let Some(text) = stream_text(&cfg, &ctx) { self.encoders.update_metadata(&text); self.state.write().stream = text; }
            }
        }
        let ovr = { let mut o = self.override_text.lock(); if o.as_ref().map(|(_, until)| Instant::now() > *until).unwrap_or(false) { *o = None; } o.clone() };
        let mut ovr_t = ovr.as_ref().map(|(t, _)| Tagged { text: t.clone(), tags: vec![] });
        // reklamní spot s vlastním textem pro DAB+/RDS (po dobu spotu)
        let spot_meta = cur.as_ref().filter(|c| c.track.id > 0).and_then(|c| crate::ads::spot_meta(&self.db, c.track.id).ok().flatten());
        if ovr_t.is_none() {
            if let Some(m) = spot_meta.as_ref().filter(|m| !m.dls_text.trim().is_empty()) {
                let text = render(&m.dls_text, &ctx).text;
                ovr_t = Some(Tagged { tags: vec![(tag::ADVERTISEMENT, 0, text.chars().count())], text });
            }
        }

        // ---- DAB DLS
        let dir = dab_dir(&cfg, &self.data_dir);
        let dab = &cfg.meta.dab;
        let rot: Vec<Tagged> = dab.dls_rotation.iter().filter_map(|i| message(&cfg, &ctx, i)).collect();
        let due = changed || force || l.dls_at.map(|t| t.elapsed() >= Duration::from_secs(dab.dls_interval_s.max(2) as u64)).unwrap_or(true);
        if due {
            let msg = match &ovr_t { Some(t) => t.clone(), None => { if !rot.is_empty() { l.dls_i %= rot.len(); } let m = rot.get(l.dls_i).cloned().unwrap_or_else(|| render(&cfg.meta.tpl_station, &ctx)); l.dls_i += 1; m } };
            let msg = limit(&msg, 128, 4);
            l.dls_at = Some(Instant::now());
            if dab.enabled {
                let running = ctx.is_song;
                let file = dls_file(&msg, l.toggle, running, dab.dl_plus);
                if file != l.last_dls || force {
                    write_atomic(&dir.join("dls.txt"), file.as_bytes())?;
                    write_atomic(&dir.join("dls_plain.txt"), format!("{}\n", msg.text).as_bytes())?;
                    for u in dab.http_push.iter().filter(|u| !u.trim().is_empty()) {
                        let url = u.replace("{DLS_URLENC}", &urlencoding::encode(&msg.text)).replace("{DLS}", &msg.text);
                        let r = if url.starts_with("POST ") { ureq::post(url[5..].trim()).timeout(Duration::from_secs(3)).send_string(&msg.text).map(|_| ()) } else { ureq::get(url.trim_start_matches("GET ").trim()).timeout(Duration::from_secs(3)).call().map(|_| ()) };
                        if let Err(e) = r { log::warn!("DLS push {url}: {e}"); }
                    }
                    l.last_dls = file;
                }
            }
            let mut st = self.state.write();
            st.dls = msg; st.dls_index = l.dls_i;
        }

        // ---- MOT Slideshow
        if dab.enabled && dab.sls_enabled {
            let sig = format!("{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}", spot_meta.as_ref().map(|m| (&m.image_path, &m.dls_text)), cur.as_ref().map(|c| (&c.track.id, &c.track.title)), l.show.0.as_ref().map(|s| (s.grid_id, s.start)),
                weather.as_ref().map(|w| (&w.updated, &w.text)), next.as_ref().map(|t| t.id), cfg.meta.logo_path, serde_json::to_string(&dab.sls_slides).unwrap_or_default());
            if sig != l.slide_sig || force {
                l.slide_sig = sig;
                match self.render_slides(&cfg, &ctx, &dir, spot_meta.as_ref()) { Ok(v) => self.state.write().slides = v, Err(e) => log::warn!("slideshow: {e}") }
            }
            let slides = self.state.read().slides.clone();
            if !slides.is_empty() && (changed || l.slide_at.map(|t| t.elapsed() >= Duration::from_secs(dab.sls_interval_s.max(5) as u64)).unwrap_or(true)) {
                // po změně skladby nejdřív obal
                if changed { l.slide_i = 0; }
                l.slide_i %= slides.len();
                let s = &slides[l.slide_i];
                if let Ok(b) = std::fs::read(dir.join("slides").join(&s.name)) { let _ = write_atomic(&dir.join("current.jpg"), &b); }
                self.state.write().current_slide = Some(s.name.clone());
                l.slide_i += 1;
                l.slide_at = Some(Instant::now());
            }
        }

        // ---- SPI / EPG
        if dab.enabled && dab.epg_enabled {
            let ef = std::mem::take(&mut *self.epg_force.lock());
            if ef || l.epg_at.map(|t| t.elapsed() > Duration::from_secs(600)).unwrap_or(true) {
                l.epg_at = Some(Instant::now());
                match crate::epg::write_files(&cfg, &self.db, &dir) {
                    Ok(_) => { let mut st = self.state.write(); st.epg_written = Some(chrono::Local::now().to_rfc3339()); st.epg_error = None; }
                    Err(e) => self.state.write().epg_error = Some(e.to_string()),
                }
            }
        }

        // ---- RDS
        let rc = &cfg.meta.rds;
        let ascii = |s: &str| if rc.ascii { crate::rds::ascii(s) } else { s.to_string() };
        // RadioText
        let rt_rot: Vec<Tagged> = rc.rt_rotation.iter().filter_map(|i| message(&cfg, &ctx, i)).collect();
        if changed || force || l.rt_at.map(|t| t.elapsed() >= Duration::from_secs(rc.rt_interval_s.max(4) as u64)).unwrap_or(true) {
            let msg = match &ovr_t { Some(t) => t.clone(), None => { if !rt_rot.is_empty() { l.rt_i %= rt_rot.len(); } let m = rt_rot.get(l.rt_i).cloned().unwrap_or_default(); l.rt_i += 1; m } };
            let msg = limit(&Tagged { text: ascii(&msg.text), tags: msg.tags }, 64, 2);
            if msg.text != l.rt_text { l.rt_ab = !l.rt_ab; l.rt_text = msg.text.clone(); }
            l.rt_at = Some(Instant::now());
            let mut st = self.state.write();
            st.rds.rt = msg.text;
            st.rds.rtplus = if rc.rtplus { msg.tags } else { vec![] };
            st.rds.rt_ab = l.rt_ab;
        }
        // PS
        let ps_due = l.ps_at.map(|t| t.elapsed() >= Duration::from_secs(rc.ps_interval_s.max(2) as u64)).unwrap_or(true);
        if changed || force || l.ps_list.is_empty() {
            l.ps_list = match rc.ps_mode.as_str() {
                "song" => { let mut v = vec![crate::rds::ps8(&ascii(&cfg.meta.station_short))]; if ctx.is_song { v.extend(crate::rds::ps_chunks(&ascii(&render(&cfg.meta.tpl_song, &ctx).text))); } v }
                "rotate" => rc.ps_texts.iter().map(|t| crate::rds::ps8(&ascii(&render(t, &ctx).text))).filter(|s| !s.trim().is_empty()).collect(),
                _ => vec![crate::rds::ps8(&ascii(&cfg.meta.station_short))],
            };
            if l.ps_list.is_empty() { l.ps_list.push(crate::rds::ps8(&ascii(&cfg.meta.station_short))); }
            l.ps_i = 0;
        }
        if ps_due || changed || force {
            l.ps_i %= l.ps_list.len();
            let ps = l.ps_list[l.ps_i].clone();
            l.ps_i += 1;
            l.ps_at = Some(Instant::now());
            self.state.write().rds.ps = ps;
        }
        {
            let pty = if rc.pty_from_show { l.show.0.as_ref().and_then(|s| s.show.pty).unwrap_or(cfg.meta.pty) } else { cfg.meta.pty };
            let mut st = self.state.write();
            st.rds.pi = rc.pi.clone(); st.rds.pty = pty; st.rds.tp = rc.tp; st.rds.ta = rc.ta; st.rds.ms_music = rc.ms_music; st.rds.di_stereo = rc.di_stereo;
        }
        if rc.enabled {
            let d = self.state.read().rds.clone();
            self.rds.send_all(rc, &d, force);
        }

        let mut st = self.state.write();
        st.show = l.show.0.clone(); st.next_show = l.show.1.clone(); st.weather = weather; st.dab_dir = dir.to_string_lossy().to_string();
        st.override_text = ovr.as_ref().map(|(t, _)| t.clone());
        st.override_until = ovr.as_ref().map(|(_, u)| (chrono::Local::now() + chrono::Duration::from_std(u.saturating_duration_since(Instant::now())).unwrap_or_default()).to_rfc3339());
        st.updated = chrono::Local::now().to_rfc3339();
        st.rds_outputs = self.rds.statuses(rc);
        drop(st);
        // JSON pro web / aplikace
        if changed || force { let _ = write_atomic(&dir.join("now.json"), serde_json::to_string_pretty(&self.now_json(&ctx))?.as_bytes()); }
        Ok(())
    }

    /// Kompletní metadata pro web, aplikace a externí systémy.
    pub fn now_json(&self, ctx: &Ctx) -> serde_json::Value {
        let cfg = self.cfg.read().clone();
        let st = self.state.read();
        let pres = |p: &Presenter| serde_json::json!({ "id": p.id, "name": p.name, "role": p.role, "photo": if p.photo_path.is_empty() { None } else { Some(format!("/meta/image/presenter/{}", p.id.unwrap_or(0))) } });
        let slot = |s: &ProgramSlot| serde_json::json!({ "id": s.show.id, "name": s.show.name, "description": s.description, "start": s.start.to_rfc3339(), "end": s.end.to_rfc3339(),
            "image": if s.show.image_path.is_empty() { None } else { Some(format!("/meta/image/show/{}", s.show.id.unwrap_or(0))) }, "presenters": s.presenters.iter().map(pres).collect::<Vec<_>>() });
        serde_json::json!({
            "station": { "name": cfg.meta.station_long, "short": cfg.meta.station_short, "slogan": cfg.meta.slogan, "url": cfg.meta.url, "phone": cfg.meta.phone, "email": cfg.meta.email },
            "now": ctx.track.as_ref().map(|t| serde_json::json!({ "type": t.kind().as_str(), "artist": t.artist, "title": t.title, "album": t.album, "year": t.year, "duration_ms": t.duration_ms, "isrc": t.isrc, "composer": t.composer, "cover": format!("/meta/cover/{}", t.id) })),
            "next": ctx.next_track.as_ref().map(|t| serde_json::json!({ "artist": t.artist, "title": t.title })),
            "show": ctx.show.as_ref().map(slot),
            "show_next": ctx.next_show.as_ref().map(slot),
            "weather": ctx.weather,
            "dls": st.dls.text, "rds": { "ps": st.rds.ps, "rt": st.rds.rt, "pty": st.rds.pty },
            "updated": chrono::Local::now().to_rfc3339(),
        })
    }

    fn render_slides(&self, cfg: &Config, ctx: &Ctx, dir: &Path, spot: Option<&crate::ads::SpotMeta>) -> anyhow::Result<Vec<SlideInfo>> {
        use crate::slides as sl;
        let dab = &cfg.meta.dab;
        let sd = dir.join("slides");
        std::fs::create_dir_all(&sd)?;
        let en = cfg.station.language == "en";
        let logo = sl::load_path(&cfg.meta.logo_path);
        let footer = match &ctx.show { Some(s) => match ctx.vars.get("HOST").filter(|h| !h.is_empty()) { Some(h) => format!("{} · {}", s.show.name, h), None => s.show.name.clone() }, None => cfg.meta.station_long.clone() };
        let c = sl::Common { w: dab.sls_width.clamp(160, 800), h: dab.sls_height.clamp(120, 600), brand: sl::parse_color(&cfg.meta.brand_color), logo: logo.as_ref(), station: &cfg.meta.station_long, footer: &footer };
        let mut out: Vec<(String, String, image::RgbImage)> = vec![];
        // reklamní spot s obrázkem: po dobu spotu jen jeho slide
        let spot_img = spot.and_then(|m| sl::load_path(&m.image_path));
        if let Some(im) = &spot_img { out.push(("00_spot.jpg".into(), "spot".into(), sl::full(&c, im))); }
        for (i, kind) in dab.sls_slides.iter().enumerate().filter(|_| spot_img.is_none()) {
            let name = format!("{:02}_{kind}.jpg", i + 1);
            let img = match kind.as_str() {
                "cover" => {
                    let Some(t) = &ctx.track else { continue };
                    if t.kind() != TrackKind::Song { continue; }
                    let cover = crate::library::cover_for(t).and_then(|(b, _)| sl::load_image(&b));
                    Some(sl::now_playing(&c, cover.as_ref(), if en { "Now playing" } else { "Právě hraje" }, &t.artist, if t.title.is_empty() { &t.filename } else { &t.title }))
                }
                "next" => {
                    let Some(t) = &ctx.next_track else { continue };
                    let cover = crate::library::cover_for(t).and_then(|(b, _)| sl::load_image(&b));
                    Some(sl::now_playing(&c, cover.as_ref(), if en { "Coming up" } else { "Dále uslyšíte" }, &t.artist, &t.title))
                }
                "presenter" => {
                    let Some(s) = &ctx.show else { continue };
                    let Some(p) = s.presenters.first() else { continue };
                    let photo = sl::load_path(&p.photo_path);
                    let detail = if p.role.is_empty() { s.show.name.clone() } else { format!("{} · {}", p.role, s.show.name) };
                    Some(sl::person(&c, photo.as_ref(), if en { "On air" } else { "Ve vysílání" }, &p.name, &detail))
                }
                "show" => {
                    let Some(s) = &ctx.show else { continue };
                    let im = sl::load_path(&s.show.image_path);
                    Some(sl::show(&c, im.as_ref(), if en { "On air now" } else { "Právě vysíláme" }, &s.show.name, &format!("{}–{}", s.start.format("%H:%M"), s.end.format("%H:%M")), &s.description))
                }
                "weather" => {
                    let Some(w) = &ctx.weather else { continue };
                    let mm = match (w.min, w.max) { (Some(a), Some(b)) => format!("min {} / max {} °C", crate::weather::fmt_temp(a), crate::weather::fmt_temp(b)), _ => String::new() };
                    let temp = if w.manual { String::new() } else { crate::weather::fmt_temp(w.temp) };
                    Some(sl::weather(&c, &w.place, &temp, &w.text, &mm, if en { "Weather" } else { "Počasí" }))
                }
                "station" => Some(sl::station(&c, &cfg.meta.slogan, &cfg.meta.url)),
                _ => None,
            };
            if let Some(img) = img { out.push((name, kind.clone(), img)); }
        }
        // odstranit staré slidy, zapsat nové (ODR-PadEnc prochází složku dokola)
        let keep: Vec<String> = out.iter().map(|(n, _, _)| n.clone()).collect();
        for e in std::fs::read_dir(&sd)?.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let base = n.trim_end_matches(".sls_params").to_string();
            if !keep.contains(&base) { let _ = std::fs::remove_file(e.path()); }
        }
        let mut info = vec![];
        let now = chrono::Local::now().to_rfc3339();
        for (n, kind, img) in out {
            let bytes = sl::encode_jpeg(&img, dab.sls_max_kb)?;
            write_atomic(&sd.join(&n), &bytes)?;
            let mut params = String::new();
            let url = spot.filter(|_| kind == "spot").map(|m| m.url.clone()).filter(|u| !u.is_empty()).unwrap_or_else(|| cfg.meta.url.clone());
            if !url.is_empty() { params += &format!("ClickThroughURL={url}\n"); }
            if !params.is_empty() { let _ = write_atomic(&sd.join(format!("{n}.sls_params")), params.as_bytes()); }
            info.push(SlideInfo { name: n, kind, bytes: bytes.len(), updated: now.clone() });
        }
        Ok(info)
    }

    pub fn slide_bytes(&self, name: &str) -> Option<Vec<u8>> {
        if name.contains('/') || name.contains('\\') || name.contains("..") { return None; }
        let cfg = self.cfg.read().clone();
        let dir = dab_dir(&cfg, &self.data_dir);
        if name == "current.jpg" { return std::fs::read(dir.join(name)).ok(); }
        std::fs::read(dir.join("slides").join(name)).ok()
    }

    pub fn request_epg(&self) { *self.epg_force.lock() = true; }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn render_tags() {
        let mut ctx = Ctx::default();
        ctx.vars.insert("ARTIST".into(), "Bob Marley".into());
        ctx.vars.insert("TITLE".into(), "Is This Love".into());
        let t = render("{ARTIST} - {TITLE}", &ctx);
        assert_eq!(t.text, "Bob Marley - Is This Love");
        assert_eq!(t.tags, vec![(tag::ARTIST, 0, 10), (tag::TITLE, 13, 12)]);
        // prázdné proměnné nezanechají dvojité mezery
        let t2 = render("Právě vysíláme: {SHOW} {HOST_WITH}", &ctx);
        assert_eq!(t2.text, "Právě vysíláme");
        let f = dls_file(&t, true, true, true);
        assert!(f.contains("DL_PLUS_TAG=4 0 9\n") && f.contains("DL_PLUS_TAG=1 13 11\n") && f.ends_with("Bob Marley - Is This Love\n"));
        let lim = limit(&t, 12, 4);
        assert_eq!(lim.tags, vec![(tag::ARTIST, 0, 10)]);
    }
}
