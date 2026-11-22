//! RDS (FM): PS, RadioText, RT+, PI, PTY, TP/TA, MS, DI.
//! Výstup na RDS kodéry přes UECP (EBU SPB 490) nebo textové příkazy (šablony), přes TCP / UDP / sériový port / soubor.

use crate::config::{RdsConfig, RdsOutput};
use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Data k odeslání.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct RdsData {
    pub pi: String,
    pub ps: String,
    pub rt: String,
    pub pty: u8,
    pub tp: bool,
    pub ta: bool,
    pub ms_music: bool,
    pub di_stereo: bool,
    /// RT+ (typ, začátek, délka) – max 2
    pub rtplus: Vec<(u8, usize, usize)>,
    pub rt_ab: bool,
}

/// Přepis do ASCII – bezpečné pro RDS přijímače i kodéry bez podpory znakové sady.
pub fn ascii(s: &str) -> String {
    s.chars().filter(|c| *c != '°').map(|c| match c {
        'á' | 'à' | 'â' | 'ä' | 'ą' | 'å' | 'ã' => 'a', 'Á' | 'À' | 'Â' | 'Ä' | 'Ą' | 'Å' | 'Ã' => 'A',
        'č' | 'ć' | 'ç' => 'c', 'Č' | 'Ć' | 'Ç' => 'C', 'ď' => 'd', 'Ď' => 'D',
        'é' | 'è' | 'ê' | 'ë' | 'ě' | 'ę' => 'e', 'É' | 'È' | 'Ê' | 'Ë' | 'Ě' | 'Ę' => 'E',
        'í' | 'ì' | 'î' | 'ï' => 'i', 'Í' | 'Ì' | 'Î' | 'Ï' => 'I', 'ľ' | 'ĺ' | 'ł' => 'l', 'Ľ' | 'Ĺ' | 'Ł' => 'L',
        'ň' | 'ń' | 'ñ' => 'n', 'Ň' | 'Ń' | 'Ñ' => 'N', 'ó' | 'ò' | 'ô' | 'ö' | 'ő' | 'õ' => 'o', 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Ő' | 'Õ' => 'O',
        'ř' | 'ŕ' => 'r', 'Ř' | 'Ŕ' => 'R', 'š' | 'ś' | 'ß' => 's', 'Š' | 'Ś' => 'S', 'ť' => 't', 'Ť' => 'T',
        'ú' | 'ù' | 'û' | 'ü' | 'ů' | 'ű' => 'u', 'Ú' | 'Ù' | 'Û' | 'Ü' | 'Ů' | 'Ű' => 'U', 'ý' | 'ÿ' => 'y', 'Ý' => 'Y',
        'ž' | 'ź' | 'ż' => 'z', 'Ž' | 'Ź' | 'Ż' => 'Z', '–' | '—' => '-', '„' | '“' | '”' => '"', '‚' | '‘' | '’' => '\'', '…' => '.',
        c if c.is_ascii() && !c.is_ascii_control() => c,
        _ => ' ',
    }).collect()
}

/// PS = 8 znaků (zarovnání na střed).
pub fn ps8(s: &str) -> String {
    let t: String = s.chars().take(8).collect();
    let n = t.chars().count();
    let left = (8 - n) / 2;
    format!("{}{}{}", " ".repeat(left), t, " ".repeat(8 - n - left))
}

/// Rozdělení textu na 8znakové kousky (dynamické PS).
pub fn ps_chunks(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for w in s.split_whitespace() {
        let w: String = w.chars().take(8).collect();
        if cur.is_empty() { cur = w; } else if cur.chars().count() + 1 + w.chars().count() <= 8 { cur = format!("{cur} {w}"); } else { out.push(ps8(&cur)); cur = w; }
    }
    if !cur.is_empty() { out.push(ps8(&cur)); }
    out
}

// ---------------------------------------------------------------------------------------------
// UECP

fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for b in data {
        crc ^= (*b as u16) << 8;
        for _ in 0..8 { crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 }; }
    }
    !crc
}

/// Rámec UECP: STA, ADD(2), SQC, MFL, MSG, CRC(2), STP – s byte stuffingem.
pub fn uecp_frame(site: u16, encoder: u8, seq: u8, msg: &[u8]) -> Vec<u8> {
    let addr: u16 = ((site & 0x3FF) << 6) | (encoder as u16 & 0x3F);
    let mut body = vec![(addr >> 8) as u8, (addr & 0xFF) as u8, seq, msg.len() as u8];
    body.extend_from_slice(msg);
    let crc = crc16_ccitt(&body);
    body.push((crc >> 8) as u8);
    body.push((crc & 0xFF) as u8);
    let mut out = vec![0xFE];
    for b in body {
        match b { 0xFD => out.extend([0xFD, 0x00]), 0xFE => out.extend([0xFD, 0x01]), 0xFF => out.extend([0xFD, 0x02]), x => out.push(x) }
    }
    out.push(0xFF);
    out
}

