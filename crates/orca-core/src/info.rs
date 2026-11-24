//! Informace na displeji rádia (DAB+ DLS a MOT Slideshow, volitelně RDS RadioText).
//!
//! Model jako u hudby: **informační sady** (zdroje obsahu – skladba, pořad, počasí, počasí v lokalitách,
//! zprávy z RSS s fotkou, webkamera s popisem a počasím, vlastní text …) se skládají do **playlistů sdělení**
//! (pořadí, doba zobrazení, kolik položek za průchod) a **rozvrh** určuje, který playlist platí v kterou
//! hodinu a den. Text (DLS) a obrázek (slide) jednoho sdělení se vysílají současně.

use crate::meta::{render, tag, Ctx, Tagged};
use crate::model::TrackKind;
use anyhow::{anyhow, Result};
use image::DynamicImage;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Place { pub name: String, pub lat: f64, pub lon: f64 }

/// Informační sada (zdroj obsahu).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct InfoSource {
    pub id: String,
    pub name: String,
    /// song | next | show | next_show | station | contact | datetime | weather | weather_places | rss | webcam | text | promo
    pub kind: String,
    pub enabled: bool,
    /// Nadpis na slidu a v textu (např. „Zprávy“, „Kamera“) – prázdné = výchozí podle typu
    pub label: String,
    /// Vlastní text (šablona s {ARTIST}, {SHOW}, {WEATHER_TEMP} …); u RSS/kamer/počasí přepíše výchozí tvar
    pub text: String,
    /// RSS / Atom kanál nebo adresa obrázku z kamery (JPEG snapshot)
    pub url: String,
    /// Vlastní obrázek (text, kamera bez URL)
    pub image_path: String,
    /// Popis lokality kamery
    pub description: String,
    /// Lokalita (kamera – pro počasí)
    pub place: Place,
    /// Lokality (počasí v lokalitách)
    pub places: Vec<Place>,
    /// Max. počet položek ze zdroje (RSS)
    pub max_items: u32,
    /// Obnovení dat (min)
    pub refresh_min: u32,
    /// Fotky ze zpráv (RSS) stahovat a dávat do slidu
    pub images: bool,
    /// Odkaz po kliknutí na slide (DAB+ ClickThroughURL) – promo / reklama
    pub click_url: String,
    /// Reklamní sdělení (v DLS označené jako reklama)
    pub is_ad: bool,
}

