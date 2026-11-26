//! Odesílání „co hraje“ na další místa (Jazler Internet Updater – služby):
//! FTP / FTPS / SFTP (nahrání exportovaných souborů na web), TuneIn (AIR API), HTTP GET/POST,
//! TCP a UDP (text podle šablony – RDS kodéry, studiové displeje, vlastní systémy).
//! Odesílá se ve vlastním vlákně, aby pomalý server nikdy nezdržel vysílání.

use crate::model::Track;
use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NpService {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// ftp | ftps | sftp | tunein | http | tcp | udp
    pub kind: String,
    pub host: String,
    /// 0 = výchozí (FTP 21, SFTP 22)
    pub port: u16,
    pub user: String,
    pub password: String,
    /// cílová složka na serveru (FTP/SFTP)
    pub remote_dir: String,
    /// které exportované soubory nahrát (FTP/SFTP); prázdné = NowOnAir.txt, NowOnAir.xml, NowOnAir.json, AirPlayHistory.xml, CurrentAlbumImage.jpg
    pub files: Vec<String>,
    /// FTP: pasivní režim (doporučeno)
    pub passive: bool,
    /// HTTP: adresa (může obsahovat {artist} … – hodnoty se zakódují)
    pub url: String,
    /// HTTP: GET | POST
    pub method: String,
    /// text pro HTTP POST / TCP / UDP, např. "{artist} - {title}"
    pub template: String,
    /// TCP/UDP: přidat konec řádku
    pub newline: bool,
    /// TuneIn AIR API
    pub tunein_partner_id: String,
    pub tunein_partner_key: String,
    pub tunein_station_id: String,
    /// posílat jen skladby (ne jingly/reklamy, i když mají vlastní text)
    pub only_songs: bool,
}

impl Default for NpService {
    fn default() -> Self {
        Self {
            id: String::new(), name: String::new(), enabled: true, kind: "ftp".into(), host: String::new(), port: 0, user: String::new(), password: String::new(),
            remote_dir: String::new(), files: vec![], passive: true, url: String::new(), method: "GET".into(), template: "{artist} - {title}".into(), newline: true,
            tunein_partner_id: String::new(), tunein_partner_key: String::new(), tunein_station_id: String::new(), only_songs: false,
        }
    }
}

pub const DEFAULT_FILES: [&str; 5] = ["NowOnAir.txt", "NowOnAir.xml", "NowOnAir.json", "AirPlayHistory.xml", "CurrentAlbumImage.jpg"];

#[derive(Debug, Clone, Serialize, Default)]
pub struct ServiceStatus { pub ok: bool, pub at: String, pub message: String, pub sent: u64, pub errors: u64 }

#[derive(Clone)]
pub struct Job { pub services: Vec<NpService>, pub track: Track, pub text: String, pub station: String, pub dir: PathBuf }

static STATUS: OnceLock<Mutex<HashMap<String, ServiceStatus>>> = OnceLock::new();
static TX: OnceLock<crossbeam_channel::Sender<Job>> = OnceLock::new();

fn status_map() -> &'static Mutex<HashMap<String, ServiceStatus>> { STATUS.get_or_init(|| Mutex::new(HashMap::new())) }
pub fn statuses() -> HashMap<String, ServiceStatus> { status_map().lock().clone() }

/// Zařadí odeslání (nové volání nahradí čekající – vždy se pošle aktuální skladba).
pub fn trigger(job: Job) {
    if job.services.iter().all(|s| !s.enabled) { return; }
    let tx = TX.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::bounded::<Job>(8);
        std::thread::Builder::new().name("orca-np-services".into()).spawn(move || {
            while let Ok(mut job) = rx.recv() {
                while let Ok(newer) = rx.try_recv() { job = newer; }
                for s in job.services.iter().filter(|s| s.enabled) {
                    let r = send(s, &job);
                    record(&s.id, &r);
                    if let Err(e) = r { log::warn!("služba {} ({}): {e:#}", s.name, s.kind); }
                }
            }
        }).ok();
        tx
    });
    let _ = tx.try_send(job);
}

