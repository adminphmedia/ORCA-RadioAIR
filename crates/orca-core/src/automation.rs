//! Automatika: režim Auto/Manual, doplňování fronty podle clocků, logování vysílání,
//! exporty now-playing a metadata pro streamy.

use crate::config::Config;
use crate::db::Db;
use crate::encoder::EncoderManager;
use crate::engine::Engine;
use crate::events::{Event, EventBus};
use crate::model::{QueueItem, Track};
use crate::nowplaying::{public_text, NowPlayingWriter};
use crate::rotation::{clock_for, grid_key, select, Exclusions};
use chrono::{DateTime, Local, Timelike};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode { Auto, Manual }

/// Položka náhledu hodiny (není ve frontě).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PreviewItem {
    pub time: Option<String>,
    pub kind: String,
    pub artist: String,
    pub title: String,
    pub category: String,
    pub duration_ms: i64,
    pub track_id: Option<i64>,
    /// selector | clock:<název>
    pub source: String,
}

pub struct Automation {
    db: Arc<Db>,
    cfg: Arc<RwLock<Config>>,
    engine: Arc<Engine>,
    #[allow(dead_code)]
    encoders: Arc<EncoderManager>,
    events: EventBus,
    np: NowPlayingWriter,
    mode: RwLock<Mode>,
    log_ids: Mutex<HashMap<String, (i64, Instant)>>,
    cursor: Mutex<(String, usize)>,
    history: Mutex<VecDeque<(String, Track)>>,
    last_no_tracks_alert: Mutex<Option<Instant>>,
    last_sched: Mutex<Option<Instant>>,
    last_maint: Mutex<Option<Instant>>,
    /// Časomíry v přesný čas: (cíl = začátek posledního tónu, nastavení, id, popis)
    countdowns: Mutex<Vec<PendingCountdown>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingCountdown {
    pub id: String, pub at: String, #[serde(skip)] pub at_dt: DateTime<Local>, pub spec: crate::countdown::CountdownSpec, pub label: String,
    /// manual (vložená ručně – zaregistrovaná) | event (z pravidelné události – zatím jen v plánu)
    #[serde(default)] pub source: String,
    /// klíč výskytu události (zrušení jen tohoto výskytu)
    #[serde(default)] pub key: String,
    /// čas prvního pípnutí (resp. začátku podkresu)
    #[serde(default)] pub start: String,
    /// název pípání a podkresu pro zobrazení ve frontě
    #[serde(default)] pub style_name: String,
    #[serde(default)] pub bed: String,
}

pub fn now_local(cfg: &Config) -> DateTime<Local> { Local::now() + chrono::Duration::seconds(cfg.station.time_offset_s) }

impl Automation {
    pub fn start(db: Arc<Db>, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, encoders: Arc<EncoderManager>, events: EventBus, data_dir: PathBuf) -> Arc<Self> {
        let mode = if cfg.read().auto.start_in_auto { Mode::Auto } else { Mode::Manual };
        let a = Arc::new(Self {
            db, cfg, engine, encoders, events: events.clone(), np: NowPlayingWriter { data_dir }, mode: RwLock::new(mode),
            log_ids: Mutex::new(HashMap::new()), cursor: Mutex::new((String::new(), 0)), history: Mutex::new(VecDeque::new()),
            last_no_tracks_alert: Mutex::new(None), last_sched: Mutex::new(None), last_maint: Mutex::new(None), countdowns: Mutex::new(vec![]),
        });
        let rx = events.subscribe();
        { let a = a.clone(); std::thread::Builder::new().name("orca-auto-events".into()).spawn(move || { while let Ok(ev) = rx.recv() { a.on_event(ev); } }).ok(); }
        { let a = a.clone(); std::thread::Builder::new().name("orca-auto-fill".into()).spawn(move || loop { a.countdown_tick(); a.breaks_tick(); a.sched_tick(); a.fill_queue(); a.maintenance(); std::thread::sleep(Duration::from_millis(300)); }).ok(); }
        a
    }

    pub fn mode(&self) -> Mode { *self.mode.read() }

    pub fn set_mode(&self, m: Mode) {
        *self.mode.write() = m;
        self.events.emit(Event::ModeChanged { mode: match m { Mode::Auto => "auto", Mode::Manual => "manual" }.into() });
    }