impl Default for InfoSource {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), kind: "text".into(), enabled: true, label: String::new(), text: String::new(), url: String::new(), image_path: String::new(),
            description: String::new(), place: Place::default(), places: vec![], max_items: 5, refresh_min: 10, images: true, click_url: String::new(), is_ad: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct InfoEntry {
    pub source: String,
    /// Doba zobrazení jednoho sdělení (s)
    pub hold_s: u32,
    /// Kolik položek sady za jeden průchod playlistu (0 = všechny); další průchod pokračuje dalšími
    pub count: u32,
    /// Posílat text (DLS / RadioText)
    pub dls: bool,
    /// Posílat obrázek (slideshow)
    pub slide: bool,
}
impl Default for InfoEntry { fn default() -> Self { Self { source: String::new(), hold_s: 12, count: 0, dls: true, slide: true } } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct InfoPlaylist { pub id: String, pub name: String, pub color: String, pub items: Vec<InfoEntry> }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct InfoConfig {
    /// Zapnuto: displej řídí playlisty sdělení (místo jednoduchého střídání textů a slidů)
    pub enabled: bool,
    pub sources: Vec<InfoSource>,
    pub playlists: Vec<InfoPlaylist>,
    /// Rozvrh „den-hodina“ (den 1 = Po … 7 = Ne) → id playlistu
    pub grid: BTreeMap<String, String>,
    pub default_playlist: String,
    /// Při začátku nové skladby nejdřív na tolik sekund zobrazit interpreta a název (0 = ne)
    pub song_first_s: u32,
    /// RDS RadioText sleduje stejná sdělení (zkrácená na 64 znaků)
    pub rds_follow: bool,
}

impl Default for InfoConfig {
    fn default() -> Self {
        let src = |id: &str, name: &str, kind: &str| InfoSource { id: id.into(), name: name.into(), kind: kind.into(), ..Default::default() };
        let e = |s: &str, hold: u32| InfoEntry { source: s.into(), hold_s: hold, ..Default::default() };
        Self {
            enabled: false,
            sources: vec![src("song", "Právě hraje", "song"), src("show", "Pořad a moderátor", "show"), src("next", "Dále uslyšíte", "next"), src("weather", "Počasí", "weather"), src("station", "Stanice", "station"), src("contact", "Kontakt do studia", "contact")],
            playlists: vec![InfoPlaylist { id: "zakladni".into(), name: "Základní".into(), color: "#14b8a6".into(),
                items: vec![e("song", 15), e("show", 10), e("weather", 10), e("song", 10), e("next", 8), e("station", 8)] }],
            grid: BTreeMap::new(), default_playlist: "zakladni".into(), song_first_s: 15, rds_follow: true,
        }
    }
}

impl InfoConfig {
    pub fn playlist_at(&self, weekday: u32, hour: u32) -> Option<&InfoPlaylist> {
        let id = self.grid.get(&format!("{weekday}-{hour}")).filter(|s| !s.is_empty()).unwrap_or(&self.default_playlist);
        self.playlists.iter().find(|p| &p.id == id).or_else(|| self.playlists.first())
    }
    pub fn source(&self, id: &str) -> Option<&InfoSource> { self.sources.iter().find(|s| s.id == id) }
}

// ---------------------------------------------------------------------------------------------
// Stažená data zdrojů

#[derive(Clone, Default)]
pub struct FItem { pub title: String, pub text: String, pub link: String, pub published: String, pub image: Option<Arc<DynamicImage>> }

#[derive(Clone, Default)]
pub struct Fetched { pub updated: Option<String>, pub error: Option<String>, pub items: Vec<FItem>, pub weather: Vec<crate::weather::Weather>, pub sig: String }

#[derive(Debug, Clone, Serialize)]
pub struct SourceStatus { pub id: String, pub name: String, pub kind: String, pub updated: Option<String>, pub error: Option<String>, pub items: usize }

#[derive(Default)]
pub struct InfoCache {
    pub data: RwLock<HashMap<String, Fetched>>,
    last: Mutex<HashMap<String, Instant>>,
    force: Mutex<HashSet<String>>,
}

fn needs_fetch(kind: &str) -> bool { matches!(kind, "rss" | "webcam" | "weather_places") }

fn sig(s: &InfoSource) -> String { format!("{}|{}|{}|{:?}|{:?}|{}", s.kind, s.url, s.image_path, s.place, s.places, s.max_items) }

impl InfoCache {
    pub fn request(&self, id: Option<&str>) { match id { Some(i) => { self.force.lock().insert(i.to_string()); } None => { self.last.lock().clear(); } } }

    pub fn statuses(&self, cfg: &InfoConfig) -> Vec<SourceStatus> {
        let d = self.data.read();
        cfg.sources.iter().map(|s| {
            let f = d.get(&s.id);
            SourceStatus { id: s.id.clone(), name: s.name.clone(), kind: s.kind.clone(), updated: f.and_then(|f| f.updated.clone()), error: f.and_then(|f| f.error.clone()),
                items: f.map(|f| f.items.len().max(f.weather.len())).unwrap_or(0) }
        }).collect()
    }

