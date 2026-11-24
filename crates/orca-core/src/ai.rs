//! Informace o skladbě pro moderátora – generování textu přes AI podle nastavení stanice.
//!
//! Poskytovatel je volitelný: Anthropic (Claude), OpenAI (ChatGPT), libovolná služba kompatibilní s OpenAI API
//! nebo místní model v Ollama. Klíč se ukládá jen do konfigurace tohoto počítače. Text je vždy návrh –
//! moderátor / dramaturg ho zkontroluje a uloží; zdroj (model, datum) se u skladby eviduje.

use crate::config::Config;
use crate::model::Track;
use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    /// anthropic | openai | compatible | ollama | "" (vypnuto)
    pub provider: String,
    /// API klíč (uložen jen v konfiguraci tohoto počítače)
    pub api_key: String,
    pub model: String,
    /// Adresa služby (Ollama: http://localhost:11434, kompatibilní služby: …/v1)
    pub base_url: String,
    /// Styl stanice (preset promptu) – viz PRESETS; "custom" = vlastní text
    pub preset: String,
    /// Vlastní pokyny (doplní se k pravidlům přesnosti)
    pub prompt: String,
    /// short | medium | long
    pub length: String,
    /// Ověřovat fakta vyhledáváním na webu (Anthropic: nástroj web search)
    pub web_search: bool,
    /// Jazyk textu (cs / en / sk …)
    pub language: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self { provider: String::new(), api_key: String::new(), model: String::new(), base_url: String::new(), preset: "hitradio".into(), prompt: String::new(),
            length: "medium".into(), web_search: true, language: "cs".into() }
    }
}

/// Styly rozhlasových stanic (id, název, pokyny).
pub const PRESETS: &[(&str, &str, &str)] = &[
    ("hitradio", "Hitové rádio (CHR / Hot AC)", "Stanice hraje aktuální hity. Piš svižně a krátce, jak mluví moderátor ranní show: úspěchy v hitparádách, rekordy, zajímavost z natáčení nebo klipu, souvislost s aktuálním děním interpreta (nové album, turné). Žádný patos."),
    ("oldies", "Oldies / zlaté hity", "Stanice hraje hity 60.–90. let. Vyprávěj příběh skladby: jak a kdy vznikla, kdo ji napsal, kde se nahrávala, v jaké době zazněla, čím byla výjimečná, cover verze a souvislosti s dobou. Laskavý, nostalgický tón."),
    ("regional", "Regionální stanice", "Regionální stanice s českou a slovenskou hudbou i světovými hity. Zdůrazni vazbu interpreta na Česko a region (koncerty, původ, spolupráce s českými umělci), úspěchy v českých hitparádách a zajímavosti srozumitelné běžnému posluchači."),
    ("rock", "Rocková stanice", "Rocková stanice. Zajímavosti o kapele, sestavě, nástrojích a zvuku, okolnosti vzniku alba, koncertní historie a vliv skladby na další muzikanty. Věcně a s respektem k žánru."),
    ("dance", "Dance / elektronická hudba", "Stanice taneční a elektronické hudby. Producent, remixy, labely, kluby a festivaly, kde skladba zazněla, technické zajímavosti (sample, BPM). Krátce a energicky."),
    ("public", "Veřejnoprávní / mluvené slovo", "Kultivovaná stanice s důrazem na kontext. Uveď autory hudby a textu, historický a kulturní kontext, ocenění a význam skladby. Věcný, uměřený tón bez superlativů."),
    ("country", "Country / folk", "Stanice country a folku. Příběh písně a textu, autor, tradice a souvislosti žánru, česká převzetí (coververze s českým textem), festivaly."),
];

const RULES_CS: &str = "Pravidla přesnosti (nejdůležitější):\n\
- Piš jen fakta, která jsou ověřitelná. Nic si nevymýšlej – žádná data, čísla, citáty ani příběhy, u kterých si nejsi jistý.\n\
- Pokud o skladbě nic spolehlivého nevíš, napiš přesně: „Ke skladbě nemám ověřené informace.“ a nic dalšího.\n\
- Neuváděj pomluvy, soukromé věci, zdraví ani spekulace o lidech.\n\
- Pokud si nejsi jistý jedním údajem, vynech ho (nepiš „možná“, „údajně“).\n\
- Rozlišuj skladbu a stejnojmenné skladby jiných interpretů.\n\
Formát: krátké odrážky (každá 1–2 věty), které moderátor může rovnou říct do mikrofonu. Bez nadpisu, bez úvodu, bez emoji, bez odkazů.";