pub fn uecp_messages(d: &RdsData) -> Vec<Vec<u8>> {
    let mut v = vec![];
    let pi = u16::from_str_radix(d.pi.trim(), 16).unwrap_or(0);
    v.push(vec![0x01, 0x00, 0x00, (pi >> 8) as u8, (pi & 0xFF) as u8]);
    let mut ps = vec![0x02, 0x00, 0x00];
    ps.extend(ps8(&ascii(&d.ps)).bytes().take(8));
    v.push(ps);
    v.push(vec![0x03, 0x00, 0x00, ((d.tp as u8) << 1) | d.ta as u8]);
    v.push(vec![0x04, 0x00, 0x00, d.di_stereo as u8]);
    v.push(vec![0x05, 0x00, 0x00, d.ms_music as u8]);
    v.push(vec![0x07, 0x00, 0x00, d.pty & 0x1F]);
    let rt: Vec<u8> = ascii(&d.rt).bytes().take(64).collect();
    let mut m = vec![0x0A, 0x00, 0x00, (rt.len() + 1) as u8, d.rt_ab as u8];
    m.extend(rt);
    v.push(m);
    v
}

// ---------------------------------------------------------------------------------------------
// Textové příkazy

pub fn unescape(s: &str) -> String { s.replace("\\r", "\r").replace("\\n", "\n").replace("\\t", "\t") }

pub fn ascii_commands(o: &RdsOutput, d: &RdsData, include_static: bool) -> Vec<String> {
    let le = unescape(&o.line_end);
    let tags = |i: usize| d.rtplus.get(i).copied().unwrap_or((0, 0, 0));
    let (t1, t2) = (tags(0), tags(1));
    let sub = |tpl: &str| -> String {
        tpl.replace("{PS}", &d.ps).replace("{RT}", &d.rt).replace("{PI}", &d.pi).replace("{PTY}", &d.pty.to_string())
            .replace("{TP}", if d.tp { "1" } else { "0" }).replace("{TA}", if d.ta { "1" } else { "0" })
            .replace("{RTP_T1}", &format!("{:02}", t1.0)).replace("{RTP_S1}", &format!("{:02}", t1.1)).replace("{RTP_L1}", &format!("{:02}", t1.2.saturating_sub(1)))
            .replace("{RTP_T2}", &format!("{:02}", t2.0)).replace("{RTP_S2}", &format!("{:02}", t2.1)).replace("{RTP_L2}", &format!("{:02}", t2.2.saturating_sub(1)))
            .replace("{RTP_LEN1}", &t1.2.to_string()).replace("{RTP_LEN2}", &t2.2.to_string())
    };
    let mut v = vec![];
    if include_static {
        if !o.cmd_pi.is_empty() { v.push(sub(&o.cmd_pi) + &le); }
        if !o.cmd_pty.is_empty() { v.push(sub(&o.cmd_pty) + &le); }
    }
    if !o.cmd_ps.is_empty() { v.push(sub(&o.cmd_ps) + &le); }
    if !o.cmd_rt.is_empty() { v.push(sub(&o.cmd_rt) + &le); }
    if !o.cmd_rtplus.is_empty() && !d.rtplus.is_empty() { v.push(sub(&o.cmd_rtplus) + &le); }
    v
}

pub fn available_ports() -> Vec<String> { serialport::available_ports().map(|v| v.into_iter().map(|p| p.port_name).collect()).unwrap_or_default() }

// ---------------------------------------------------------------------------------------------
// Přenos

enum Conn { Tcp(TcpStream), Serial(Box<dyn serialport::SerialPort>) }

#[derive(Debug, Clone, Serialize, Default)]
pub struct OutputStatus { pub id: String, pub name: String, pub ok: bool, pub message: String, pub last_sent: Option<String>, pub bytes: u64 }

#[derive(Default)]
pub struct RdsSender {
    conns: Mutex<HashMap<String, Conn>>,
    last: Mutex<HashMap<String, (RdsData, Instant)>>,
    pub status: Mutex<HashMap<String, OutputStatus>>,
    seq: Mutex<u8>,
}

impl RdsSender {
    pub fn new() -> Self { Self::default() }