    /// Jedno kolo: stáhne zdroje, kterým vypršela platnost (nebo byly vyžádány).
    pub fn tick(&self, cfg: &InfoConfig, en: bool) {
        for s in cfg.sources.iter().filter(|s| s.enabled && needs_fetch(&s.kind)) {
            let forced = self.force.lock().remove(&s.id);
            let changed = self.data.read().get(&s.id).map(|f| f.sig != sig(s)).unwrap_or(true);
            let due = self.last.lock().get(&s.id).map(|t| t.elapsed() >= Duration::from_secs(s.refresh_min.max(1) as u64 * 60)).unwrap_or(true);
            if !(forced || changed || due) { continue; }
            self.last.lock().insert(s.id.clone(), Instant::now());
            let r = fetch_source(s, en);
            let mut d = self.data.write();
            let e = d.entry(s.id.clone()).or_default();
            e.sig = sig(s);
            match r {
                Ok(f) => { e.items = f.items; e.weather = f.weather; e.updated = Some(chrono::Local::now().to_rfc3339()); e.error = None; }
                Err(err) => { log::warn!("info {}: {err}", s.name); e.error = Some(err.to_string()); }
            }
        }
    }
}

const MAX_IMG: u64 = 6 * 1024 * 1024;

pub fn http_bytes(url: &str) -> Result<Vec<u8>> {
    let r = ureq::get(url).set("User-Agent", "ORCA-RadioAIR/0.6 (+info display)").timeout(Duration::from_secs(10)).call()?;
    let mut b = vec![];
    r.into_reader().take(MAX_IMG).read_to_end(&mut b)?;
    Ok(b)
}

fn image_from_url(url: &str) -> Option<Arc<DynamicImage>> {
    if url.is_empty() { return None; }
    match http_bytes(url).ok().and_then(|b| crate::slides::load_image(&b)) {
        Some(i) => Some(Arc::new(i.thumbnail(800, 600))),
        None => { log::debug!("obrázek {url} se nepodařilo načíst"); None }
    }
}

fn fetch_source(s: &InfoSource, en: bool) -> Result<Fetched> {
    let mut f = Fetched::default();
    match s.kind.as_str() {
        "rss" => {
            if s.url.trim().is_empty() { return Err(anyhow!("chybí adresa RSS kanálu")); }
            let xml = String::from_utf8_lossy(&http_bytes(s.url.trim())?).to_string();
            let mut items = parse_feed(&xml)?;
            items.truncate(s.max_items.clamp(1, 30) as usize);
            if s.images { for it in items.iter_mut() { if let Some(u) = it.link_image.take() { it.item.image = image_from_url(&u); } } }
            f.items = items.into_iter().map(|i| i.item).collect();
        }
        "webcam" => {
            let img = if !s.url.trim().is_empty() { image_from_url(s.url.trim()).ok_or_else(|| anyhow!("obrázek z kamery se nepodařilo stáhnout"))? }
                else { crate::slides::load_path(&s.image_path).map(Arc::new).ok_or_else(|| anyhow!("chybí adresa obrázku kamery"))? };
            f.items = vec![FItem { title: s.place.name.clone(), text: s.description.clone(), image: Some(img), ..Default::default() }];
            if s.place.lat != 0.0 || s.place.lon != 0.0 {
                let wc = crate::config::WeatherConfig { enabled: true, place: s.place.name.clone(), lat: s.place.lat, lon: s.place.lon, ..Default::default() };
                if let Ok(w) = crate::weather::fetch(&wc, en) { f.weather = vec![w]; }
            }
        }
        "weather_places" => {
            for p in s.places.iter().filter(|p| !p.name.is_empty()) {
                let wc = crate::config::WeatherConfig { enabled: true, place: p.name.clone(), lat: p.lat, lon: p.lon, ..Default::default() };
                f.weather.push(crate::weather::fetch(&wc, en)?);
            }
            if f.weather.is_empty() { return Err(anyhow!("nejsou zadané lokality")); }
        }
        _ => {}
    }
    Ok(f)
}

pub struct FeedItem { pub item: FItem, pub link_image: Option<String> }

/// RSS 2.0 / Atom → položky (název, text bez HTML, odkaz, obrázek z enclosure / media:content / media:thumbnail / <img>).
pub fn parse_feed(xml: &str) -> Result<Vec<FeedItem>> {
    let doc = roxmltree::Document::parse_with_options(xml, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() }).map_err(|e| anyhow!("kanál není platné RSS/Atom: {e}"))?;
    let mut out = vec![];
    for n in doc.descendants().filter(|n| n.is_element() && matches!(n.tag_name().name(), "item" | "entry")) {
        let child = |name: &str| n.children().find(|c| c.is_element() && c.tag_name().name() == name);
        let text_of = |name: &str| child(name).map(|c| c.descendants().filter(|d| d.is_text()).map(|d| d.text().unwrap_or("")).collect::<String>()).unwrap_or_default();
        let title = clean(&text_of("title"));
        if title.is_empty() { continue; }
        let raw = { let d = text_of("description"); if d.is_empty() { let s = text_of("summary"); if s.is_empty() { text_of("content") } else { s } } else { d } };
        let link = child("link").map(|l| l.attribute("href").map(|s| s.to_string()).unwrap_or_else(|| l.text().unwrap_or("").trim().to_string())).unwrap_or_default();
        let published = { let p = text_of("pubDate"); if p.is_empty() { text_of("updated") } else { p } };
        let mut img = None;
        for c in n.children().filter(|c| c.is_element()) {
            let nm = c.tag_name().name();
            let ty = c.attribute("type").unwrap_or("");
            if (nm == "enclosure" && (ty.starts_with("image") || ty.is_empty())) || ((nm == "content" || nm == "thumbnail") && c.attribute("url").is_some() && (ty.is_empty() || ty.starts_with("image") || c.attribute("medium") == Some("image"))) {
                if let Some(u) = c.attribute("url") { img = Some(u.to_string()); break; }
            }
            if nm == "group" { if let Some(u) = c.descendants().find(|d| matches!(d.tag_name().name(), "content" | "thumbnail")).and_then(|d| d.attribute("url")) { img = Some(u.to_string()); break; } }
        }
        if img.is_none() { img = first_img_src(&raw); }
        out.push(FeedItem { item: FItem { title, text: clean(&strip_html(&raw)), link, published, image: None }, link_image: img });
    }
    Ok(out)
}