const RULES_EN: &str = "Accuracy rules (most important):\n\
- Only write verifiable facts. Never invent dates, numbers, quotes or stories you are not sure about.\n\
- If you have no reliable information, reply exactly: \"No verified information about this track.\" and nothing else.\n\
- No gossip, private matters, health or speculation about people.\n\
- If unsure about a detail, leave it out (no \"maybe\", \"reportedly\").\n\
- Do not confuse the track with same-titled songs by other artists.\n\
Format: short bullet points (1–2 sentences each) a presenter can read on air. No heading, no intro, no emoji, no links.";

fn length_hint(l: &str, en: bool) -> &'static str {
    match (l, en) {
        ("short", false) => "Napiš 2–3 odrážky.", ("short", true) => "Write 2–3 bullet points.",
        ("long", false) => "Napiš 6–8 odrážek.", ("long", true) => "Write 6–8 bullet points.",
        (_, false) => "Napiš 4–5 odrážek.", (_, true) => "Write 4–5 bullet points.",
    }
}

/// Systémový prompt podle nastavení (pro náhled v UI i pro volání).
pub fn system_prompt(cfg: &Config) -> String {
    let a = &cfg.ai;
    let en = a.language == "en";
    let style = PRESETS.iter().find(|p| p.0 == a.preset).map(|p| p.2).unwrap_or("");
    let station = if cfg.meta.station_long.is_empty() { cfg.station.name.clone() } else { cfg.meta.station_long.clone() };
    let mut s = if en { format!("You prepare on-air notes for the presenter of radio station {station}.") } else { format!("Připravuješ podklady pro moderátora rozhlasové stanice {station}. Píšeš česky.") };
    if a.language != "cs" && a.language != "en" && !a.language.is_empty() { s += &format!(" Write in language: {}.", a.language); }
    if !style.is_empty() { s += "\n\n"; s += style; }
    if !a.prompt.trim().is_empty() { s += "\n\n"; s += a.prompt.trim(); }
    s += "\n\n";
    s += if en { RULES_EN } else { RULES_CS };
    s += "\n";
    s += length_hint(&a.length, en);
    s
}

pub fn user_prompt(t: &Track, en: bool) -> String {
    let mut v = vec![];
    let mut add = |k: &str, val: &str| { if !val.trim().is_empty() { v.push(format!("{k}: {}", val.trim())); } };
    add(if en { "Artist" } else { "Interpret" }, &t.artist);
    add(if en { "Title" } else { "Název" }, &t.title);
    add("Album", &t.album);
    add(if en { "Year" } else { "Rok" }, &t.year.map(|y| y.to_string()).unwrap_or_default());
    add(if en { "Composer" } else { "Autor hudby" }, &t.composer);
    add(if en { "Lyricist" } else { "Autor textu" }, &t.lyricist);
    add("Label", &t.label);
    add(if en { "Genre" } else { "Žánr" }, &t.genre);
    format!("{}\n\n{}", if en { "Prepare presenter notes about this track:" } else { "Připrav podklady pro moderátora k této skladbě:" }, v.join("\n"))
}

fn agent(timeout_s: u64) -> ureq::Agent { ureq::AgentBuilder::new().timeout(Duration::from_secs(timeout_s)).build() }

fn err_text(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body).ok()
                .and_then(|v| v.pointer("/error/message").or_else(|| v.get("error")).and_then(|x| x.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| body.chars().take(300).collect());
            match code { 401 | 403 => anyhow!("AI odmítla přístup ({code}) – zkontrolujte API klíč: {msg}"), 404 => anyhow!("model nebo adresa nenalezena (404): {msg}"), 429 => anyhow!("AI je přetížená nebo došel kredit (429): {msg}"), _ => anyhow!("AI vrátila chybu {code}: {msg}") }
        }
        e => anyhow!("AI není dostupná: {e}"),
    }
}

