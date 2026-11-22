//! Počasí z Open-Meteo (bez API klíče) pro RDS, DLS a slideshow.

use crate::config::WeatherConfig;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Weather {
    pub place: String,
    pub temp: f64,
    pub code: i64,
    pub text: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub wind_kmh: Option<f64>,
    pub humidity: Option<f64>,
    pub updated: String,
    pub manual: bool,
}

pub fn describe(code: i64, en: bool) -> &'static str {
    match (code, en) {
        (0, false) => "jasno", (0, true) => "clear sky",
        (1, false) => "skoro jasno", (1, true) => "mainly clear",
        (2, false) => "polojasno", (2, true) => "partly cloudy",
        (3, false) => "zataženo", (3, true) => "overcast",
        (45 | 48, false) => "mlha", (45 | 48, true) => "fog",
        (51..=57, false) => "mrholení", (51..=57, true) => "drizzle",
        (61 | 63 | 80 | 81, false) => "déšť", (61 | 63 | 80 | 81, true) => "rain",
        (65 | 82, false) => "silný déšť", (65 | 82, true) => "heavy rain",
        (66 | 67, false) => "mrznoucí déšť", (66 | 67, true) => "freezing rain",
        (71 | 73 | 77 | 85, false) => "sněžení", (71 | 73 | 77 | 85, true) => "snow",
        (75 | 86, false) => "silné sněžení", (75 | 86, true) => "heavy snow",
        (95, false) => "bouřky", (95, true) => "thunderstorm",
        (96 | 99, false) => "bouřky s kroupami", (96 | 99, true) => "thunderstorm with hail",
        (_, false) => "proměnlivo", (_, true) => "changeable",
    }
}

pub fn fetch(cfg: &WeatherConfig, en: bool) -> Result<Weather> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code,wind_speed_10m,relative_humidity_2m&daily=temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=1",
        cfg.lat, cfg.lon
    );
    let v: serde_json::Value = ureq::get(&url).timeout(std::time::Duration::from_secs(10)).call()?.into_json()?;
    let cur = v.get("current").ok_or_else(|| anyhow!("odpověď bez aktuálního počasí"))?;
    let code = cur.get("weather_code").and_then(|x| x.as_i64()).unwrap_or(-1);
    let first = |k: &str| v.get("daily").and_then(|d| d.get(k)).and_then(|a| a.get(0)).and_then(|x| x.as_f64());
    Ok(Weather {
        place: cfg.place.clone(),
        temp: cur.get("temperature_2m").and_then(|x| x.as_f64()).unwrap_or(0.0),
        code, text: describe(code, en).to_string(),
        min: first("temperature_2m_min"), max: first("temperature_2m_max"),
        wind_kmh: cur.get("wind_speed_10m").and_then(|x| x.as_f64()),
        humidity: cur.get("relative_humidity_2m").and_then(|x| x.as_f64()),
        updated: chrono::Local::now().to_rfc3339(), manual: false,
    })
}

/// Vyhledání místa (název → souřadnice).
pub fn geocode(name: &str, lang: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!("https://geocoding-api.open-meteo.com/v1/search?name={}&count=8&language={}", urlencoding::encode(name), lang);
    let v: serde_json::Value = ureq::get(&url).timeout(std::time::Duration::from_secs(10)).call()?.into_json()?;
    Ok(v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default().into_iter().map(|r| serde_json::json!({
        "name": r.get("name"), "admin1": r.get("admin1"), "country": r.get("country"), "lat": r.get("latitude"), "lon": r.get("longitude"),
    })).collect())
}

pub fn fmt_temp(t: f64) -> String { format!("{}", t.round() as i64) }