fn first_img_src(html: &str) -> Option<String> {
    let i = html.find("<img")?;
    let rest = &html[i..];
    let s = rest.find("src=")? + 4;
    let q = rest[s..].chars().next()?;
    if q != '"' && q != '\'' { return None; }
    let end = rest[s + 1..].find(q)?;
    Some(rest[s + 1..s + 1 + end].to_string())
}

pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() { match c { '<' => in_tag = true, '>' if in_tag => { in_tag = false; out.push(' '); } _ if !in_tag => out.push(c), _ => {} } }
    out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

fn clean(s: &str) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") }

// ---------------------------------------------------------------------------------------------
// Sdělení (frames)

#[derive(Debug, Clone, Serialize)]
pub struct Frame {
    pub source: String,
    pub source_name: String,
    pub kind: String,
    /// pořadí položky v sadě (RSS: číslo zprávy, počasí: lokalita)
    pub idx: usize,
    pub dls: Tagged,
    pub hold_s: u32,
    pub send_dls: bool,
    pub send_slide: bool,
}

fn default_label(kind: &str, en: bool) -> &'static str {
    match (kind, en) {
        ("song", false) => "Právě hraje", ("song", true) => "Now playing",
        ("next", false) => "Dále uslyšíte", ("next", true) => "Coming up",
        ("show", false) => "Právě vysíláme", ("show", true) => "On air now",
        ("next_show", false) => "Následuje", ("next_show", true) => "Next show",
        ("weather" | "weather_places", false) => "Počasí", ("weather" | "weather_places", true) => "Weather",
        ("rss", false) => "Zprávy", ("rss", true) => "News",
        ("webcam", false) => "Kamera", ("webcam", true) => "Webcam",
        ("contact", false) => "Studio", ("contact", true) => "Studio",
        ("datetime", false) => "Čas", ("datetime", true) => "Time",
        _ => "",
    }
}