    /// Náhled hodiny začínající v `at` bez zařazení do fronty (ruční režim: „co by se hrálo“).
    /// Z publikovaného plánu Music Selectoru konkrétní skladby, jinak kroky hodinového schématu.
    pub fn preview_hour(&self, at: DateTime<Local>) -> Vec<PreviewItem> {
        let cfg = self.cfg.read().clone();
        let mut out = vec![];
        if crate::selector::load_config(&self.db).map(|c| c.data.mode == "active").unwrap_or(false) {
            let day = at.format("%Y-%m-%d").to_string();
            if let Ok(Some(log)) = crate::selector::logs::published(&self.db, &day) {
                if let Ok(plan) = crate::selector::logs::load_plan(&self.db, log.id) {
                    if let Some(h) = plan.hours.iter().find(|h| h.spec.hour == at.hour()) {
                        for it in &h.items {
                            let tr = it.track_id.and_then(|id| self.db.get_track(id).ok().flatten());
                            out.push(PreviewItem {
                                time: it.est_start.map(|d| d.with_timezone(&Local).format("%H:%M:%S").to_string()),
                                kind: tr.as_ref().map(|t| t.kind().as_str().to_string()).unwrap_or_else(|| it.slot.kind.clone()),
                                artist: tr.as_ref().map(|t| t.artist.clone()).unwrap_or_default(),
                                title: tr.as_ref().map(|t| t.title.clone()).unwrap_or_else(|| it.slot.label.clone()),
                                category: tr.as_ref().map(|t| t.category.clone()).unwrap_or_else(|| it.slot.category.clone()),
                                duration_ms: if it.length_ms > 0 { it.length_ms } else { tr.as_ref().map(|t| t.duration_ms).unwrap_or(0) },
                                track_id: it.track_id, source: "selector".into(),
                            });
                        }
                        return out;
                    }
                }
            }
        }
        // nanečisto projde kroky clocku a vybere skladby (bez zápisu do fronty a historie), dokud nenaplní hodinu
        let Some(clock) = clock_for(&cfg, &at) else { return out };
        if clock.steps.is_empty() { return out; }
        let end = at + chrono::Duration::hours(1);
        let mut t = at;
        let mut excl = Exclusions { track_ids: HashSet::new(), artists: HashSet::new(), at: Some(at.naive_local()) };
        for it in self.engine.queue_snapshot() { excl.track_ids.insert(it.track.id); }
        let mut misses = 0;
        for i in 0..200 {
            if t >= end || misses >= clock.steps.len() { break; }
            let step = &clock.steps[i % clock.steps.len()];
            let src = format!("clock:{}", clock.name);
            if let Some(kind) = &step.stitch {
                out.push(PreviewItem { time: Some(t.format("%H:%M:%S").to_string()), kind: "stitch".into(), artist: String::new(), title: kind.clone(), category: String::new(), duration_ms: 0, track_id: None, source: src });
                continue;
            }
            excl.at = Some(t.naive_local());
            match select(&self.db, &cfg.auto, step, &clock.backup_category, &excl) {
                Ok(Some((tr, _))) => {
                    misses = 0;
                    excl.track_ids.insert(tr.id);
                    if !tr.artist.is_empty() && tr.kind() == crate::model::TrackKind::Song { excl.artists.insert(tr.artist.to_lowercase()); }
                    let len = (tr.mix_ms.max(tr.cue_in_ms) - tr.cue_in_ms).max(0);
                    let len = if len > 0 { len } else { tr.duration_ms };
                    out.push(PreviewItem { time: Some(t.format("%H:%M:%S").to_string()), kind: tr.kind().as_str().to_string(), artist: tr.artist.clone(), title: tr.title.clone(), category: tr.category.clone(), duration_ms: tr.duration_ms, track_id: Some(tr.id), source: src });
                    t += chrono::Duration::milliseconds(len.max(1000));
                }
                _ => { misses += 1; }
            }
        }
        out
    }

    /// Fronta s odhadovanými časy začátku.
    pub fn queue_with_eta(&self) -> Vec<QueueItem> {
        let cfg = self.cfg.read().clone();
        let st = self.engine.snapshot();
        let mut t = now_local(&cfg);
        if let Some(cur) = &st.current {
            // živý vstup čeká na moderátora – počítá se plánovaná délka, ne „neomezené“ čekání
            let left = if cur.item.live && !cur.item.live_auto { (cur.item.track.duration_ms - cur.position_ms).max(0) } else { (cur.mix_at_ms - cur.position_ms).max(0) };
            t += chrono::Duration::milliseconds(left);
        }
        let mut q = self.engine.queue_snapshot();
        let mut last_start = t;
        for it in q.iter_mut() {
            if it.is_attached_overlay() {
                // hraje přes předchozí položku – čas neposouvá
                it.eta = Some(last_start.to_rfc3339());
                continue;
            }
            it.eta = Some(t.to_rfc3339());
            last_start = t;
            t += chrono::Duration::milliseconds(it.slot_ms());
        }
        q
    }