/// Zavolá AI se systémovým a uživatelským promptem a vrátí text.
pub fn complete(a: &AiConfig, system: &str, user: &str) -> Result<String> {
    let max_tokens = match a.length.as_str() { "short" => 500, "long" => 1500, _ => 900 };
    match a.provider.as_str() {
        "anthropic" => {
            if a.api_key.trim().is_empty() { return Err(anyhow!("chybí API klíč Anthropic")); }
            let model = if a.model.is_empty() { "claude-sonnet-5-5" } else { &a.model };
            let mut body = serde_json::json!({ "model": model, "max_tokens": max_tokens + if a.web_search { 1500 } else { 0 }, "system": system, "messages": [{ "role": "user", "content": user }] });
            if a.web_search { body["tools"] = serde_json::json!([{ "type": "web_search_20250305", "name": "web_search", "max_uses": 3 }]); }
            let url = if a.base_url.is_empty() { "https://api.anthropic.com/v1/messages".to_string() } else { format!("{}/v1/messages", a.base_url.trim_end_matches('/')) };
            let v: serde_json::Value = agent(120).post(&url).set("x-api-key", a.api_key.trim()).set("anthropic-version", "2023-06-01").send_json(body).map_err(err_text)?.into_json()?;
            // u odpovědi s vyhledáváním je text rozdělený do více bloků – použijeme bloky za posledním vyhledáváním
            let blocks = v.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            let last_tool = blocks.iter().rposition(|b| b.get("type").and_then(|t| t.as_str()).map(|t| t.contains("tool_result") || t == "server_tool_use").unwrap_or(false));
            let text: String = blocks.iter().enumerate().filter(|(i, _)| last_tool.map(|l| *i > l).unwrap_or(true))
                .filter_map(|(_, b)| (b.get("type").and_then(|t| t.as_str()) == Some("text")).then(|| b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())).collect();
            if text.trim().is_empty() { return Err(anyhow!("AI nevrátila žádný text")); }
            Ok(text.trim().to_string())
        }
        "openai" | "compatible" => {
            let base = if a.base_url.is_empty() { "https://api.openai.com/v1".to_string() } else { a.base_url.trim_end_matches('/').to_string() };
            if a.model.is_empty() { return Err(anyhow!("vyberte model (tlačítko Načíst modely)")); }
            let body = serde_json::json!({ "model": a.model, "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }] });
            let mut req = agent(120).post(&format!("{base}/chat/completions"));
            if !a.api_key.trim().is_empty() { req = req.set("Authorization", &format!("Bearer {}", a.api_key.trim())); }
            let v: serde_json::Value = req.send_json(body).map_err(err_text)?.into_json()?;
            v.pointer("/choices/0/message/content").and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("AI nevrátila žádný text"))
        }
        "ollama" => {
            let base = if a.base_url.is_empty() { "http://localhost:11434".to_string() } else { a.base_url.trim_end_matches('/').to_string() };
            if a.model.is_empty() { return Err(anyhow!("vyberte model (tlačítko Načíst modely)")); }
            let body = serde_json::json!({ "model": a.model, "stream": false, "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }] });
            let v: serde_json::Value = agent(300).post(&format!("{base}/api/chat")).send_json(body).map_err(err_text)?.into_json()?;
            v.pointer("/message/content").and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("model nevrátil žádný text"))
        }
        "" => Err(anyhow!("AI není nastavená – Nastavení → Informace o skladbách (AI)")),
        p => Err(anyhow!("neznámý poskytovatel AI „{p}“")),
    }
}

/// Seznam modelů poskytovatele (pro výběr v nastavení).
pub fn models(a: &AiConfig) -> Result<Vec<String>> {
    let mut v: Vec<String> = match a.provider.as_str() {
        "anthropic" => {
            let url = if a.base_url.is_empty() { "https://api.anthropic.com/v1/models?limit=100".to_string() } else { format!("{}/v1/models?limit=100", a.base_url.trim_end_matches('/')) };
            let r: serde_json::Value = agent(20).get(&url).set("x-api-key", a.api_key.trim()).set("anthropic-version", "2023-06-01").call().map_err(err_text)?.into_json()?;
            r.get("data").and_then(|d| d.as_array()).map(|d| d.iter().filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(String::from)).collect()).unwrap_or_default()
        }
        "openai" | "compatible" => {
            let base = if a.base_url.is_empty() { "https://api.openai.com/v1".to_string() } else { a.base_url.trim_end_matches('/').to_string() };
            let mut req = agent(20).get(&format!("{base}/models"));
            if !a.api_key.trim().is_empty() { req = req.set("Authorization", &format!("Bearer {}", a.api_key.trim())); }
            let r: serde_json::Value = req.call().map_err(err_text)?.into_json()?;
            let all: Vec<String> = r.get("data").and_then(|d| d.as_array()).map(|d| d.iter().filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(String::from)).collect()).unwrap_or_default();
            // u OpenAI jen textové modely
            if a.provider == "openai" { all.into_iter().filter(|m| (m.starts_with("gpt") || m.starts_with('o')) && !m.contains("audio") && !m.contains("realtime") && !m.contains("image") && !m.contains("tts") && !m.contains("transcribe") && !m.contains("embedding")).collect() } else { all }
        }
        "ollama" => {
            let base = if a.base_url.is_empty() { "http://localhost:11434".to_string() } else { a.base_url.trim_end_matches('/').to_string() };
            let r: serde_json::Value = agent(10).get(&format!("{base}/api/tags")).call().map_err(err_text)?.into_json()?;
            r.get("models").and_then(|d| d.as_array()).map(|d| d.iter().filter_map(|m| m.get("name").and_then(|x| x.as_str()).map(String::from)).collect()).unwrap_or_default()
        }
        _ => return Err(anyhow!("nejdřív vyberte poskytovatele")),
    };
    v.sort();
    Ok(v)
}