    fn write(&self, o: &RdsOutput, bytes: &[u8]) -> Result<()> {
        match o.transport.as_str() {
            "udp" => {
                let sock = UdpSocket::bind("0.0.0.0:0")?;
                sock.send_to(bytes, (o.host.as_str(), o.port))?;
                Ok(())
            }
            "file" => {
                if o.file_path.is_empty() { return Err(anyhow!("chybí cesta k souboru")); }
                let tmp = format!("{}.tmp", o.file_path);
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, &o.file_path)?;
                Ok(())
            }
            t => {
                let mut conns = self.conns.lock();
                if !conns.contains_key(&o.id) {
                    let c = if t == "serial" {
                        if o.serial_port.is_empty() { return Err(anyhow!("není zvolený sériový port")); }
                        Conn::Serial(serialport::new(&o.serial_port, o.baud.max(1200)).timeout(Duration::from_millis(500)).open()?)
                    } else {
                        let addr = (o.host.as_str(), o.port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("neplatná adresa"))?;
                        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
                        s.set_write_timeout(Some(Duration::from_secs(2)))?;
                        Conn::Tcp(s)
                    };
                    conns.insert(o.id.clone(), c);
                }
                let res = match conns.get_mut(&o.id).unwrap() { Conn::Tcp(s) => s.write_all(bytes).and_then(|_| s.flush()), Conn::Serial(p) => p.write_all(bytes).and_then(|_| p.flush()) };
                if let Err(e) = res { conns.remove(&o.id); return Err(e.into()); }
                Ok(())
            }
        }
    }

    /// Odešle data na všechny výstupy (jen při změně, statická data jednou za minutu).
    pub fn send_all(&self, cfg: &RdsConfig, d: &RdsData, force: bool) {
        for o in cfg.outputs.iter().filter(|o| o.enabled) {
            let (changed, resend) = {
                let last = self.last.lock();
                match last.get(&o.id) { Some((ld, t)) => (ld != d, t.elapsed() > Duration::from_secs(60)), None => (true, true) }
            };
            if !changed && !resend && !force { continue; }
            let payload: Vec<u8> = if o.protocol == "uecp" {
                let mut s = self.seq.lock();
                let mut b = vec![];
                for m in uecp_messages(d) { *s = s.wrapping_add(1); if *s == 0 { *s = 1; } b.extend(uecp_frame(o.uecp_site, o.uecp_encoder, *s, &m)); }
                b
            } else if o.transport == "file" {
                format!("PS={}\nRT={}\nPI={}\nPTY={}\nTP={}\nTA={}\nRTPLUS={}\n", d.ps, d.rt, d.pi, d.pty, d.tp as u8, d.ta as u8,
                    d.rtplus.iter().map(|(t, s, l)| format!("{t},{s},{l}")).collect::<Vec<_>>().join(";")).into_bytes()
            } else {
                ascii_commands(o, d, resend || force).concat().into_bytes()
            };
            let r = self.write(o, &payload);
            let mut st = self.status.lock();
            let e = st.entry(o.id.clone()).or_default();
            e.id = o.id.clone(); e.name = o.name.clone();
            match r {
                Ok(()) => { e.ok = true; e.message = String::new(); e.last_sent = Some(chrono::Local::now().to_rfc3339()); e.bytes += payload.len() as u64; self.last.lock().insert(o.id.clone(), (d.clone(), Instant::now())); }
                Err(err) => { if e.ok || e.message.is_empty() { log::warn!("RDS {}: {err}", o.name); } e.ok = false; e.message = err.to_string(); }
            }
        }
    }

    pub fn statuses(&self, cfg: &RdsConfig) -> Vec<OutputStatus> {
        let st = self.status.lock();
        cfg.outputs.iter().map(|o| st.get(&o.id).cloned().unwrap_or(OutputStatus { id: o.id.clone(), name: o.name.clone(), ..Default::default() })).collect()
    }

    pub fn reset(&self) { self.conns.lock().clear(); self.last.lock().clear(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_and_stuffing() {
        // CRC-CCITT (init FFFF, inverted) – ověření stabilní hodnoty a byte stuffingu
        let f = uecp_frame(0, 0, 0, &[0x02, 0x00, 0x00, 0xFE, 0xFF, 0xFD]);
        assert_eq!(f[0], 0xFE);
        assert_eq!(*f.last().unwrap(), 0xFF);
        assert!(f[1..f.len() - 1].windows(2).any(|w| w == [0xFD, 0x01]));
        assert!(!f[1..f.len() - 1].contains(&0xFE));
        assert_eq!(crc16_ccitt(b"123456789"), !0x29B1);
    }
    #[test]
    fn ps_helpers() {
        assert_eq!(ps8("ORCA"), "  ORCA  ");
        assert_eq!(ascii("Žluťoučký kůň"), "Zlutoucky kun");
        assert_eq!(ps_chunks("Lucie Medvídek"), vec![" Lucie  ".to_string(), "Medvídek".to_string()]);
    }
}