fn record(id: &str, r: &Result<String>) {
    let mut m = status_map().lock();
    let e = m.entry(id.to_string()).or_default();
    e.at = chrono::Local::now().to_rfc3339();
    match r { Ok(msg) => { e.ok = true; e.message = msg.clone(); e.sent += 1; } Err(err) => { e.ok = false; e.message = format!("{err:#}"); e.errors += 1; } }
}

/// Test jedné služby hned (pro tlačítko Vyzkoušet).
pub fn test(s: &NpService, job: &Job) -> Result<String> {
    let r = send(s, job);
    record(&s.id, &r);
    r
}

pub fn fill(tpl: &str, t: &Track, text: &str, station: &str, url: bool) -> String {
    let enc = |v: &str| if url { urlencoding::encode(v).into_owned() } else { v.to_string() };
    tpl.replace("{artist}", &enc(&t.artist)).replace("{title}", &enc(&t.title)).replace("{album}", &enc(&t.album))
        .replace("{text}", &enc(text)).replace("{kind}", t.kind().as_str()).replace("{category}", &enc(&t.category))
        .replace("{duration_s}", &(t.duration_ms / 1000).to_string()).replace("{duration}", &format!("{}:{:02}", t.duration_ms / 60000, (t.duration_ms / 1000) % 60))
        .replace("{isrc}", &enc(&t.isrc)).replace("{year}", &t.year.map(|y| y.to_string()).unwrap_or_default())
        .replace("{station}", &enc(station)).replace("{time}", &chrono::Local::now().format("%H:%M:%S").to_string())
}

fn send(s: &NpService, j: &Job) -> Result<String> {
    if s.only_songs && j.track.kind() != crate::model::TrackKind::Song { return Ok("přeskočeno (není skladba)".into()); }
    match s.kind.as_str() {
        "ftp" | "ftps" => ftp_upload(s, &files_of(s, &j.dir)),
        "sftp" => sftp_upload(s, &files_of(s, &j.dir)),
        "tunein" => tunein(s, j),
        "http" => http(s, j),
        "tcp" => {
            let addr = format!("{}:{}", s.host.trim(), s.port);
            let mut c = std::net::TcpStream::connect_timeout(&resolve(&addr)?, Duration::from_secs(5)).with_context(|| format!("spojení s {addr}"))?;
            c.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut msg = fill(&s.template, &j.track, &j.text, &j.station, false); if s.newline { msg.push_str("\r\n"); }
            c.write_all(msg.as_bytes())?;
            Ok(format!("odesláno {} B na {addr}", msg.len()))
        }
        "udp" => {
            let addr = format!("{}:{}", s.host.trim(), s.port);
            let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
            let _ = sock.set_broadcast(true);
            let mut msg = fill(&s.template, &j.track, &j.text, &j.station, false); if s.newline { msg.push_str("\r\n"); }
            sock.send_to(msg.as_bytes(), resolve(&addr)?)?;
            Ok(format!("odesláno {} B na {addr}", msg.len()))
        }
        k => Err(anyhow!("neznámý typ služby {k}")),
    }
}

fn resolve(addr: &str) -> Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs().with_context(|| format!("adresa {addr}"))?.next().ok_or_else(|| anyhow!("adresu {addr} nelze najít"))
}

fn files_of(s: &NpService, dir: &Path) -> Vec<PathBuf> {
    let names: Vec<String> = if s.files.is_empty() { DEFAULT_FILES.iter().map(|x| x.to_string()).collect() } else { s.files.clone() };
    names.iter().map(|n| dir.join(n)).filter(|p| p.is_file()).collect()
}

/// Nahraje soubory přes FTP/FTPS/SFTP službu (i pro podcast).
pub fn upload_files(s: &NpService, files: &[PathBuf]) -> Result<String> {
    match s.kind.as_str() { "ftp" | "ftps" => ftp_upload(s, files), "sftp" => sftp_upload(s, files), k => Err(anyhow!("služba typu {k} neumí nahrávat soubory – použijte FTP, FTPS nebo SFTP")) }
}