pub fn label(s: &InfoSource, en: bool) -> String { if s.label.trim().is_empty() { default_label(&s.kind, en).to_string() } else { s.label.trim().to_string() } }

fn tagged(text: String, t: u8) -> Tagged { let n = text.chars().count(); Tagged { text, tags: if n > 0 { vec![(t, 0, n)] } else { vec![] } } }

fn weather_vars(ctx: &Ctx, w: &crate::weather::Weather) -> Ctx {
    let mut c = ctx.clone();
    c.vars.insert("WEATHER_PLACE".into(), w.place.clone());
    c.vars.insert("WEATHER_TEMP".into(), crate::weather::fmt_temp(w.temp));
    c.vars.insert("WEATHER_TEXT".into(), w.text.clone());
    c.vars.insert("WEATHER_MIN".into(), w.min.map(crate::weather::fmt_temp).unwrap_or_default());
    c.vars.insert("WEATHER_MAX".into(), w.max.map(crate::weather::fmt_temp).unwrap_or_default());
    c.vars.insert("WEATHER_WIND".into(), w.wind_kmh.map(|x| format!("{} km/h", x.round() as i64)).unwrap_or_default());
    c
}

/// Texty všech položek sady (bez omezení počtu) – (idx, text).
pub fn source_texts(cfg: &crate::config::Config, s: &InfoSource, ctx: &Ctx, cache: &InfoCache) -> Vec<(usize, Tagged)> {
    let en = cfg.station.language == "en";
    let m = &cfg.meta;
    let lbl = label(s, en);
    let custom = |c: &Ctx| if s.text.trim().is_empty() { None } else { Some(render(&s.text, c)) };
    let one = |t: Option<Tagged>| t.filter(|t| !t.text.trim().is_empty()).map(|t| vec![(0, t)]).unwrap_or_default();
    let data = cache.data.read().get(&s.id).cloned().unwrap_or_default();
    match s.kind.as_str() {
        "song" => one(if ctx.is_song { custom(ctx).or_else(|| Some(render(&m.tpl_song, ctx))) } else { None }),
        "next" => one(ctx.next_track.as_ref().filter(|t| t.kind() == TrackKind::Song).map(|_| custom(ctx).unwrap_or_else(|| render(&m.tpl_next, ctx)))),
        "show" => one(ctx.show.as_ref().map(|_| custom(ctx).unwrap_or_else(|| render(&m.tpl_show, ctx)))),
        "next_show" => one(ctx.next_show.as_ref().map(|_| custom(ctx).unwrap_or_else(|| render(if en { "Next: {SHOW_NEXT} at {SHOW_NEXT_TIME}" } else { "Následuje: {SHOW_NEXT} od {SHOW_NEXT_TIME}" }, ctx)))),
        "station" => one(Some(custom(ctx).unwrap_or_else(|| render(&m.tpl_station, ctx)))),
        "contact" => {
            let def = if en { "Call the studio {PHONE} · SMS {SMS} · {EMAIL}" } else { "Volejte do studia {PHONE} · SMS {SMS} · {EMAIL}" };
            let mut t = custom(ctx).unwrap_or_else(|| render(def, ctx));
            t.text = t.text.trim_matches(|c: char| c == '·' || c.is_whitespace()).replace("·  ·", "·").to_string();
            one(Some(t))
        }
        "datetime" => one(Some(custom(ctx).unwrap_or_else(|| render(if en { "It is {TIME} · {DATE}" } else { "Je {TIME} · {DATE}" }, ctx)))),
        "weather" => one(ctx.weather.as_ref().map(|w| { let c = weather_vars(ctx, w); let mut t = custom(&c).unwrap_or_else(|| render(&m.tpl_weather, &c)); let n = t.text.chars().count(); t.tags = vec![(tag::WEATHER, 0, n)]; t })),
        "weather_places" => data.weather.iter().enumerate().map(|(i, w)| {
            let c = weather_vars(ctx, w);
            let t = custom(&c).unwrap_or_else(|| render(if en { "Weather {WEATHER_PLACE}: {WEATHER_TEMP} °C, {WEATHER_TEXT}" } else { "Počasí {WEATHER_PLACE}: {WEATHER_TEMP} °C, {WEATHER_TEXT}" }, &c));
            (i, tagged(t.text, tag::WEATHER))
        }).collect(),
        "rss" => data.items.iter().enumerate().map(|(i, it)| {
            let mut c = ctx.clone();
            c.vars.insert("HEADLINE".into(), it.title.clone());
            c.vars.insert("SUMMARY".into(), it.text.clone());
            c.vars.insert("LABEL".into(), lbl.clone());
            let t = custom(&c).map(|t| t.text).unwrap_or_else(|| format!("{lbl}: {}", it.title));
            (i, tagged(t, tag::NEWS))
        }).collect(),
        "webcam" => {
            let Some(it) = data.items.first() else { return vec![] };
            let mut c = match data.weather.first() { Some(w) => weather_vars(ctx, w), None => ctx.clone() };
            c.vars.insert("PLACE".into(), it.title.clone());
            c.vars.insert("DESCRIPTION".into(), it.text.clone());
            c.vars.insert("LABEL".into(), lbl.clone());
            let def = if data.weather.is_empty() { "{LABEL} {PLACE}: {DESCRIPTION}" } else { "{LABEL} {PLACE}: {DESCRIPTION} · {WEATHER_TEMP} °C, {WEATHER_TEXT}" };
            one(Some(custom(&c).unwrap_or_else(|| render(def, &c))))
        }
        "promo" => {
            let t = custom(ctx).unwrap_or_default();
            if t.text.trim().is_empty() { return if s.image_path.is_empty() { vec![] } else { vec![(0, Tagged::default())] }; }
            vec![(0, if s.is_ad { tagged(t.text, tag::ADVERTISEMENT) } else { t })]
        }
        _ => one(custom(ctx)),
    }
}

