//! Uživatelé, role a oprávnění (Jazler: Users & Permissions). Přihlášení PINem/heslem,
//! automatické odhlášení po nečinnosti. Bez uživatelů (nebo s vypnutým `require_login`)
//! pracuje systém jako dosud – vše povoleno.

use crate::db::{now_utc, Db};
use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Oprávnění – klíče používané v UI i na serveru.
pub const PERMS: &[&str] = &[
    "studio",      // ovládání vysílání (play/next/stop, fronta, instanty, znělky)
    "library",     // úpravy knihovny, import, mazání skladeb
    "spots",       // reklama: kampaně, spoty, bloky
    "schedule",    // plánování, playlisty, události, pořady, úpravy plánu Music Selectoru
    "publish",     // publikace plánu Music Selectoru (koncept → vysílání)
    "broadcast",   // výstupy, streamy, převzaté vysílání, metadata, RDS/DAB
    "processing",  // zvukový processing
    "reports",     // reporty a logy
    "settings",    // obecná nastavení, zálohy, údržba
    "users",       // správa uživatelů
    "voicetrack",  // nahrávání voice tracků (i z domova přes web)
];

pub fn role_perms(role: &str) -> Vec<String> {
    let p: &[&str] = match role {
        "admin" => PERMS,
        "director" => &["studio", "library", "spots", "schedule", "publish", "reports", "voicetrack"],
        "editor" => &["studio", "library", "spots", "schedule", "reports", "voicetrack"],
        "presenter" => &["studio", "voicetrack"],
        "viewer" => &[],
        _ => &[],
    };
    p.iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub role: String,
    /// Doplňková oprávnění nad rámec role
    #[serde(default)]
    pub perms: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub has_pin: bool,
    #[serde(default)]
    pub last_login: Option<String>,
}
fn yes() -> bool { true }

impl User {
    pub fn effective(&self) -> Vec<String> {
        let mut v = role_perms(&self.role);
        for p in &self.perms { if !v.contains(p) { v.push(p.clone()); } }
        v
    }
}

