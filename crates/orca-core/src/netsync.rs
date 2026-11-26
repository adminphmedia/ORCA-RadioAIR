//! Synchronizace reklam v síti stanic.
//! **Centrála** (hub) vysílá program, který regionální stanice přebírají (Rebroadcast). Když centrála
//! začne reklamní blok, pošle všem regionům signál (UDP, případně HTTP). **Regionální stanice** v tu
//! chvíli ztlumí převzatý program, odvysílá svůj místní reklamní blok, zbytek času doplní (promo, jingly)
//! a vrátí se zpět na program centrály – posluchač nic nepozná.
use crate::events::Event;
use crate::model::{QueueItem, Track};
use crate::Orca;
use anyhow::{anyhow, bail, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NetSyncConfig {
    /// off | hub | regional
    pub role: String,
    /// Společný klíč sítě
    pub key: String,
    /// Centrála: kam posílat signály (host:port, jeden na řádek)
    pub targets: Vec<String>,
    /// Region: kde poslouchat (UDP)
    pub listen: String,
    /// Region: reagovat jen při přebírání programu (Rebroadcast)
    pub only_during_relay: bool,
    /// Region: místní blok = nejbližší reklamní blok z Reklamních bloků (± okno min); jinak jen výplň
    pub use_break_schedule: bool,
    pub window_min: u32,
    /// Region: výplň zbývajícího času (kategorie knihovny, např. Promo)
    pub fill_category: String,
    /// Region: jingle na začátek / konec místního bloku (id skladby, 0 = žádný)
    pub intro_track: i64,
    pub outro_track: i64,
    /// Region: na signál konce bloku z centrály ukončit výplň a vrátit se na program centrály
    pub return_on_end: bool,
    /// Region: posunout návrat o N ms (zpoždění streamu centrály, typicky 2000–10000 ms)
    pub delay_ms: u32,
}
impl Default for NetSyncConfig {
    fn default() -> Self {
        Self { role: "off".into(), key: String::new(), targets: vec![], listen: "0.0.0.0:8790".into(), only_during_relay: true, use_break_schedule: true, window_min: 15, fill_category: String::new(), intro_track: 0, outro_track: 0, return_on_end: true, delay_ms: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Signal {
    #[serde(default)]
    pub orca: String,
    #[serde(default)]
    pub key: String,
    /// break_start | break_end | test
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub len_ms: i64,
    #[serde(default)]
    pub station: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct NetSyncStatus {
    pub role: String,
    pub listening: Option<String>,
    pub active: Option<String>,
    pub sent: u64,
    pub received: u64,
    pub log: Vec<String>,
}

#[derive(Default)]
pub struct NetSync {
    pub status: RwLock<NetSyncStatus>,
    active_until: RwLock<Option<Instant>>,
}

impl NetSync {
    fn log(&self, s: impl Into<String>) {
        let s = format!("{} {}", chrono::Local::now().format("%d.%m. %H:%M:%S"), s.into());
        log::info!("síť stanic: {s}");
        let mut st = self.status.write();
        st.log.insert(0, s);
        st.log.truncate(80);
    }
}

/// Centrála: pošle signál všem regionům.
pub fn send(o: &Orca, kind: &str, id: &str, len_ms: i64) -> Result<usize> {
    let c = o.cfg.read().clone();
    let n = &c.netsync;
    if n.targets.iter().all(|t| t.trim().is_empty()) { bail!("nejsou zadané adresy regionálních stanic"); }
    let msg = serde_json::to_vec(&Signal { orca: "netsync".into(), key: n.key.clone(), kind: kind.into(), id: id.into(), len_ms, station: c.meta.station_long.clone() })?;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let mut ok = 0;
    for t in n.targets.iter().map(|x| x.trim()).filter(|x| !x.is_empty()) {
        let addr = if t.contains(':') { t.to_string() } else { format!("{t}:8790") };
        // UDP: pro jistotu 3× (regiony duplicitu poznají podle id)
        let mut sent = false;
        for _ in 0..3 { if sock.send_to(&msg, &addr).is_ok() { sent = true; } std::thread::sleep(Duration::from_millis(30)); }
        if sent { ok += 1; } else { o.netsync.log(format!("signál pro {addr} se nepodařilo odeslat")); }
    }
    o.netsync.status.write().sent += 1;
    o.netsync.log(format!("odeslán signál {} ({} s) → {ok} stanic", label(kind), len_ms / 1000));
    Ok(ok)
}

fn label(k: &str) -> &str { match k { "break_start" => "ZAČÁTEK BLOKU", "break_end" => "KONEC BLOKU", "test" => "ZKOUŠKA", x => x } }

/// Spustí hlídání (centrála: sleduje začátky bloků; region: poslouchá signály).
pub fn start(o: Arc<Orca>) {
    // centrála
    {
        let o = o.clone();
        let rx = o.events.subscribe();
        std::thread::Builder::new().name("orca-netsync-hub".into()).spawn(move || {
            let mut in_break: Option<String> = None;
            while let Ok(ev) = rx.recv() {
                let Event::TrackStarted { item, channel } = ev else { continue };
                if channel != "main" { continue; }
                if o.cfg.read().netsync.role != "hub" { in_break = None; continue; }
                match &item.break_ref {
                    Some(b) if in_break.as_deref() != Some(b.as_str()) => {
                        // délka bloku = hrající + následující položky stejného bloku
                        let rest: i64 = o.engine.queue_snapshot().iter().take_while(|q| q.break_ref.as_deref() == Some(b.as_str())).map(|q| play_len(&q.track)).sum();
                        let len = play_len(&item.track) + rest;
                        in_break = Some(b.clone());
                        if let Err(e) = send(&o, "break_start", b, len) { o.netsync.log(format!("chyba: {e}")); }
                    }
                    Some(_) => {}
                    None => if let Some(b) = in_break.take() { let _ = send(&o, "break_end", &b, 0); },
                }
            }
        }).ok();
    }
    // region
    std::thread::Builder::new().name("orca-netsync-rx".into()).spawn(move || {
        let mut bound: Option<(String, UdpSocket)> = None;
        let mut seen: Vec<(String, String, Instant)> = vec![];
        loop {
            let c = o.cfg.read().netsync.clone();
            let want = (c.role == "regional").then(|| c.listen.clone());
            if want.as_deref() != bound.as_ref().map(|b| b.0.as_str()) {
                bound = None;
                o.netsync.status.write().listening = None;
                if let Some(addr) = want {
                    match UdpSocket::bind(&addr) {
                        Ok(s) => { let _ = s.set_read_timeout(Some(Duration::from_millis(500))); o.netsync.status.write().listening = Some(addr.clone()); o.netsync.log(format!("poslouchám signály na {addr}")); bound = Some((addr, s)); }
                        Err(e) => { o.events.alert("error", "netsync_port", format!("Síť stanic: port {addr} nelze otevřít ({e})")); std::thread::sleep(Duration::from_secs(10)); }
                    }
                }
            }
            o.netsync.status.write().role = c.role.clone();
            let Some((_, s)) = &bound else { std::thread::sleep(Duration::from_secs(2)); continue };
            let mut buf = [0u8; 2048];
            let Ok((n, from)) = s.recv_from(&mut buf) else { continue };
            let Some(sig) = parse(&buf[..n]) else { o.netsync.log(format!("neznámý signál od {from}")); continue };
            if !c.key.is_empty() && sig.key != c.key { o.netsync.log(format!("signál od {from} se špatným klíčem – ignoruji")); continue; }
            seen.retain(|x| x.2.elapsed() < Duration::from_secs(30));
            if seen.iter().any(|x| x.0 == sig.kind && x.1 == sig.id) { continue; }
            seen.push((sig.kind.clone(), sig.id.clone(), Instant::now()));
            o.netsync.status.write().received += 1;
            handle(&o, &sig, &from.to_string());
        }
    }).ok();
}

/// Signál z ORCA (JSON) nebo jednoduchý text z jiného systému: `BREAK_START 120` / `BREAK_END`.
pub fn parse(b: &[u8]) -> Option<Signal> {
    if let Ok(s) = serde_json::from_slice::<Signal>(b) { return Some(s); }
    let t = String::from_utf8_lossy(b);
    let mut it = t.split_whitespace();
    let k = it.next()?.to_lowercase();
    let kind = match k.as_str() { "break_start" | "start" => "break_start", "break_end" | "end" | "stop" => "break_end", "test" => "test", _ => return None };
    let len = it.next().and_then(|x| x.parse::<f64>().ok()).map(|s| (s * 1000.0) as i64).unwrap_or(0);
    let key = it.next().unwrap_or("").to_string();
    Some(Signal { orca: String::new(), key, kind: kind.into(), id: format!("txt-{}", chrono::Local::now().format("%H%M%S")), len_ms: len, station: String::new() })
}

fn play_len(t: &Track) -> i64 { let end = if t.cue_out_ms > 0 { t.cue_out_ms } else { t.duration_ms }; (end - t.cue_in_ms).max(0) }

pub fn handle(o: &Arc<Orca>, sig: &Signal, from: &str) {
    let who = if sig.station.is_empty() { from.to_string() } else { format!("{} ({from})", sig.station) };
    match sig.kind.as_str() {
        "test" => o.netsync.log(format!("zkušební signál od {who} – spojení funguje")),
        "break_start" => match local_break(o, sig.len_ms) {
            Ok(m) => { o.netsync.log(format!("ZAČÁTEK BLOKU od {who}: {m}")); o.events.alert("info", "netsync_break", format!("Síť stanic: místní reklamní blok – {m}")); }
            Err(e) => o.netsync.log(format!("začátek bloku od {who}: {e}")),
        },
        "break_end" => {
            let active = o.netsync.active_until.read().is_some();
            if !active { return; }
            let c = o.cfg.read().netsync.clone();
            if c.return_on_end {
                // zrušit zbývající výplň, návrat na program centrály
                let q = o.engine.queue_snapshot();
                let mut removed = 0;
                for it in q.iter().take_while(|x| x.relay.is_none()) { if it.source == "netsync_fill" && o.engine.queue_remove(&it.id) { removed += 1; } }
                o.netsync.log(format!("KONEC BLOKU od {who}{}", if removed > 0 { format!(" – zrušeno {removed} položek výplně") } else { String::new() }));
            } else { o.netsync.log(format!("KONEC BLOKU od {who}")); }
            *o.netsync.active_until.write() = None;
            o.netsync.status.write().active = None;
        }
        _ => {}
    }
}

/// Region: místní blok místo převzatého programu.
pub fn local_break(o: &Arc<Orca>, hub_len_ms: i64) -> Result<String> {
    let c = o.cfg.read().clone();
    let n = &c.netsync;
    if let Some(t) = *o.netsync.active_until.read() { if Instant::now() < t { bail!("místní blok už běží"); } }
    let st = o.engine.snapshot();
    let relay_item = st.current.as_ref().filter(|x| x.item.relay.is_some()).map(|x| (x.item.clone(), x.remaining_ms));
    if n.only_during_relay && relay_item.is_none() { bail!("ignoruji – stanice právě nepřebírá program centrály"); }
    let mut items: Vec<QueueItem> = vec![];
    let reason = "Síť stanic: místní blok místo reklamy centrály".to_string();
    let push_id = |items: &mut Vec<QueueItem>, id: i64, src: &str| { if id > 0 { if let Ok(Some(t)) = o.db.get_track(id) { items.push(QueueItem::new(t, src, reason.clone())); } } };
    push_id(&mut items, n.intro_track, "netsync");
    let mut what = vec![];
    // místní reklamní blok podle plánu bloků
    if n.use_break_schedule {
        let now = crate::automation::now_local(&c);
        let best = c.spots.breaks.iter().filter_map(|d| {
            let t = chrono::NaiveTime::parse_from_str(&d.time, "%H:%M").ok()?;
            let dt = now.date_naive().and_time(t).and_local_timezone(chrono::Local).single()?;
            let diff = (dt - now).num_minutes().abs();
            (diff <= n.window_min as i64).then_some((diff, d.clone()))
        }).min_by_key(|x| x.0);
        if let Some((_, def)) = best {
            let blk = crate::spots::build_block(&o.db, &c, now.date_naive(), &def, true)?;
            if !blk.items.is_empty() {
                what.push(format!("blok {} ({} spotů)", def.time, blk.items.len()));
                for bi in &blk.items {
                    let mut q = QueueItem::new(bi.track.clone(), "break", format!("Síť stanic • místní blok {} • {}", blk.time, bi.campaign));
                    q.break_ref = Some(crate::spots::break_key(&blk.date, &blk.time));
                    items.push(q);
                }
                let _ = crate::spots::mark_inserted(&o.db, &blk.date, &blk.time);
            }
        }
    }
    push_id(&mut items, n.outro_track, "netsync");
    // výplň do délky bloku centrály
    let mut total: i64 = items.iter().map(|x| play_len(&x.track)).sum();
    let target = hub_len_ms + n.delay_ms as i64;
    if target > total && !n.fill_category.is_empty() {
        let fills = fill_tracks(o, &n.fill_category, target - total)?;
        if !fills.is_empty() { what.push(format!("výplň {} × {}", fills.len(), n.fill_category)); }
        for t in fills { total += play_len(&t); items.push(QueueItem::new(t, "netsync_fill", "Síť stanic: výplň do délky bloku centrály")); }
    }
    if items.is_empty() { bail!("není co vysílat – nastavte místní reklamní bloky, výplň nebo jingly"); }
    // zpět na program centrály
    // zpět na program centrály (na zbývající čas převzetí)
    if let Some((r, remaining)) = relay_item {
        let left = remaining - total;
        if left > 5000 {
            let mut back = r.clone();
            back.id = uuid::Uuid::new_v4().to_string(); back.reason = "Síť stanic: návrat na program centrály".into(); back.eta = None;
            back.track.cue_in_ms = 0; back.track.duration_ms = left; back.track.cue_out_ms = left; back.track.mix_ms = left; back.track.intro_ms = 0;
            items.push(back);
        }
    }
    let count = items.len();
    for (i, it) in items.into_iter().enumerate() { o.engine.queue_push(it, Some(i)); }
    o.engine.send(crate::engine::Cmd::PlayNow);
    *o.netsync.active_until.write() = Some(Instant::now() + Duration::from_millis(total.max(5000) as u64 + 5000));
    o.netsync.status.write().active = Some(format!("{} – {} s", what.join(", "), total / 1000));
    Ok(format!("{} ({count} položek, {} s; blok centrály {} s)", if what.is_empty() { "jingly".into() } else { what.join(", ") }, total / 1000, hub_len_ms / 1000))
}

fn fill_tracks(o: &Orca, category: &str, mut remaining_ms: i64) -> Result<Vec<Track>> {
    let ids: Vec<i64> = o.db.with_conn(|c| -> Result<Vec<i64>> {
        let mut st = c.prepare("SELECT id FROM tracks WHERE enabled=1 AND missing=0 AND (category=?1 OR category2=?1 OR category3=?1) ORDER BY random() LIMIT 200")?;
        let v = st.query_map([category], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })?;
    if ids.is_empty() { return Err(anyhow!("kategorie výplně „{category}“ je prázdná")); }
    let mut out = vec![];
    for id in ids {
        if remaining_ms < 3000 { break; }
        if let Ok(Some(t)) = o.db.get_track(id) { let l = play_len(&t); if l > 0 && l <= remaining_ms + 1500 { remaining_ms -= l; out.push(t); } }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_text() {
        let s = parse(b"BREAK_START 90 tajne").unwrap();
        assert_eq!((s.kind.as_str(), s.len_ms, s.key.as_str()), ("break_start", 90000, "tajne"));
        assert_eq!(parse(b"end").unwrap().kind, "break_end");
        assert!(parse(b"nesmysl").is_none());
        let j = parse(br#"{"orca":"netsync","key":"k","type":"break_start","id":"x","len_ms":5}"#).unwrap();
        assert_eq!(j.len_ms, 5);
    }
}