fn ftp_upload(s: &NpService, files: &[PathBuf]) -> Result<String> {
    use suppaftp::types::FileType;
    use suppaftp::Mode;
    let port = if s.port == 0 { 21 } else { s.port };
    let addr = resolve(&format!("{}:{port}", s.host.trim()))?;
    let files = files.to_vec();
    if files.is_empty() { return Err(anyhow!("žádný soubor k nahrání – zapněte exporty „co hraje“")); }
    macro_rules! go { ($ftp:expr) => {{
        let mut ftp = $ftp;
        ftp.set_mode(if s.passive { Mode::Passive } else { Mode::Active });
        ftp.login(s.user.as_str(), s.password.as_str()).map_err(|e| anyhow!("přihlášení: {e}"))?;
        ftp.transfer_type(FileType::Binary)?;
        if !s.remote_dir.trim().is_empty() { ftp.cwd(s.remote_dir.trim()).map_err(|e| anyhow!("složka {}: {e}", s.remote_dir))?; }
        let mut n = 0;
        for p in &files {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            let tmp = format!("{name}.part");
            let mut f = std::fs::File::open(p)?;
            ftp.put_file(tmp.as_str(), &mut f).map_err(|e| anyhow!("nahrání {name}: {e}"))?;
            // přejmenování až po nahrání – web nikdy nevidí poloviční soubor
            let _ = ftp.rm(name.as_str());
            ftp.rename(tmp.as_str(), name.as_str()).map_err(|e| anyhow!("přejmenování {name}: {e}"))?;
            n += 1;
        }
        let _ = ftp.quit();
        Ok(format!("nahráno souborů: {n}"))
    }}}
    if s.kind == "ftps" {
        let roots = suppaftp::rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let cfg = suppaftp::rustls::ClientConfig::builder_with_provider(Arc::new(suppaftp::rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().map_err(|e| anyhow!("TLS: {e}"))?
            .with_root_certificates(roots).with_no_client_auth();
        let ftp = suppaftp::RustlsFtpStream::connect_timeout(addr, Duration::from_secs(10)).map_err(|e| anyhow!("spojení: {e}"))?
            .into_secure(suppaftp::RustlsConnector::from(Arc::new(cfg)), s.host.trim()).map_err(|e| anyhow!("TLS (FTPS): {e}"))?;
        go!(ftp)
    } else {
        let ftp = suppaftp::FtpStream::connect_timeout(addr, Duration::from_secs(10)).map_err(|e| anyhow!("spojení: {e}"))?;
        go!(ftp)
    }
}

struct SshH { known: PathBuf, host: String }
impl russh::client::Handler for SshH {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // TOFU: první připojení klíč serveru uloží, příště ho vyžaduje (ochrana proti podvrženému serveru)
        let fp = match key { russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(russh::keys::HashAlg::Sha256).to_string(), _ => format!("{key:?}") };
        let text = std::fs::read_to_string(&self.known).unwrap_or_default();
        for line in text.lines() {
            if let Some((h, f)) = line.split_once(' ') {
                if h == self.host { if f == fp { return Ok(true); } return Err(anyhow!("klíč serveru {} se změnil (nový {fp}) – pokud je to v pořádku, smažte řádek v souboru {}", self.host, self.known.display())); }
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.known) { let _ = writeln!(f, "{} {fp}", self.host); }
        Ok(true)
    }
}

fn sftp_upload(s: &NpService, files: &[PathBuf]) -> Result<String> {
    let files = files.to_vec();
    if files.is_empty() { return Err(anyhow!("žádný soubor k nahrání – zapněte exporty „co hraje“")); }
    let port = if s.port == 0 { 22 } else { s.port };
    let known = crate::countdown::data_dir().unwrap_or_else(std::env::temp_dir).join("sftp_known_hosts.txt");
    let host = s.host.trim().to_string();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        use tokio::io::AsyncWriteExt;
        let cfg = Arc::new(russh::client::Config { inactivity_timeout: Some(Duration::from_secs(20)), ..Default::default() });
        let mut sess = tokio::time::timeout(Duration::from_secs(10), russh::client::connect(cfg, (host.as_str(), port), SshH { known, host: format!("{host}:{port}") }))
            .await.map_err(|_| anyhow!("spojení vypršelo"))?.map_err(|e| anyhow!("spojení: {e:#}"))?;
        let auth = sess.authenticate_password(s.user.clone(), s.password.clone()).await.map_err(|e| anyhow!("přihlášení: {e}"))?;
        if !matches!(auth, russh::client::AuthResult::Success) { return Err(anyhow!("přihlášení odmítnuto – zkontrolujte jméno a heslo")); }
        let ch = sess.channel_open_session().await?;
        ch.request_subsystem(true, "sftp").await?;
        let sftp = russh_sftp::client::SftpSession::new(ch.into_stream()).await.map_err(|e| anyhow!("SFTP: {e}"))?;
        let base = s.remote_dir.trim().trim_end_matches('/').to_string();
        let mut n = 0;
        for p in &files {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            let target = if base.is_empty() { name.clone() } else { format!("{base}/{name}") };
            let tmp = format!("{target}.part");
            let data = std::fs::read(p)?;
            let mut f = sftp.create(tmp.clone()).await.map_err(|e| anyhow!("zápis {target}: {e}"))?;
            f.write_all(&data).await?; f.shutdown().await?;
            let _ = sftp.remove_file(target.clone()).await;
            sftp.rename(tmp, target.clone()).await.map_err(|e| anyhow!("přejmenování {target}: {e}"))?;
            n += 1;
        }
        let _ = sftp.close().await;
        Ok(format!("nahráno souborů: {n} (SFTP)"))
    })
}