fn hash(name: &str, pin: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"orca-radioair:");
    h.update(name.to_lowercase().as_bytes());
    h.update(b":");
    h.update(pin.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Session { pub user: User, pub last: Instant }

#[derive(Default)]
pub struct Users { sessions: Mutex<HashMap<String, Session>> }

impl Users {
    pub fn list(&self, db: &Db) -> Result<Vec<User>> {
        db.with_conn(|c| -> Result<_> {
            let mut st = c.prepare("SELECT id, name, role, perms, enabled, pin_hash != '', last_login FROM users ORDER BY name")?;
            let v = st.query_map([], |r| Ok(User { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, perms: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(), enabled: r.get::<_, i64>(4)? != 0, has_pin: r.get(5)?, last_login: r.get(6)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        })
    }

    pub fn save(&self, db: &Db, u: &User, pin: Option<&str>) -> Result<i64> {
        let name = u.name.trim();
        if name.is_empty() { return Err(anyhow!("zadejte jméno")); }
        if !["admin", "director", "editor", "presenter", "viewer"].contains(&u.role.as_str()) { return Err(anyhow!("neznámá role {}", u.role)); }
        let perms = serde_json::to_string(&u.perms.iter().filter(|p| PERMS.contains(&p.as_str())).collect::<Vec<_>>())?;
        // poslední aktivní administrátor nesmí přijít o roli
        if u.id > 0 && (u.role != "admin" || !u.enabled) {
            let admins = self.list(db)?.into_iter().filter(|x| x.role == "admin" && x.enabled && x.id != u.id).count();
            let was_admin = self.list(db)?.into_iter().any(|x| x.id == u.id && x.role == "admin");
            if was_admin && admins == 0 { return Err(anyhow!("v systému musí zůstat alespoň jeden aktivní administrátor")); }
        }
        db.with_conn(|c| -> Result<i64> {
            if u.id > 0 {
                c.execute("UPDATE users SET name=?2, role=?3, perms=?4, enabled=?5 WHERE id=?1", params![u.id, name, u.role, perms, u.enabled as i64])?;
                if let Some(p) = pin { c.execute("UPDATE users SET pin_hash=?2 WHERE id=?1", params![u.id, if p.is_empty() { String::new() } else { hash(name, p) }])?; }
                Ok(u.id)
            } else {
                c.execute("INSERT INTO users(name, role, perms, enabled, pin_hash) VALUES(?1,?2,?3,?4,?5)", params![name, u.role, perms, u.enabled as i64, pin.filter(|p| !p.is_empty()).map(|p| hash(name, p)).unwrap_or_default()])?;
                Ok(c.last_insert_rowid())
            }
        })
    }

    pub fn delete(&self, db: &Db, id: i64) -> Result<()> {
        let all = self.list(db)?;
        if all.iter().any(|x| x.id == id && x.role == "admin") && all.iter().filter(|x| x.role == "admin" && x.enabled).count() <= 1 {
            return Err(anyhow!("nelze smazat posledního administrátora"));
        }
        db.with_conn(|c| c.execute("DELETE FROM users WHERE id=?1", [id]))?;
        self.sessions.lock().retain(|_, s| s.user.id != id);
        Ok(())
    }

    pub fn login(&self, db: &Db, name: &str, pin: &str) -> Result<(String, User)> {
        let row: Option<(i64, String)> = db.with_conn(|c| c.query_row("SELECT id, pin_hash FROM users WHERE lower(name)=lower(?1) AND enabled=1", [name.trim()], |r| Ok((r.get(0)?, r.get(1)?))).ok());
        let (id, h) = row.ok_or_else(|| anyhow!("neznámý uživatel"))?;
        if !h.is_empty() && h != hash(name.trim(), pin) {
            std::thread::sleep(Duration::from_millis(400));
            return Err(anyhow!("chybný PIN"));
        }
        let user = self.list(db)?.into_iter().find(|u| u.id == id).ok_or_else(|| anyhow!("uživatel nenalezen"))?;
        db.with_conn(|c| c.execute("UPDATE users SET last_login=?2 WHERE id=?1", params![id, now_utc()]))?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.sessions.lock().insert(token.clone(), Session { user: user.clone(), last: Instant::now() });
        Ok((token, user))
    }

    pub fn logout(&self, token: &str) { self.sessions.lock().remove(token); }

    /// Vrátí uživatele relace a obnoví čas aktivity; None = neplatná / vypršelá relace.
    pub fn touch(&self, token: &str, auto_logout_min: u32) -> Option<User> {
        let mut s = self.sessions.lock();
        if auto_logout_min > 0 {
            let lim = Duration::from_secs(auto_logout_min as u64 * 60);
            s.retain(|_, x| x.last.elapsed() < lim);
        }
        let x = s.get_mut(token)?;
        x.last = Instant::now();
        Some(x.user.clone())
    }

    pub fn any(&self, db: &Db) -> bool { self.list(db).map(|v| !v.is_empty()).unwrap_or(false) }
}

/// Které oprávnění vyžaduje daný API požadavek (None = jen čtení / bez omezení).
pub fn required_perm(method: &str, path: &str) -> Option<&'static str> {
    let p = path.trim_start_matches("/api");
    let write = method != "GET" && method != "HEAD";
    let pick = |prefixes: &[&str]| prefixes.iter().any(|x| p.starts_with(x));
    if pick(&["/users"]) { return Some("users"); }
    if write && p.starts_with("/selector/logs/") && p.ends_with("/publish") { return Some("publish"); }
    if p.starts_with("/session") || p.starts_with("/manual") || p.starts_with("/ui/") { return None; }
    // voice tracky z domova kontrolují přihlášení a oprávnění samy (vždy, i bez povinného přihlášení)
    if p.starts_with("/vt/") || p == "/vt" { return None; }
    if !write { return None; }
    if pick(&["/transport", "/gpio", "/queue", "/instants", "/pads", "/mode", "/monitor", "/memos", "/record"]) { return Some("studio"); }
    if pick(&["/tracks", "/library", "/categories", "/upload", "/analyze", "/stitch", "/ai/track-notes", "/ai/batch", "/video"]) { return Some("library"); }
    if pick(&["/spots", "/campaigns", "/breaks"]) { return Some("spots"); }
    if pick(&["/plan", "/sched", "/selector", "/playlists", "/events", "/shows", "/episodes", "/grid", "/rotations"]) { return Some("schedule"); }
    if pick(&["/encoders", "/relay", "/meta", "/rds", "/dab", "/outputs"]) { return Some("broadcast"); }
    if pick(&["/processing", "/proc"]) { return Some("processing"); }
    if pick(&["/reports"]) { return Some("reports"); }
    Some("settings")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn perms_map() {
        assert_eq!(required_perm("POST", "/api/transport/next"), Some("studio"));
        assert_eq!(required_perm("GET", "/api/tracks"), None);
        assert_eq!(required_perm("PUT", "/api/config"), Some("settings"));
        assert_eq!(required_perm("GET", "/api/users"), Some("users"));
        assert!(role_perms("presenter").contains(&"studio".to_string()));
        assert_eq!(required_perm("POST", "/api/selector/logs/5/publish"), Some("publish"));
        assert_eq!(required_perm("POST", "/api/selector/logs/5/edit"), Some("schedule"));
        assert!(!role_perms("editor").contains(&"publish".to_string()));
        assert!(!role_perms("presenter").contains(&"settings".to_string()));
    }
}
