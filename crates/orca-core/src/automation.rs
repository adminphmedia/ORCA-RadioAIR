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
use chrono::{DateTime, Local};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode { Auto, Manual }

pub struct Automation {
    db: Arc<Db>,
    cfg: Arc<RwLock<Config>>,
    engine: Arc<Engine>,
    encoders: Arc<EncoderManager>,
    events: EventBus,
    np: NowPlayingWriter,
    mode: RwLock<Mode>,
    log_ids: Mutex<HashMap<String, (i64, Instant)>>,
    cursor: Mutex<(String, usize)>,
    history: Mutex<VecDeque<(String, Track)>>,
    last_no_tracks_alert: Mutex<Option<Instant>>,
}

pub fn now_local(cfg: &Config) -> DateTime<Local> { Local::now() + chrono::Duration::seconds(cfg.station.time_offset_s) }

impl Automation {
    pub fn start(db: Arc<Db>, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, encoders: Arc<EncoderManager>, events: EventBus, data_dir: PathBuf) -> Arc<Self> {
        let mode = if cfg.read().auto.start_in_auto { Mode::Auto } else { Mode::Manual };
        let a = Arc::new(Self {
            db, cfg, engine, encoders, events: events.clone(), np: NowPlayingWriter { data_dir }, mode: RwLock::new(mode),
            log_ids: Mutex::new(HashMap::new()), cursor: Mutex::new((String::new(), 0)), history: Mutex::new(VecDeque::new()),
            last_no_tracks_alert: Mutex::new(None),
        });
        let rx = events.subscribe();
        { let a = a.clone(); std::thread::Builder::new().name("orca-auto-events".into()).spawn(move || { while let Ok(ev) = rx.recv() { a.on_event(ev); } }).ok(); }
        { let a = a.clone(); std::thread::Builder::new().name("orca-auto-fill".into()).spawn(move || loop { a.breaks_tick(); a.fill_queue(); std::thread::sleep(Duration::from_millis(300)); }).ok(); }
        a
    }

    pub fn mode(&self) -> Mode { *self.mode.read() }

    pub fn set_mode(&self, m: Mode) {
        *self.mode.write() = m;
        self.events.emit(Event::ModeChanged { mode: match m { Mode::Auto => "auto", Mode::Manual => "manual" }.into() });
    }

    /// Fronta s odhadovanými časy začátku.
    pub fn queue_with_eta(&self) -> Vec<QueueItem> {
        let cfg = self.cfg.read().clone();
        let st = self.engine.snapshot();
        let mut t = now_local(&cfg);
        if let Some(cur) = &st.current {
            t += chrono::Duration::milliseconds((cur.mix_at_ms - cur.position_ms).max(0));
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
            let (ci, mix, _) = it.track.effective_cues();
            t += chrono::Duration::milliseconds((mix - ci).max(0));
        }
        q
    }

    /// Kdy dohraje poslední položka fronty.
    fn queue_end(&self, q: &[QueueItem], cfg: &Config) -> DateTime<Local> {
        let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Local));
        match q.iter().rev().find(|i| !i.is_attached_overlay()) {
            Some(last) => {
                let eta = last.eta.as_deref().and_then(parse).unwrap_or_else(|| now_local(cfg));
                let (ci, mix, _) = last.track.effective_cues();
                eta + chrono::Duration::milliseconds((mix - ci).max(0))
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

    fn fill_queue(&self) {
        if self.mode() != Mode::Auto { return; }
        let cfg = self.cfg.read().clone();
        let mut guard = 0;
        while self.engine.queue.lock().len() < cfg.auto.min_queue && guard < cfg.auto.min_queue * 3 {
            guard += 1;
            let q = self.queue_with_eta();
            // čas, kdy začne nově přidaná položka → podle něj vybereme clock
            let slot_time = if q.is_empty() { now_local(&cfg) } else { self.queue_end(&q, &cfg) };
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
            let mut excl = Exclusions { track_ids: HashSet::new(), artists: HashSet::new() };
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
                match self.db.log_start(&item.track, &item.source, &item.reason) {
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
                    self.encoders.update_metadata(&text);
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