fn tunein(s: &NpService, j: &Job) -> Result<String> {
    if s.tunein_partner_id.is_empty() || s.tunein_partner_key.is_empty() || s.tunein_station_id.is_empty() { return Err(anyhow!("vyplňte Partner ID, Partner Key a Station ID od TuneIn")); }
    let mut url = format!("https://air.radiotime.com/Playing.ashx?partnerId={}&partnerKey={}&id={}", urlencoding::encode(&s.tunein_partner_id), urlencoding::encode(&s.tunein_partner_key), urlencoding::encode(&s.tunein_station_id));
    if j.track.kind() == crate::model::TrackKind::Song {
        url += &format!("&title={}&artist={}", urlencoding::encode(&j.track.title), urlencoding::encode(&j.track.artist));
        if !j.track.album.is_empty() { url += &format!("&album={}", urlencoding::encode(&j.track.album)); }
    } else if j.track.kind() == crate::model::TrackKind::Spot {
        url += "&commercial=true";
    } else {
        url += &format!("&title={}", urlencoding::encode(&j.text));
    }
    let r = ureq::get(&url).timeout(Duration::from_secs(8)).call().map_err(|e| anyhow!("TuneIn: {e}"))?;
    Ok(format!("TuneIn odpověděl {}", r.status()))
}

fn http(s: &NpService, j: &Job) -> Result<String> {
    let url = fill(s.url.trim(), &j.track, &j.text, &j.station, true);
    if url.is_empty() { return Err(anyhow!("chybí adresa")); }
    let r = if s.method.eq_ignore_ascii_case("POST") {
        let body = fill(&s.template, &j.track, &j.text, &j.station, false);
        let req = ureq::post(&url).timeout(Duration::from_secs(8));
        if body.trim_start().starts_with('{') { req.set("Content-Type", "application/json").send_string(&body) } else { req.set("Content-Type", "text/plain; charset=utf-8").send_string(&body) }
    } else { ureq::get(&url).timeout(Duration::from_secs(8)).call() };
    let r = r.map_err(|e| anyhow!("HTTP: {e}"))?;
    Ok(format!("HTTP {}", r.status()))
}