    /// Kdy dohraje poslední položka fronty.
    fn queue_end(&self, q: &[QueueItem], cfg: &Config) -> DateTime<Local> {
        let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Local));
        match q.iter().rev().find(|i| !i.is_attached_overlay()) {
            Some(last) => {
                let eta = last.eta.as_deref().and_then(parse).unwrap_or_else(|| now_local(cfg));
                eta + chrono::Duration::milliseconds(last.slot_ms())
            }
            None => {
                let st = self.engine.snapshot();
                now_local(cfg) + chrono::Duration::milliseconds(st.current.map(|c| (c.mix_at_ms - c.position_ms).max(0)).unwrap_or(0))
            }
        }
    }

    /// Reklamní bloky: vložení do decků v pravý čas (nebo okamžitý start se stažením skladby).
    fn breaks_tick(&self) {
        let cfg = self.cfg.read().clone();
        if !cfg.spots.auto_insert || self.mode() != Mode::Auto { return; }
        let now = now_local(&cfg);
        let today = now.date_naive();
        for date in [today, today.succ_opt().unwrap_or(today)] {
            for def in &cfg.spots.breaks {
                let Ok(t) = chrono::NaiveTime::parse_from_str(&def.time, "%H:%M") else { continue };
                let Some(bdt) = date.and_time(t).and_local_timezone(Local).single() else { continue };
                let bdt = bdt; // čas bloku v čase stanice
                if bdt < now - chrono::Duration::seconds(60) || bdt > now + chrono::Duration::minutes(cfg.spots.lookahead_min as i64) { continue; }
                let date_s = date.format("%Y-%m-%d").to_string();
                let preview = match crate::spots::build_block(&self.db, &cfg, date, def, false) { Ok(b) => b, Err(e) => { log::error!("blok {}: {e}", def.time); continue; } };
                if preview.inserted { continue; }
                if preview.items.is_empty() { let _ = crate::spots::mark_inserted(&self.db, &date_s, &def.time); continue; }
                let to_items = |blk: &crate::spots::BreakBlock| -> Vec<QueueItem> {
                    blk.items.iter().map(|bi| {
                        let reason = format!("Reklamní blok {} • priorita {} {} • {}", blk.time, bi.priority, bi.priority_name, bi.campaign);
                        let mut q = QueueItem::new(bi.track.clone(), "break", reason);
                        q.break_ref = Some(crate::spots::break_key(&blk.date, &blk.time));
                        q
                    }).collect()
                };
                if preview.start_immediately {
                    if now + chrono::Duration::milliseconds(300) < bdt { continue; }
                    let Ok(blk) = crate::spots::build_block(&self.db, &cfg, date, def, true) else { continue };
                    for (i, it) in to_items(&blk).into_iter().enumerate() { self.engine.queue_push(it, Some(i)); }
                    let _ = crate::spots::mark_inserted(&self.db, &date_s, &def.time);
                    self.engine.send(crate::engine::Cmd::Next);
                    log::info!("reklamní blok {} spuštěn okamžitě ({} položek)", def.time, blk.items.len());
                    continue;
                }
                let target = match def.position.as_str() {
                    "end" => bdt - chrono::Duration::milliseconds(preview.duration_ms),
                    "time" => bdt - chrono::Duration::milliseconds(preview.time_offset_ms),
                    _ => bdt,
                };
                let q = self.queue_with_eta();
                let end = self.queue_end(&q, &cfg);
                // fronta ještě nesahá k času bloku → počkáme (automatika ji průběžně doplňuje)
                if end < target { continue; }
                // nejbližší hranice mezi položkami k času bloku (včetně konce fronty)
                let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Local));
                let mut bounds: Vec<(usize, DateTime<Local>)> = q.iter().enumerate()
                    .filter(|(_, i)| !i.is_attached_overlay())
                    .filter_map(|(k, i)| i.eta.as_deref().and_then(parse).map(|e| (k, e))).collect();
                bounds.push((q.len(), end));
                let idx = bounds.iter().min_by_key(|(_, e)| (*e - target).num_milliseconds().abs()).map(|(k, _)| *k).unwrap_or(q.len());
                let Ok(blk) = crate::spots::build_block(&self.db, &cfg, date, def, true) else { continue };
                for (k, it) in to_items(&blk).into_iter().enumerate() { self.engine.queue_push(it, Some(idx + k)); }
                let _ = crate::spots::mark_inserted(&self.db, &date_s, &def.time);
                log::info!("reklamní blok {} vložen na pozici {idx} ({} položek)", def.time, blk.items.len());
            }
        }
    }

    /// Ruční vložení bloku hned jako další (i v režimu Manual).
    pub fn insert_break_now(&self, date: chrono::NaiveDate, time: &str) -> anyhow::Result<usize> {
        let cfg = self.cfg.read().clone();
        let def = cfg.spots.breaks.iter().find(|b| b.time == time).cloned().unwrap_or(crate::config::BreakDef { time: time.to_string(), ..Default::default() });
        let blk = crate::spots::build_block(&self.db, &cfg, date, &def, true)?;
        let n = blk.items.len();
        for (i, bi) in blk.items.iter().enumerate() {
            let mut q = QueueItem::new(bi.track.clone(), "break", format!("Reklamní blok {} (vložen ručně) • priorita {} {} • {}", blk.time, bi.priority, bi.priority_name, bi.campaign));
            q.break_ref = Some(crate::spots::break_key(&blk.date, &blk.time));
            self.engine.queue_push(q, Some(i));
        }
        crate::spots::mark_inserted(&self.db, &blk.date, &blk.time)?;
        Ok(n)
    }

    /// Vloží položky tak, aby začaly co nejblíž cílovému času (na hranici mezi položkami), nebo okamžitě.
    /// Vrací false, pokud fronta ještě k cílovému času nesahá (zkusit později).
    fn insert_at(&self, items: Vec<QueueItem>, target: DateTime<Local>, immediate: bool, override_auto: bool, cfg: &Config) -> bool {
        let now = now_local(cfg);
        if immediate {
            if now + chrono::Duration::milliseconds(300) < target { return false; }
            let n = items.len();
            for (i, it) in items.into_iter().enumerate() { self.engine.queue_push(it, Some(i)); }
            if override_auto { self.drop_auto_after(n); }
            self.engine.send(crate::engine::Cmd::Next);
            return true;
        }
        let q = self.queue_with_eta();
        let end = self.queue_end(&q, cfg);
        if end < target && self.mode() == Mode::Auto { return false; }
        let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Local));
        let mut bounds: Vec<(usize, DateTime<Local>)> = q.iter().enumerate().filter(|(_, i)| !i.is_attached_overlay())
            .filter_map(|(k, i)| i.eta.as_deref().and_then(parse).map(|e| (k, e))).collect();
        bounds.push((q.len(), end));
        let idx = bounds.iter().min_by_key(|(_, e)| (*e - target).num_milliseconds().abs()).map(|(k, _)| *k).unwrap_or(q.len());
        let n = items.len();
        for (k, it) in items.into_iter().enumerate() { self.engine.queue_push(it, Some(idx + k)); }
        if override_auto { self.drop_auto_after(idx + n); }
        true
    }

    /// Override: zruší automatikou vybrané položky za vloženým playlistem.
    fn drop_auto_after(&self, from: usize) {
        let ids: Vec<String> = self.engine.queue_snapshot().into_iter().skip(from).filter(|i| i.source == "auto" || i.source == "plan").map(|i| i.id).collect();
        for id in ids { self.engine.queue_remove(&id); }
    }

    /// Naplánované playlisty, programová mřížka a události.
    /// Časomíra: hned (přeruší aktuální položku), jako další, nebo v přesný čas.
    pub fn countdown_now(&self, spec: &crate::countdown::CountdownSpec, interrupt: bool) -> anyhow::Result<()> {
        let rate = self.engine.sample_rate().max(8000);
        let (path, ms) = crate::countdown::render(&self.db, &self.np.data_dir, spec, rate, 0)?;
        self.engine.queue_push(crate::countdown::queue_item(spec, &path, ms, "manual", "časomíra"), Some(0));
        if interrupt { self.engine.send(crate::engine::Cmd::Next); }
        Ok(())
    }

    pub fn countdown_at(&self, at: DateTime<Local>, spec: crate::countdown::CountdownSpec, label: &str) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.countdowns.lock().push(PendingCountdown { id: id.clone(), at: at.to_rfc3339(), at_dt: at, label: if label.is_empty() { spec.label() } else { label.to_string() }, spec, source: "manual".into(), key: String::new(), start: String::new(), style_name: String::new(), bed: String::new() });
        id
    }
    pub fn countdowns(&self) -> Vec<PendingCountdown> {
        let mut v = self.countdowns.lock().clone();
        // pravidelné časomíry z událostí v příštích 3 hodinách (ještě nezaregistrované)
        let cfg = self.cfg.read().clone();
        if cfg.schedule.enabled {
            let now = now_local(&cfg);
            let today = now.date_naive();
            for date in [today, today.succ_opt().unwrap_or(today)] {
                let Ok(occ) = crate::sched::day_occurrences(&self.db, date) else { continue };
                for o in occ.into_iter().filter(|o| !o.fired && o.source == "event") {
                    let Some(t) = crate::sched::occurrence_dt(date, &o.time) else { continue };
                    if t < now || t > now + chrono::Duration::hours(3) || v.iter().any(|c| c.at_dt == t) { continue; }
                    let Ok(Some(e)) = crate::sched::get::<crate::sched::SchedEvent>(&self.db, o.id) else { continue };
                    if e.action != "countdown" { continue; }
                    let spec = e.countdown.clone().unwrap_or_default();
                    v.push(PendingCountdown { id: format!("ev:{}", o.key), at: t.to_rfc3339(), at_dt: t, label: if e.name.is_empty() { spec.label() } else { e.name.clone() }, spec, source: "event".into(), key: o.key.clone(), start: String::new(), style_name: String::new(), bed: String::new() });
                }
            }
        }
        for c in v.iter_mut() {
            let pre = c.spec.to_final_ms() + if c.spec.bed_track_id.is_some() { (c.spec.bed_pre_s * 1000.0) as i64 } else { 0 };
            c.start = (c.at_dt - chrono::Duration::milliseconds(pre)).to_rfc3339();
            c.style_name = crate::countdown::STYLES.iter().find(|x| x.0 == c.spec.style).map(|x| x.1.to_string()).unwrap_or_default();
            c.bed = c.spec.bed_track_id.and_then(|id| self.db.get_track(id).ok().flatten()).map(|t| if t.artist.is_empty() { t.title } else { format!("{} – {}", t.artist, t.title) }).unwrap_or_default();
        }
        v.sort_by_key(|c| c.at_dt); v
    }
    pub fn countdown_cancel(&self, id: &str) -> bool {
        // výskyt pravidelné události: přeskočit jen tento výskyt
        if let Some(key) = id.strip_prefix("ev:") { return crate::sched::mark_fired(&self.db, key).is_ok(); }
        let mut l = self.countdowns.lock(); let n = l.len(); l.retain(|c| c.id != id); l.len() != n }

    /// Každých 300 ms: časomíry v přesný čas. Start se spočítá tak, aby poslední tón zazněl přesně v cíl;
    /// nepřesnost smyčky se vyrovná krátkou předehrou (ticho / podkres) na začátku souboru.
    fn countdown_tick(&self) {
        let cfg = self.cfg.read().clone();
        let now = now_local(&cfg);
        let due: Vec<PendingCountdown> = {
            let mut l = self.countdowns.lock();
            l.retain(|c| c.at_dt > now - chrono::Duration::seconds(30)); // zmeškané zahodit
            let (d, keep): (Vec<_>, Vec<_>) = l.drain(..).partition(|c| {
                let pre = if c.spec.bed_track_id.is_some() { (c.spec.bed_pre_s.clamp(0.0, 30.0) * 1000.0) as i64 } else { 0 };
                let start = c.at_dt - chrono::Duration::milliseconds(c.spec.to_final_ms() + pre);
                now >= start - chrono::Duration::milliseconds(450)
            });
            *l = keep; d
        };
        for c in due {
            let pre = if c.spec.bed_track_id.is_some() { (c.spec.bed_pre_s.clamp(0.0, 30.0) * 1000.0) as i64 } else { 0 };
            let start = c.at_dt - chrono::Duration::milliseconds(c.spec.to_final_ms() + pre);
            let lead = (start - now_local(&cfg)).num_milliseconds().max(0);
            let rate = self.engine.sample_rate().max(8000);
            match crate::countdown::render(&self.db, &self.np.data_dir, &c.spec, rate, lead) {
                Ok((path, ms)) => {
                    self.engine.queue_push(crate::countdown::queue_item(&c.spec, &path, ms, "event", &format!("časomíra na {}", c.at_dt.format("%H:%M:%S"))), Some(0));
                    self.engine.send(crate::engine::Cmd::Next);
                    log::info!("časomíra {}: start, předehra {lead} ms", c.at_dt.format("%H:%M:%S"));
                }
                Err(e) => self.events.alert("warning", "countdown", format!("Časomíra {}: {e}", c.at_dt.format("%H:%M:%S"))),
            }
        }
    }

    fn sched_tick(&self) {
        {
            let mut l = self.last_sched.lock();
            if l.map(|t| t.elapsed() < Duration::from_millis(900)).unwrap_or(false) { return; }
            *l = Some(Instant::now());
        }
        let cfg = self.cfg.read().clone();
        if !cfg.schedule.enabled { return; }
        let now = now_local(&cfg);
        let today = now.date_naive();
        for date in [today, today.succ_opt().unwrap_or(today)] {
            let occ = match crate::sched::day_occurrences(&self.db, date) { Ok(o) => o, Err(e) => { log::error!("plánování: {e}"); return; } };
            for o in occ.into_iter().filter(|o| !o.fired) {
                let Some(t) = crate::sched::occurrence_dt(date, &o.time) else { continue };
                if t < now - chrono::Duration::seconds(120) {
                    // zmeškané (program neběžel) – už nespouštět
                    let _ = crate::sched::mark_fired(&self.db, &o.key);
                    continue;
                }
                if t > now + chrono::Duration::minutes(cfg.schedule.lookahead_min.max(1) as i64) { continue; }
                if let Err(e) = self.fire(&cfg, &o, t, now) { log::error!("plánování {}: {e}", o.name); let _ = crate::sched::mark_fired(&self.db, &o.key); }
            }
        }
    }

    fn fire(&self, cfg: &Config, o: &crate::sched::Occurrence, t: DateTime<Local>, now: DateTime<Local>) -> anyhow::Result<()> {
        use crate::sched::{self, GridEntry, PlItem, Playlist, SchedEvent};
        let due = now >= t - chrono::Duration::milliseconds(300);
        let reason_base = format!("{} {} ({})", match o.source.as_str() { "playlist" => "playlist", "grid" => "program", _ => "událost" }, o.name, &o.time[..5]);
        // (položky, okamžitě, override)
        let mut plan: Option<(Vec<PlItem>, bool, bool, bool)> = None; // + shuffle
        match o.source.as_str() {
            "playlist" => {
                let Some(p) = sched::get::<Playlist>(&self.db, o.id)? else { return sched::mark_fired(&self.db, &o.key) };
                plan = Some((p.items, p.start_immediately, p.mode == "override", p.shuffle));
            }
            "grid" => {
                let Some(g) = sched::get::<GridEntry>(&self.db, o.id)? else { return sched::mark_fired(&self.db, &o.key) };
                match g.content.as_str() {
                    "episode" => plan = Some((vec![PlItem { typ: "episode".into(), show_id: g.show_id, mode: g.episode_mode.clone(), nth: g.nth, ..Default::default() }], g.start_immediately, false, false)),
                    "playlist" => if let Some(p) = g.playlist_id.and_then(|id| sched::get::<Playlist>(&self.db, id).ok().flatten()) { plan = Some((p.items, g.start_immediately || p.start_immediately, p.mode == "override", p.shuffle)); },
                    "relay" => {
                        let dur = (o.duration_ms / 1000) as u32;
                        plan = Some((vec![PlItem { typ: "relay".into(), relay_id: g.relay_id.clone(), duration_s: dur, ..Default::default() }], g.start_immediately, true, false));
                    }
                    "live" => { if due { self.set_mode(Mode::Manual); self.events.alert("info", "sched_live", format!("{}: živé vysílání – automatika přepnuta na RUČNĚ", o.name)); } else { return Ok(()); } }
                    _ => {}
                }
                if plan.is_none() { return if due { sched::mark_fired(&self.db, &o.key) } else { Ok(()) }; }
            }
            _ => {
                let Some(e) = sched::get::<SchedEvent>(&self.db, o.id)? else { return sched::mark_fired(&self.db, &o.key) };
                let imm = e.start_immediately;
                if e.action == "countdown" {
                    // přesný čas hlídá countdown_tick (300 ms) – tady jen zaregistrovat
                    self.countdown_at(t, e.countdown.clone().unwrap_or_default(), &e.name);
                    return sched::mark_fired(&self.db, &o.key);
                }
                match e.action.as_str() {
                    "playlist" => if let Some(p) = e.playlist_id.and_then(|id| sched::get::<Playlist>(&self.db, id).ok().flatten()) { plan = Some((p.items, imm || p.start_immediately, p.mode == "override", p.shuffle)); },
                    "episode" => plan = Some((vec![PlItem { typ: "episode".into(), show_id: e.show_id, mode: "next".into(), ..Default::default() }], imm, false, false)),
                    "relay_start" => plan = Some((vec![PlItem { typ: "relay".into(), relay_id: e.relay_id.clone(), duration_s: e.duration_s, ..Default::default() }], imm, true, false)),
                    "track" => plan = Some((vec![PlItem { typ: "track".into(), track_id: e.track_id, ..Default::default() }], imm, false, false)),
                    "stitch" => plan = Some((vec![PlItem { typ: "stitch".into(), stitch: e.stitch.clone(), ..Default::default() }], imm, false, false)),
                    "break" => {
                        if !due { return Ok(()); }
                        let _ = self.insert_break_now(t.date_naive(), &t.format("%H:%M").to_string());
                    }
                    other => {
                        if !due { return Ok(()); }
                        match other {
                            "mode_auto" => self.set_mode(Mode::Auto),
                            "mode_manual" => self.set_mode(Mode::Manual),
                            "relay_stop" => {
                                if self.engine.snapshot().current.map(|c| c.item.relay.is_some()).unwrap_or(false) { self.engine.send(crate::engine::Cmd::Next); }
                                let ids: Vec<String> = self.engine.queue_snapshot().into_iter().filter(|i| i.relay.is_some()).map(|i| i.id).collect();
                                for id in ids { self.engine.queue_remove(&id); }
                            }
                            "clear_queue" => self.engine.queue_clear(),
                            "stop_after" => self.engine.send(crate::engine::Cmd::StopAfterCurrent(true)),
                            _ => {}
                        }
                    }
                }
                if plan.is_none() { return if due { sched::mark_fired(&self.db, &o.key) } else { Ok(()) }; }
            }
        }
        let Some((mut items, immediate, override_auto, shuffle)) = plan else { return Ok(()) };
        if self.mode() != Mode::Auto {
            // v ručním režimu se playlisty nespouští samy
            if due { self.events.alert("warning", "sched_skipped", format!("{reason_base}: přeskočeno – automatika je v režimu RUČNĚ")); sched::mark_fired(&self.db, &o.key)?; }
            return Ok(());
        }
        if shuffle { use rand::seq::SliceRandom; items.shuffle(&mut rand::thread_rng()); }
        // rozvinout až těsně před vložením (výběr podle aktuální rotace)
        let q = self.queue_with_eta();
        if !immediate && self.queue_end(&q, cfg) < t { return Ok(()); }
        if immediate && !due { return Ok(()); }
        let (qitems, aired, warnings) = sched::expand(&self.db, cfg, &items, "playlist", &reason_base, 0)?;
        for w in warnings { self.events.alert("warning", "sched_item", format!("{reason_base}: {w}")); }
        if qitems.is_empty() { return sched::mark_fired(&self.db, &o.key); }
        let n = qitems.len();
        if self.insert_at(qitems, t, immediate, override_auto, cfg) {
            for e in aired { let _ = sched::mark_episode_aired(&self.db, e); }
            sched::mark_fired(&self.db, &o.key)?;
            log::info!("{reason_base}: vloženo {n} položek{}", if immediate { " (okamžitě)" } else { "" });
        }
        Ok(())
    }

    /// Údržba: automatické generování plánu, import playlistů ze složky.
    fn maintenance(&self) {
        {
            let mut l = self.last_maint.lock();
            if l.map(|t| t.elapsed() < Duration::from_secs(30)).unwrap_or(false) { return; }
            *l = Some(Instant::now());
        }
        let cfg = self.cfg.read().clone();
        if cfg.schedule.auto_generate_hours > 0 {
            let now = now_local(&cfg);
            let mut by_date: std::collections::BTreeMap<chrono::NaiveDate, Vec<u32>> = Default::default();
            for i in 0..cfg.schedule.auto_generate_hours.min(168) as i64 {
                let t = now + chrono::Duration::hours(i);
                by_date.entry(t.date_naive()).or_default().push(chrono::Timelike::hour(&t));
            }
            for (d, hours) in by_date {
                if let Err(e) = crate::sched::generate_plan(&self.db, &cfg, d, &hours, false) { log::error!("generování plánu: {e}"); }
            }
        }
        if !cfg.schedule.playlist_import_dir.as_os_str().is_empty() {
            match crate::sched::auto_import_dir(&self.db, &cfg.schedule.playlist_import_dir) {
                Ok(v) => for m in v { log::info!("{m}"); },
                Err(e) => log::warn!("import playlistů: {e}"),
            }
        }
    }

    fn fill_queue(&self) {
        let cfg = self.cfg.read().clone();
        if self.mode() != Mode::Auto && !cfg.auto.fill_in_manual { return; }
        // cíl: aspoň min_queue položek a (volitelně) celá další hodina – konec další hodiny od teď
        let horizon = {
            let n = now_local(&cfg);
            let top = n.with_minute(0).and_then(|d| d.with_second(0)).and_then(|d| d.with_nanosecond(0)).unwrap_or(n);
            top + chrono::Duration::hours(2)
        };
        let mut guard = 0;
        let max_steps = cfg.auto.min_queue * 3 + if cfg.auto.fill_next_hour { 120 } else { 0 };
        loop {
            if guard >= max_steps { break; }
            let q = self.queue_with_eta();
            let short = q.len() < cfg.auto.min_queue;
            let need_hour = cfg.auto.fill_next_hour && self.queue_end(&q, &cfg) < horizon;
            if !short && !need_hour { break; }
            guard += 1;
            // čas, kdy začne nově přidaná položka → podle něj vybereme clock
            let slot_time = if q.is_empty() { now_local(&cfg) } else { self.queue_end(&q, &cfg) };
            if cfg.schedule.use_plan {
                match crate::sched::plan_take(&self.db, &slot_time) {
                    Ok(Some(it)) => {
                        let reason = format!("plán vysílání {}", slot_time.format("%d.%m. %H:00"));
                        match crate::sched::expand(&self.db, &cfg, std::slice::from_ref(&it), "plan", &reason, 0) {
                            Ok((items, aired, _)) => {
                                for i in items { self.engine.queue_push(i, None); }
                                for e in aired { let _ = crate::sched::mark_episode_aired(&self.db, e); }
                            }
                            Err(e) => log::error!("plán: {e}"),
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => log::error!("plán: {e}"),
                }
            }
            // ORCA Music Selector (aktivní): hudba z publikovaného plánu; bez plánu / po vyčerpání hodiny nouzově z clocků
            if crate::selector::load_config(&self.db).map(|c| c.data.mode == "active").unwrap_or(false) {
                match crate::selector::logs::take_next(&self.db, slot_time) {
                    Ok(Some(crate::selector::logs::Next::Track { item_id, track_id, slot, reason })) => {
                        if let Ok(Some(track)) = self.db.get_track(track_id) {
                            if !track.missing && track.valid_at(slot_time.naive_local()) {
                                let mut item = QueueItem::new(track, "selector", reason);
                                item.plan_item = Some(item_id);
                                if slot.kind == "imaging" { item.position = slot.position.clone(); }
                                self.engine.queue_push(item, None);
                                continue;
                            }
                        }
                        self.events.alert("warning", "selector_missing", format!("Položka plánu {item_id}: skladba {track_id} chybí – doplněno z clocku"));
                    }
                    Ok(Some(crate::selector::logs::Next::Stitch { kind, reason })) => {
                        let est = match crate::stitcher::resolve(&self.db, &cfg, &kind, &q, slot_time) { Ok(p) => p.total_ms(), Err(_) => 0 };
                        if est > 0 { self.engine.queue_push(crate::stitcher::queue_item(&kind, est, &reason), None); continue; }
                    }
                    Ok(None) => {}
                    Err(e) => log::error!("selector: {e}"),
                }
            }
            let Some(clock) = clock_for(&cfg, &slot_time) else { return };
            if clock.steps.is_empty() { return; }
            let key = format!("{}|{}", grid_key(&slot_time), clock.name);
            let step_idx = {
                let mut c = self.cursor.lock();
                if c.0 != key { *c = (key.clone(), 0); }
                let i = c.1 % clock.steps.len();
                c.1 += 1;
                i
            };
            let step = &clock.steps[step_idx];
            if let Some(kind) = &step.stitch {
                let est = match crate::stitcher::resolve(&self.db, &cfg, kind, &q, slot_time) { Ok(p) => p.total_ms(), Err(_) => 0 };
                if est > 0 {
                    self.engine.queue_push(crate::stitcher::queue_item(kind, est, &format!("clock '{}', krok {}: Stitcher", clock.name, step_idx + 1)), None);
                }
                continue;
            }
            let mut excl = Exclusions { track_ids: HashSet::new(), artists: HashSet::new(), at: Some(slot_time.naive_local()) };
            for it in &q { excl.track_ids.insert(it.track.id); if !it.track.artist.is_empty() && it.track.kind() == crate::model::TrackKind::Song { excl.artists.insert(it.track.artist.to_lowercase()); } }
            if let Some(cur) = self.engine.snapshot().current { excl.track_ids.insert(cur.item.track.id); if !cur.item.track.artist.is_empty() { excl.artists.insert(cur.item.track.artist.to_lowercase()); } }
            match select(&self.db, &cfg.auto, step, &clock.backup_category, &excl) {
                Ok(Some((track, reason))) => {
                    let reason = format!("{} (clock '{}', krok {})", reason, clock.name, step_idx + 1);
                    let mut item = QueueItem::new(track, "auto", reason);
                    if step.kind.is_overlay_capable() { item.position = step.position.clone(); }
                    self.engine.queue_push(item, None);
                }
                Ok(None) => {
                    let mut last = self.last_no_tracks_alert.lock();
                    if last.map(|t| t.elapsed() > Duration::from_secs(60)).unwrap_or(true) {
                        *last = Some(Instant::now());
                        self.events.alert("warning", "auto_no_tracks", format!("Pro krok clocku '{}' ({} / {}) nejsou k dispozici žádné skladby", clock.name, step.kind.as_str(), if step.category.is_empty() { "*" } else { &step.category }));
                    }
                    // když nic nenajde ani jeden krok, nezacyklíme se
                    if guard >= clock.steps.len() && self.engine.queue.lock().is_empty() { return; }
                }
                Err(e) => { log::error!("výběr skladby: {e}"); return; }
            }
        }
    }

    fn on_event(&self, ev: Event) {
        match ev {
            Event::TrackStarted { item, channel } => {
                match self.db.log_start_ex(&item.track, &item.source, &item.reason, Some(&item.id), &channel, item.plan_item) {
                    Ok(id) => { self.log_ids.lock().insert(item.id.clone(), (id, Instant::now())); }
                    Err(e) => log::error!("log: {e}"),
                }
                if channel != "main" { return; }
                let cfg = self.cfg.read().clone();
                if let Some(text) = public_text(&cfg, &item.track) {
                    {
                        let mut h = self.history.lock();
                        h.push_front((now_local(&cfg).to_rfc3339(), item.track.clone()));
                        h.truncate(50);
                    }
                    let hist: Vec<(String, Track)> = self.history.lock().iter().cloned().collect();
                    let next = self.queue_with_eta();
                    self.np.update(&cfg, &item.track, &text, &hist, &next);
                }
            }
            Event::TrackEnded { item_id, played_ms } => {
                if let Some((id, _)) = self.log_ids.lock().remove(&item_id) { let _ = self.db.log_end(id, played_ms); }
            }
            _ => {}
        }
    }
}