/// Sdělení jednoho průchodu playlistu. `cursors` posouvá vícepoložkové sady (RSS, lokality) mezi průchody.
pub fn frames(cfg: &crate::config::Config, pl: &InfoPlaylist, ctx: &Ctx, cache: &InfoCache, cursors: &mut HashMap<String, usize>) -> Vec<Frame> {
    let info = &cfg.meta.info;
    let mut out = vec![];
    for (ei, e) in pl.items.iter().enumerate() {
        let Some(s) = info.source(&e.source).filter(|s| s.enabled) else { continue };
        let all = source_texts(cfg, s, ctx, cache);
        let has_slide_only = all.is_empty() && e.slide && matches!(s.kind.as_str(), "text" | "promo") && !s.image_path.is_empty();
        let pick: Vec<(usize, Tagged)> = if has_slide_only { vec![(0, Tagged::default())] }
            else if e.count == 0 || all.len() <= e.count as usize { all }
            else {
                let key = format!("{}#{ei}", pl.id);
                let c = cursors.entry(key).or_insert(0);
                let start = *c % all.len();
                *c = start + e.count as usize;
                (0..e.count as usize).map(|k| all[(start + k) % all.len()].clone()).collect()
            };
        for (idx, t) in pick {
            out.push(Frame { source: s.id.clone(), source_name: s.name.clone(), kind: s.kind.clone(), idx, dls: crate::meta::limit(&t, 128, 4), hold_s: e.hold_s.clamp(3, 600), send_dls: e.dls && !t.text.is_empty(), send_slide: e.slide });
        }
    }
    out
}

