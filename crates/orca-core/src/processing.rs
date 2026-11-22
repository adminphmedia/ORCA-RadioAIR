//! Správa vysílacího processingu: presety (tovární + vlastní), revize (rozpracovaná / aktivní),
//! nasazení s potvrzením z audio enginu, rollback, náhled A/B mimo živou cestu, měření, blokové presety
//! z programové mřížky a auditní log.

use crate::config::Config;
use crate::db::{now_utc, Db};
use crate::dsp::params::{effective, factory_presets, template, BasicControls, BranchConf, Preset, SCHEMA_VERSION};
use crate::dsp::{self, Branch, MasterChain, Meter, MeterSnap};
use crate::engine::{Engine, ProcMsg};
use crate::events::EventBus;
use anyhow::{anyhow, bail, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Revize nastavení stanice: preset (snímek), jednoduché ovladače, výstupní větve, zapnutí.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Revision {
    pub schema: u32,
    /// Master processing zapnutý (při migraci vypnuto = původní chování)
    pub enabled: bool,
    /// Servisní hard bypass celé DSP cesty včetně výstupní ochrany
    pub hard_bypass: bool,
    pub preset_id: String,
    pub preset: Preset,
    pub basic: BasicControls,
    pub branches: Vec<BranchConf>,
}

impl Default for Revision {
    fn default() -> Self {
        let p = factory_presets().into_iter().find(|p| p.id == "transparent").unwrap_or_default();
        Self { schema: SCHEMA_VERSION, enabled: false, hard_bypass: false, preset_id: p.id.clone(), preset: p, basic: BasicControls::default(), branches: vec![] }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RevisionRow { pub id: i64, pub created_at: String, pub author: String, pub note: String, pub status: String, pub data: Revision }

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProcStatus {
    pub active_id: Option<i64>,
    pub active: Option<Revision>,
    /// Co právě skutečně hraje (může se lišit od aktivní revize při blokovém presetu)
    pub live_preset_id: String,
    pub block_override: Option<String>,
    pub manual_hold: bool,
    pub generation: u64,
    pub applied: u64,
    pub master_latency_ms: f32,
    pub output_latency_ms: f32,
    pub meters: Vec<MeterSnap>,
    pub agc_gain_db: f32,
    pub agc_gated: bool,
    pub band_gr_db: [f32; 4],
    pub deess_gr_db: f32,
    pub cpu_us_last: u32,
    pub cpu_us_max: u32,
    pub block_us: u32,
    pub dsp_errors: u64,
    pub clip_blocks: u64,
    pub underruns: u64,
    pub last_error: Option<String>,
    pub external_hint: Option<String>,
}

struct State { active_id: Option<i64>, active: Revision, live_preset: String, block_override: Option<String>, manual_hold: bool, last_error: Option<String> }

pub struct Processing {
    db: Arc<Db>,
    cfg: Arc<RwLock<Config>>,
    engine: Arc<Engine>,
    events: EventBus,
    data_dir: PathBuf,
    state: Mutex<State>,
    generation_ctr: AtomicU64,
    deploy_lock: Mutex<()>,
    capture: Arc<Mutex<Capture>>,
}

/// Posledních 30 s programu před processingem (pro náhled A/B).
struct Capture { buf: Vec<f32>, pos: usize, filled: bool }
impl Capture {
    fn push(&mut self, x: &[f32]) { for v in x { self.buf[self.pos] = *v; self.pos += 1; if self.pos == self.buf.len() { self.pos = 0; self.filled = true; } } }
    fn last(&self, frames: usize) -> Vec<f32> {
        let n = (frames * 2).min(if self.filled { self.buf.len() } else { self.pos });
        let mut out = Vec::with_capacity(n);
        let start = (self.pos + self.buf.len() - n) % self.buf.len();
        for i in 0..n { out.push(self.buf[(start + i) % self.buf.len()]); }
        out
    }
}

pub fn audit(db: &Db, user: &str, action: &str, detail: &str) {
    let _ = db.with_conn(|c| c.execute("INSERT INTO audit_log(at, user, action, detail) VALUES(?1,?2,?3,?4)", params![now_utc(), user, action, detail]));
}

pub fn audit_list(db: &Db, limit: i64) -> Result<Vec<serde_json::Value>> {
    db.with_conn(|c| {
        let mut st = c.prepare("SELECT id, at, user, action, detail FROM audit_log ORDER BY id DESC LIMIT ?1")?;
        let v = st.query_map([limit], |r| Ok(serde_json::json!({"id": r.get::<_, i64>(0)?, "at": r.get::<_, String>(1)?, "user": r.get::<_, String>(2)?, "action": r.get::<_, String>(3)?, "detail": r.get::<_, String>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })
}

impl Processing {
    pub fn start(db: Arc<Db>, cfg: Arc<RwLock<Config>>, engine: Arc<Engine>, events: EventBus, data_dir: PathBuf) -> Arc<Self> {
        let rate = engine.sample_rate() as usize;
        let p = Arc::new(Self {
            db, cfg, engine, events, data_dir, generation_ctr: AtomicU64::new(0), deploy_lock: Mutex::new(()),
            state: Mutex::new(State { active_id: None, active: Revision::default(), live_preset: String::new(), block_override: None, manual_hold: false, last_error: None }),
            capture: Arc::new(Mutex::new(Capture { buf: vec![0.0; rate * 2 * 30], pos: 0, filled: false })),
        });
        // aktivní revize z DB (při první migraci: processing vypnutý = původní chování)
        let (id, mut rev) = match p.active_row() { Ok(Some(r)) => (Some(r.id), r.data), _ => (None, Revision::default()) };
        p.fill_branches(&mut rev);
        { let mut st = p.state.lock(); st.active_id = id; st.active = rev.clone(); }
        if let Err(e) = p.apply(&rev, &rev.preset) { log::error!("processing: {e}"); p.state.lock().last_error = Some(e.to_string()); }
        p.start_meters();
        { let p2 = p.clone(); std::thread::Builder::new().name("orca-proc-sched".into()).spawn(move || loop { p2.block_tick(); std::thread::sleep(Duration::from_secs(5)); }).ok(); }
        p
    }

    // --- presety

    pub fn presets(&self) -> Result<Vec<Preset>> {
        let mut v = factory_presets();
        let custom: Vec<Preset> = self.db.with_conn(|c| -> Result<Vec<Preset>> {
            let mut st = c.prepare("SELECT data FROM proc_presets ORDER BY id")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().filter_map(|d| serde_json::from_str::<Preset>(&d).ok()).collect())
        })?;
        v.extend(custom.into_iter().map(|mut p| { p.factory = false; p }));
        Ok(v)
    }

    pub fn preset(&self, id: &str) -> Result<Preset> { self.presets()?.into_iter().find(|p| p.id == id).ok_or_else(|| anyhow!("preset '{id}' neexistuje")) }

    /// Uloží vlastní preset (tovární nelze přepsat – vznikne kopie).
    pub fn save_preset(&self, mut p: Preset, user: &str) -> Result<Preset> {
        let errs = p.params.validate(self.engine.sample_rate() as f32);
        if p.params.schema > SCHEMA_VERSION { bail!("{}", errs.join("; ")); }
        if p.name.trim().is_empty() { bail!("preset musí mít název"); }
        if p.id.is_empty() || factory_presets().iter().any(|f| f.id == p.id) { p.id = format!("custom-{}", &uuid::Uuid::new_v4().to_string()[..8]); }
        p.factory = false;
        let json = serde_json::to_string(&p)?;
        self.db.with_conn(|c| c.execute("INSERT OR REPLACE INTO proc_presets(id, data, updated_at) VALUES(?1,?2,?3)", params![p.id, json, now_utc()]))?;
        audit(&self.db, user, "preset_save", &format!("{} ({})", p.name, p.id));
        Ok(p)
    }

    pub fn delete_preset(&self, id: &str, user: &str) -> Result<()> {
        if factory_presets().iter().any(|f| f.id == id) { bail!("tovární preset nelze smazat"); }
        self.db.with_conn(|c| c.execute("DELETE FROM proc_presets WHERE id=?1", [id]))?;
        audit(&self.db, user, "preset_delete", id);
        Ok(())
    }

    /// Import presetu z JSON (s kontrolou schématu a rozsahů).
    pub fn import_preset(&self, json: &str, user: &str) -> Result<(Preset, Vec<String>)> {
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| anyhow!("neplatný JSON: {e}"))?;
        let schema = v.get("schema").and_then(|x| x.as_u64()).or_else(|| v.pointer("/params/schema").and_then(|x| x.as_u64())).unwrap_or(1) as u32;
        if schema > SCHEMA_VERSION { bail!("preset má novější schéma {schema}, tato verze ORCA podporuje {SCHEMA_VERSION}"); }
        let mut p: Preset = serde_json::from_value(v.get("preset").cloned().unwrap_or(v)).map_err(|e| anyhow!("neplatný preset: {e}"))?;
        let warn = p.params.validate(self.engine.sample_rate() as f32);
        p.id = String::new();
        let p = self.save_preset(p, user)?;
        Ok((p, warn))
    }

    pub fn export_preset(&self, id: &str) -> Result<String> {
        let p = self.preset(id)?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({ "orca": "processing-preset", "schema": SCHEMA_VERSION, "exported": now_utc(), "preset": p }))?)
    }

    // --- revize

    fn active_row(&self) -> Result<Option<RevisionRow>> {
        self.db.with_conn(|c| -> Result<Option<RevisionRow>> {
            let r = c.query_row("SELECT id, created_at, author, note, data, status FROM proc_revisions WHERE status='active' ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))).optional()?;
            Ok(r.and_then(|(id, created_at, author, note, data, status)| serde_json::from_str(&data).ok().map(|data| RevisionRow { id, created_at, author, note, status, data })))
        })
    }

    pub fn revisions(&self, limit: i64) -> Result<Vec<RevisionRow>> {
        self.db.with_conn(|c| -> Result<Vec<RevisionRow>> {
            let mut st = c.prepare("SELECT id, created_at, author, note, data, status FROM proc_revisions ORDER BY id DESC LIMIT ?1")?;
            let rows = st.query_map([limit], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().filter_map(|(id, created_at, author, note, data, status)| serde_json::from_str(&data).ok().map(|data| RevisionRow { id, created_at, author, note, status, data })).collect())
        })
    }

    /// Doplní chybějící větve pro skutečně existující výstupy (zvuková karta + každý enkodér).
    pub fn fill_branches(&self, rev: &mut Revision) {
        let cfg = self.cfg.read();
        let mut out = vec![];
        let find = |t: &str| rev.branches.iter().find(|b| b.target == t).cloned();
        if cfg.audio.output_device != "none" { out.push(find("output").unwrap_or_else(|| BranchConf { enabled: false, ..template("pcm", "output") })); }
        for e in cfg.encoders.iter() { out.push(find(&e.id).unwrap_or_else(|| template("icecast", &e.id))); }
        rev.branches = out;
    }

    pub fn default_revision(&self) -> Revision { let mut r = self.state.lock().active.clone(); self.fill_branches(&mut r); r }

    /// Postaví řetězce a předá je mixeru; čeká na potvrzení (max. 3 s).
    fn apply(&self, rev: &Revision, preset: &Preset) -> Result<u64> {
        let rate = self.engine.sample_rate();
        let mut eff = effective(preset, &rev.basic);
        let errs = eff.validate(rate as f32);
        if eff.schema > SCHEMA_VERSION { bail!("{}", errs.join("; ")); }
        let generation_ctr = self.generation_ctr.fetch_add(1, Ordering::SeqCst) + 1;
        let master = if rev.enabled && !rev.hard_bypass { Some(Box::new(MasterChain::new(eff, rate, generation_ctr))) } else { None };
        let mut branches = rev.branches.clone();
        for b in branches.iter_mut() { b.validate(); }
        let protect = !rev.hard_bypass;
        let out_branch = branches.iter().find(|b| b.target == "output" && b.enabled && protect).map(|b| Box::new(Branch::new(b.clone(), rate, rate)));
        {
            let mut rt = dsp::runtime().write();
            rt.enabled = master.is_some();
            rt.bypass = rev.hard_bypass;
            rt.branches = branches;
        }
        if !self.engine.proc_apply(ProcMsg { master, output_branch: out_branch, bypass: rev.hard_bypass, generation: generation_ctr }) { bail!("audio engine neběží"); }
        let t0 = Instant::now();
        while self.engine.proc_applied.load(Ordering::Acquire) < generation_ctr {
            if t0.elapsed() > Duration::from_secs(3) { bail!("audio engine nepotvrdil převzetí nastavení (přehrávání může být zastavené – nastavení se projeví po spuštění)"); }
            std::thread::sleep(Duration::from_millis(10));
        }
        dsp::runtime_gen().fetch_add(1, Ordering::SeqCst);
        self.state.lock().live_preset = preset.id.clone();
        Ok(generation_ctr)
    }

    /// Nasazení do vysílání: ověří, aplikuje, po potvrzení zapíše jako aktivní revizi.
    pub fn deploy(&self, mut rev: Revision, user: &str, note: &str, base_id: Option<i64>) -> Result<i64> {
        let _g = self.deploy_lock.lock();
        let cur = self.state.lock().active_id;
        if base_id.is_some() && base_id != cur { bail!("nastavení mezitím nasadil někdo jiný (revize #{}) – načtěte aktuální stav", cur.unwrap_or(0)); }
        let rate = self.engine.sample_rate() as f32;
        let errs = rev.preset.params.validate(rate);
        for b in rev.branches.iter_mut() { b.validate(); }
        if rev.preset.params.schema > SCHEMA_VERSION { bail!("{}", errs.join("; ")); }
        rev.schema = SCHEMA_VERSION;
        let prev = self.state.lock().active.clone();
        if let Err(e) = self.apply(&rev, &rev.preset) {
            // neúspěch: zachovat poslední funkční konfiguraci
            let _ = self.apply(&prev, &prev.preset);
            self.state.lock().last_error = Some(e.to_string());
            audit(&self.db, user, "deploy_failed", &e.to_string());
            return Err(e);
        }
        let json = serde_json::to_string(&rev)?;
        let id = self.db.with_conn(|c| -> Result<i64> {
            let tx = c.unchecked_transaction()?;
            tx.execute("UPDATE proc_revisions SET status='archived' WHERE status='active'", [])?;
            tx.execute("INSERT INTO proc_revisions(created_at, author, note, data, status) VALUES(?1,?2,?3,?4,'active')", params![now_utc(), user, note, json])?;
            let id = tx.last_insert_rowid();
            tx.commit()?;
            Ok(id)
        })?;
        { let mut st = self.state.lock(); st.active_id = Some(id); st.active = rev.clone(); st.block_override = None; st.manual_hold = false; st.last_error = None; }
        audit(&self.db, user, "deploy", &format!("revize #{id}: {} ({}), intenzita {}, zapnuto {}{}", rev.preset.name, rev.preset_id, rev.basic.intensity, rev.enabled, if note.is_empty() { String::new() } else { format!(" – {note}") }));
        self.events.alert("info", "proc_deployed", format!("Zvukový processing nasazen: {} (revize #{id})", rev.preset.name));
        Ok(id)
    }

    /// Obnovení konkrétní revize (bez restartu vysílání).
    pub fn rollback(&self, id: i64, user: &str) -> Result<i64> {
        let row = self.revisions(500)?.into_iter().find(|r| r.id == id).ok_or_else(|| anyhow!("revize #{id} neexistuje"))?;
        let mut data = row.data;
        self.fill_branches(&mut data);
        self.deploy(data, user, &format!("návrat k revizi #{id}"), None)
    }

    // --- blokové presety z programové mřížky

    pub fn set_manual_hold(&self, hold: bool, user: &str) -> Result<()> {
        let rev = { let mut st = self.state.lock(); st.manual_hold = hold; if !hold { st.block_override = None; } st.active.clone() };
        if !hold { self.apply(&rev, &rev.preset)?; }
        audit(&self.db, user, if hold { "block_hold" } else { "block_release" }, "");
        Ok(())
    }

    fn block_tick(&self) {
        let (rev, cur_override, hold) = { let st = self.state.lock(); (st.active.clone(), st.block_override.clone(), st.manual_hold) };
        if !rev.enabled || rev.hard_bypass || hold { return; }
        let now = crate::automation::now_local(&self.cfg.read());
        let want: Option<String> = crate::sched::now_next(&self.db, now).ok().and_then(|(cur, _)| cur)
            .and_then(|slot| crate::sched::get::<crate::sched::GridEntry>(&self.db, slot.grid_id).ok().flatten())
            .and_then(|g| g.proc_preset).filter(|p| !p.is_empty() && *p != rev.preset_id);
        if want == cur_override { return; }
        let preset = match &want { Some(id) => match self.preset(id) { Ok(p) => p, Err(e) => { log::warn!("blokový preset: {e}"); return; } }, None => rev.preset.clone() };
        match self.apply(&rev, &preset) {
            Ok(_) => {
                self.state.lock().block_override = want.clone();
                audit(&self.db, "plán", "block_preset", &match &want { Some(id) => format!("blok pořadu: {}", preset.name.clone() + " (" + id + ")"), None => "návrat k hlavnímu presetu".into() });
            }
            Err(e) => log::warn!("blokový preset: {e}"),
        }
    }

    // --- měření

    fn start_meters(self: &Arc<Self>) {
        let (tx, rx) = crossbeam_channel::bounded::<(u8, Arc<Vec<f32>>)>(256);
        *self.engine.meter_tx.write() = Some(tx);
        let rate = self.engine.sample_rate();
        let cap = self.capture.clone();
        std::thread::Builder::new().name("orca-proc-meter".into()).spawn(move || {
            let mut m_in = Meter::new("input", rate);
            let mut m_out = Meter::new("master", rate);
            let mut last = Instant::now();
            let mut reset = dsp::meter_reset_gen().load(Ordering::Relaxed);
            while let Ok((k, b)) = rx.recv() {
                let rg = dsp::meter_reset_gen().load(Ordering::Relaxed);
                if rg != reset { reset = rg; m_in.reset(); m_out.reset(); }
                if k == 0 { m_in.feed(&b); cap.lock().push(&b); } else { m_out.feed(&b); }
                if last.elapsed() > Duration::from_millis(250) { last = Instant::now(); dsp::publish(&m_in.snap); if m_out.frames_fed() > 0 { dsp::publish(&m_out.snap); } }
            }
        }).ok();
    }

    pub fn status(&self) -> ProcStatus {
        let st = self.state.lock();
        let s = dsp::stats();
        let rate = self.engine.sample_rate() as f32;
        let mut meters: Vec<MeterSnap> = dsp::meters().lock().values().cloned().collect();
        meters.sort_by(|a, b| a.key.cmp(&b.key));
        let out_lat = st.active.branches.iter().find(|b| b.target == "output" && b.enabled).map(|b| Branch::new(b.clone(), rate as u32, rate as u32).latency_ms()).unwrap_or(0.0);
        let master_on = st.active.enabled && !st.active.hard_bypass;
        ProcStatus {
            active_id: st.active_id, active: Some(st.active.clone()), live_preset_id: st.live_preset.clone(), block_override: st.block_override.clone(), manual_hold: st.manual_hold,
            generation: self.generation_ctr.load(Ordering::Relaxed), applied: self.engine.proc_applied.load(Ordering::Relaxed),
            master_latency_ms: 0.0, output_latency_ms: out_lat, meters,
            agc_gain_db: if master_on { dsp::get(&s.agc_gain_db) } else { 0.0 }, agc_gated: s.agc_gated.load(Ordering::Relaxed) != 0,
            band_gr_db: [0, 1, 2, 3].map(|i| if master_on { dsp::get(&s.band_gr_db[i]) } else { 0.0 }), deess_gr_db: if master_on { dsp::get(&s.deess_gr_db) } else { 0.0 },
            cpu_us_last: s.cpu_us_last.load(Ordering::Relaxed), cpu_us_max: s.cpu_us_max.load(Ordering::Relaxed), block_us: (480.0 / rate * 1e6) as u32,
            dsp_errors: s.errors.load(Ordering::Relaxed), clip_blocks: s.clip_samples.load(Ordering::Relaxed), underruns: self.engine.snapshot().output.underruns,
            last_error: st.last_error.clone(),
            external_hint: if st.active.enabled { None } else { Some("Processing je vypnutý – signál jde do streamů beze změny. Před zapnutím ověřte, zda na trase není externí procesor nebo automatické vyrovnávání hlasitosti.".into()) },
        }
    }

    // --- náhled A/B (offline, mimo živou cestu)

    pub fn preview(&self, req: &PreviewReq) -> Result<PreviewResult> {
        let rate = self.engine.sample_rate();
        let secs = req.seconds.clamp(5.0, 30.0);
        let input: Vec<f32> = match req.track_id {
            Some(id) => {
                let t = self.db.get_track(id)?.ok_or_else(|| anyhow!("soubor neexistuje"))?;
                let start = req.start_s.map(|s| (s * 1000.0) as i64).unwrap_or_else(|| (t.duration_ms / 3).max(0));
                let mut st = crate::decode::StereoStream::open(std::path::Path::new(&t.path), rate, start)?;
                let mut out = vec![]; let mut buf = vec![];
                while out.len() < (secs * rate as f32) as usize * 2 { if !st.read(&mut buf)? { break; } out.extend_from_slice(&buf); }
                out.truncate((secs * rate as f32) as usize * 2);
                out
            }
            None => self.capture.lock().last((secs * rate as f32) as usize),
        };
        if input.len() < rate as usize { bail!("málo audia pro náhled (spusťte přehrávání nebo vyberte soubor)"); }
        let dir = self.data_dir.join("Processing").join("preview");
        std::fs::create_dir_all(&dir)?;
        let token = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let render = |rev: &Revision| -> Vec<f32> {
            let mut chain = if rev.enabled && !rev.hard_bypass { Some(MasterChain::new(effective(&rev.preset, &rev.basic), rate, 0)) } else { None };
            let bconf = rev.branches.iter().find(|b| b.target == req.branch).cloned().unwrap_or_else(|| template("icecast", "preview"));
            let mut br = Branch::new(BranchConf { sample_rate: 0, ..bconf }, rate, rate);
            let mut out = Vec::with_capacity(input.len());
            let mut work = input.clone();
            for blk in work.chunks_mut(960) { if let Some(c) = chain.as_mut() { c.process(blk); } br.process(blk, &mut out); }
            out
        };
        let a = render(&req.a);
        let b = render(&req.b);
        let measure = |x: &[f32]| -> Measure {
            let mut e = ebur128::EbuR128::new(2, rate, ebur128::Mode::I | ebur128::Mode::LRA | ebur128::Mode::TRUE_PEAK | ebur128::Mode::SAMPLE_PEAK).unwrap();
            let _ = e.add_frames_f32(x);
            let tp = e.true_peak(0).unwrap_or(0.0).max(e.true_peak(1).unwrap_or(0.0));
            let sp = e.sample_peak(0).unwrap_or(0.0).max(e.sample_peak(1).unwrap_or(0.0));
            Measure { integrated_lufs: e.loudness_global().unwrap_or(-70.0).max(-70.0), lra_lu: e.loudness_range().unwrap_or(0.0), true_peak_dbtp: 20.0 * tp.max(1e-9).log10(), sample_peak_dbfs: 20.0 * sp.max(1e-9).log10() }
        };
        let (mi, ma, mb) = (measure(&input), measure(&a), measure(&b));
        // vyrovnání vnímané hlasitosti náhledu (B na úroveň A)
        let gain = 10f32.powf(((ma.integrated_lufs - mb.integrated_lufs) as f32) / 20.0);
        let b_matched: Vec<f32> = b.iter().map(|x| x * gain).collect();
        let orig_gain = 10f32.powf(((ma.integrated_lufs - mi.integrated_lufs) as f32) / 20.0);
        let orig_matched: Vec<f32> = input.iter().map(|x| x * orig_gain).collect();
        let write = |name: &str, x: &[f32]| -> Result<String> {
            let f = format!("{token}_{name}.wav");
            let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
            let mut w = hound::WavWriter::create(dir.join(&f), spec)?;
            for v in x { w.write_sample((v.clamp(-1.0, 1.0) * 32767.0) as i16)?; }
            w.finalize()?;
            Ok(f)
        };
        // úklid starších náhledů
        if let Ok(rd) = std::fs::read_dir(&dir) { for e in rd.flatten() { if e.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(3600)).unwrap_or(false) { let _ = std::fs::remove_file(e.path()); } } }
        Ok(PreviewResult {
            a: write("a", &a)?, b: write("b", &b)?, b_matched: write("b_matched", &b_matched)?, original: write("orig", &input)?, original_matched: write("orig_matched", &orig_matched)?,
            input: mi, a_measure: ma, b_measure: mb, match_gain_db: 20.0 * gain.log10(), seconds: input.len() as f32 / 2.0 / rate as f32,
        })
    }

    pub fn preview_path(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') || name.contains('\\') || name.contains("..") || !name.ends_with(".wav") { return None; }
        let p = self.data_dir.join("Processing").join("preview").join(name);
        p.is_file().then_some(p)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewReq {
    pub a: Revision,
    pub b: Revision,
    /// Výstupní větev pro náhled (target)
    #[serde(default)]
    pub branch: String,
    pub track_id: Option<i64>,
    pub start_s: Option<f32>,
    #[serde(default = "d20")]
    pub seconds: f32,
}
fn d20() -> f32 { 20.0 }

#[derive(Debug, Clone, Serialize, Default)]
pub struct Measure { pub integrated_lufs: f64, pub lra_lu: f64, pub true_peak_dbtp: f64, pub sample_peak_dbfs: f64 }

#[derive(Debug, Clone, Serialize)]
pub struct PreviewResult {
    pub a: String, pub b: String, pub b_matched: String, pub original: String, pub original_matched: String,
    pub input: Measure, pub a_measure: Measure, pub b_measure: Measure, pub match_gain_db: f32, pub seconds: f32,
}