/// Vygeneruje text pro moderátora ke skladbě (neukládá).
pub fn track_notes(cfg: &Config, t: &Track) -> Result<(String, String)> {
    let text = complete(&cfg.ai, &system_prompt(cfg), &user_prompt(t, cfg.ai.language == "en"))?;
    let model = if cfg.ai.model.is_empty() && cfg.ai.provider == "anthropic" { "claude-sonnet-5-5".to_string() } else { cfg.ai.model.clone() };
    Ok((text, format!("ai:{} {}", model, chrono::Local::now().format("%Y-%m-%d"))))
}

// ---------------------------------------------------------------------------------------------
// Dávkové generování (vybrané skladby / všechny bez textu)

#[derive(Debug, Clone, Serialize, Default)]
pub struct BatchStatus { pub running: bool, pub total: usize, pub done: usize, pub ok: usize, pub failed: usize, pub last: String, pub errors: Vec<String>, pub message: String }

fn cell() -> &'static (Mutex<BatchStatus>, AtomicBool) {
    static C: OnceLock<(Mutex<BatchStatus>, AtomicBool)> = OnceLock::new();
    C.get_or_init(|| (Mutex::new(BatchStatus::default()), AtomicBool::new(false)))
}
pub fn batch_status() -> BatchStatus { cell().0.lock().clone() }
pub fn batch_cancel() { cell().1.store(true, Ordering::Relaxed); }

/// Spustí dávku na pozadí. Hotové texty se rovnou uloží ke skladbám (s označením AI).
pub fn batch_start(db: Arc<crate::db::Db>, cfg: Config, ids: Vec<i64>, overwrite: bool) -> Result<BatchStatus> {
    if cell().0.lock().running { return Err(anyhow!("generování už běží")); }
    if cfg.ai.provider.is_empty() { return Err(anyhow!("AI není nastavená")); }
    cell().1.store(false, Ordering::Relaxed);
    *cell().0.lock() = BatchStatus { running: true, total: ids.len(), message: "Generuji…".into(), ..Default::default() };
    std::thread::Builder::new().name("orca-ai-batch".into()).spawn(move || {
        for id in ids {
            if cell().1.load(Ordering::Relaxed) { break; }
            let r = (|| -> Result<Option<String>> {
                let Some(t) = db.get_track(id)? else { return Ok(None) };
                if !overwrite && !t.presenter_notes.trim().is_empty() { return Ok(None); }
                let (text, src) = track_notes(&cfg, &t)?;
                db.patch_track(id, &crate::db::TrackPatch { presenter_notes: Some(text), notes_source: Some(src), ..Default::default() })?;
                Ok(Some(t.display()))
            })();
            let mut s = cell().0.lock();
            s.done += 1;
            match r { Ok(Some(n)) => { s.ok += 1; s.last = n; } Ok(None) => {} Err(e) => { s.failed += 1; if s.errors.len() < 20 { s.errors.push(format!("#{id}: {e}")); } } }
        }
        let mut s = cell().0.lock();
        s.running = false;
        s.message = format!("Hotovo: {} textů vygenerováno{}", s.ok, if s.failed > 0 { format!(", {} chyb", s.failed) } else { String::new() });
    }).ok();
    Ok(batch_status())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompts() {
        let mut cfg = Config::default();
        cfg.meta.station_long = "Rádio Vysočina".into();
        cfg.ai.preset = "oldies".into();
        cfg.ai.prompt = "Zmiň české coververze.".into();
        let s = system_prompt(&cfg);
        assert!(s.contains("Rádio Vysočina") && s.contains("60.–90.") && s.contains("Nic si nevymýšlej") && s.contains("české coververze") && s.contains("4–5"));
        let t = Track { artist: "Olympic".into(), title: "Jasná zpráva".into(), year: Some(1968), ..Default::default() };
        let u = user_prompt(&t, false);
        assert!(u.contains("Interpret: Olympic") && u.contains("Rok: 1968") && !u.contains("Album"));
        assert!(complete(&AiConfig::default(), "", "").is_err());
    }
}