/// Obrázek (slide) sdělení.
pub fn render_slide(cfg: &crate::config::Config, f: &Frame, ctx: &Ctx, cache: &InfoCache, c: &crate::slides::Common) -> Option<image::RgbImage> {
    use crate::slides as sl;
    let en = cfg.station.language == "en";
    let s = cfg.meta.info.source(&f.source)?;
    let lbl = label(s, en);
    let data = cache.data.read().get(&s.id).cloned().unwrap_or_default();
    Some(match s.kind.as_str() {
        "song" => { let t = ctx.track.as_ref().filter(|_| ctx.is_song)?; let cover = crate::library::cover_for(t).and_then(|(b, _)| sl::load_image(&b)); sl::now_playing(c, cover.as_ref(), &lbl, &t.artist, if t.title.is_empty() { &t.filename } else { &t.title }) }
        "next" => { let t = ctx.next_track.as_ref()?; let cover = crate::library::cover_for(t).and_then(|(b, _)| sl::load_image(&b)); sl::now_playing(c, cover.as_ref(), &lbl, &t.artist, &t.title) }
        "show" => {
            let sh = ctx.show.as_ref()?;
            match sh.presenters.first().filter(|p| !p.photo_path.is_empty()) {
                Some(p) => { let photo = sl::load_path(&p.photo_path); sl::person(c, photo.as_ref(), &lbl, &sh.show.name, &format!("{} · {}–{}", p.name, sh.start.format("%H:%M"), sh.end.format("%H:%M"))) }
                None => { let im = sl::load_path(&sh.show.image_path); sl::show(c, im.as_ref(), &lbl, &sh.show.name, &format!("{}–{}", sh.start.format("%H:%M"), sh.end.format("%H:%M")), &sh.description) }
            }
        }
        "next_show" => { let sh = ctx.next_show.as_ref()?; let im = sl::load_path(&sh.show.image_path); sl::show(c, im.as_ref(), &lbl, &sh.show.name, &sh.start.format("%H:%M").to_string(), &sh.description) }
        "station" => sl::station(c, &cfg.meta.slogan, &cfg.meta.url),
        "contact" => sl::card(c, None, &lbl, &f.dls.text, ""),
        "datetime" => { let now = crate::automation::now_local(cfg); sl::clock(c, &now.format("%H:%M").to_string(), &now.format("%-d. %-m. %Y").to_string(), &lbl) }
        "weather" => { let w = ctx.weather.as_ref()?; weather_slide(c, w, &lbl) }
        "weather_places" => { let w = data.weather.get(f.idx)?; weather_slide(c, w, &lbl) }
        "rss" => { let it = data.items.get(f.idx)?; sl::headline(c, it.image.as_deref(), &format!("{lbl} · {}", s.name), &it.title, &it.text) }
        "webcam" => {
            let it = data.items.first()?;
            let line = match data.weather.first() { Some(w) => format!("{} · {} °C, {}", it.text, crate::weather::fmt_temp(w.temp), w.text), None => it.text.clone() };
            sl::webcam(c, it.image.as_deref(), &format!("{lbl} {}", it.title), line.trim_start_matches(" · "))
        }
        "promo" => { let img = sl::load_path(&s.image_path); sl::promo(c, img.as_ref(), &s.label, &f.dls.text) }
        _ => { let img = sl::load_path(&s.image_path); if img.is_some() && f.dls.text.is_empty() { sl::full(c, img.as_ref()?) } else { sl::card(c, img.as_ref(), &lbl, &f.dls.text, "") } }
    })
}

fn weather_slide(c: &crate::slides::Common, w: &crate::weather::Weather, lbl: &str) -> image::RgbImage {
    let mm = match (w.min, w.max) { (Some(a), Some(b)) => format!("min {} / max {} °C", crate::weather::fmt_temp(a), crate::weather::fmt_temp(b)), _ => String::new() };
    let temp = if w.manual { String::new() } else { crate::weather::fmt_temp(w.temp) };
    crate::slides::weather(c, &w.place, &temp, &w.text, &mm, lbl)
}

/// Stav rotace pro ovládací panel.
#[derive(Debug, Clone, Serialize, Default)]
pub struct InfoState {
    pub enabled: bool,
    pub playlist: String,
    pub playlist_name: String,
    pub frames: Vec<Frame>,
    pub index: usize,
    pub current: Option<Frame>,
    pub slide: Option<String>,
    pub until: Option<String>,
    pub song_first: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rss_and_atom() {
        let rss = r#"<?xml version="1.0"?><rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/"><channel><title>X</title>
          <item><title>První &amp; zpráva</title><description>&lt;p&gt;Text &lt;img src="https://a.cz/1.jpg"&gt; dál&lt;/p&gt;</description><link>https://a.cz/1</link></item>
          <item><title><![CDATA[Druhá  zpráva]]></title><media:content url="https://a.cz/2.jpg" medium="image"/></item>
          <item><title>Třetí</title><enclosure url="https://a.cz/3.jpg" type="image/jpeg" length="1"/></item></channel></rss>"#;
        let v = parse_feed(rss).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].item.title, "První & zpráva");
        assert_eq!(v[0].item.text, "Text dál");
        assert_eq!(v[0].link_image.as_deref(), Some("https://a.cz/1.jpg"));
        assert_eq!(v[1].item.title, "Druhá zpráva");
        assert_eq!(v[1].link_image.as_deref(), Some("https://a.cz/2.jpg"));
        assert_eq!(v[2].link_image.as_deref(), Some("https://a.cz/3.jpg"));
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>A</title><summary>S</summary><link href="https://b.cz"/></entry></feed>"#;
        let a = parse_feed(atom).unwrap();
        assert_eq!((a[0].item.title.as_str(), a[0].item.link.as_str()), ("A", "https://b.cz"));
    }

    #[test]
    fn rotation_and_grid() {
        let mut cfg = crate::config::Config::default();
        let info = &mut cfg.meta.info;
        info.sources.push(InfoSource { id: "z".into(), name: "Zprávy".into(), kind: "rss".into(), ..Default::default() });
        info.playlists.push(InfoPlaylist { id: "noc".into(), name: "Noc".into(), items: vec![InfoEntry { source: "z".into(), count: 2, ..Default::default() }, InfoEntry { source: "station".into(), ..Default::default() }], ..Default::default() });
        info.grid.insert("1-23".into(), "noc".into());
        assert_eq!(info.playlist_at(1, 23).unwrap().id, "noc");
        assert_eq!(info.playlist_at(1, 10).unwrap().id, "zakladni");
        let cache = InfoCache::default();
        cache.data.write().insert("z".into(), Fetched { items: (0..5).map(|i| FItem { title: format!("Zpráva {i}"), ..Default::default() }).collect(), ..Default::default() });
        let ctx = crate::meta::build_ctx(&cfg, None, None, None, None, None);
        let pl = cfg.meta.info.playlist_at(1, 23).unwrap().clone();
        let mut cur = HashMap::new();
        let a: Vec<String> = frames(&cfg, &pl, &ctx, &cache, &mut cur).iter().map(|f| f.dls.text.clone()).collect();
        let b: Vec<String> = frames(&cfg, &pl, &ctx, &cache, &mut cur).iter().map(|f| f.dls.text.clone()).collect();
        let c: Vec<String> = frames(&cfg, &pl, &ctx, &cache, &mut cur).iter().map(|f| f.dls.text.clone()).collect();
        assert_eq!(a[..2], ["Zprávy: Zpráva 0".to_string(), "Zprávy: Zpráva 1".to_string()]);
        assert_eq!(b[..2], ["Zprávy: Zpráva 2".to_string(), "Zprávy: Zpráva 3".to_string()]);
        assert_eq!(c[..2], ["Zprávy: Zpráva 4".to_string(), "Zprávy: Zpráva 0".to_string()]);
        assert_eq!(a.len(), 3, "stanice na konci");
        // slidy se vykreslí v limitu 50 kB
        let common = crate::slides::Common { w: 320, h: 240, brand: crate::slides::parse_color("#0f172a"), logo: None, station: "ORCA", footer: "ORCA" };
        let fr = frames(&cfg, &pl, &ctx, &cache, &mut cur);
        for f in &fr { let img = render_slide(&cfg, f, &ctx, &cache, &common).unwrap(); assert!(crate::slides::encode_jpeg(&img, 50).unwrap().len() <= 50 * 1024); }
    }
}
