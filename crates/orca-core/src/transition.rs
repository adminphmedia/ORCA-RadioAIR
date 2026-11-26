//! Vícestopý editor přechodů – zvukový náhled přechodu (konec skladby A, začátek B, voice track / sweeper).
//! Časy jsou v milisekundách souboru A (B začíná v bodě mix_ms skladby A).

use crate::config::Config;
use crate::db::Db;
use crate::encoder::resolve_ffmpeg;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
pub struct Lane { pub track_id: i64, #[serde(default)] pub cue_in_ms: Option<i64>, #[serde(default)] pub mix_ms: Option<i64>, #[serde(default)] pub cue_out_ms: Option<i64>, #[serde(default)] pub gain_db: f32 }

#[derive(Debug, Clone, Deserialize)]
pub struct Overlay { pub track_id: i64, /// začátek v čase souboru A
    pub at_ms: i64, #[serde(default)] pub gain_db: f32, /// o kolik dB ztlumit hudbu pod hlasem
    #[serde(default)] pub duck_db: f32 }

#[derive(Debug, Clone, Deserialize)]
pub struct Preview { pub a: Lane, pub b: Lane, #[serde(default)] pub v: Option<Overlay>, /// kolik ms před přechodem začít
    #[serde(default)] pub pre_ms: i64, /// délka náhledu
    #[serde(default)] pub len_ms: i64 }

pub fn render(cfg: &Config, db: &Db, p: &Preview) -> Result<PathBuf> {
    let path = |id: i64| -> Result<(PathBuf, crate::model::Track)> {
        let t = db.get_track(id)?.ok_or_else(|| anyhow!("položka {id} není v knihovně"))?;
        let pth = PathBuf::from(crate::general::map_path(cfg, &t.path));
        if !pth.is_file() { return Err(anyhow!("soubor chybí: {}", t.path)); }
        Ok((pth, t))
    };
    let (pa, ta) = path(p.a.track_id)?; let (pb, tb) = path(p.b.track_id)?;
    let (_, amix0, aout0) = ta.effective_cues();
    let amix = p.a.mix_ms.unwrap_or(amix0); let aout = p.a.cue_out_ms.unwrap_or(aout0).max(amix);
    let bin = p.b.cue_in_ms.unwrap_or(tb.effective_cues().0);
    let pre = if p.pre_ms > 0 { p.pre_ms } else { 8000 };
    let start = (amix - pre).max(0);
    let len = if p.len_ms > 0 { p.len_ms } else { 20000 };
    let s = |ms: i64| format!("{:.3}", ms as f64 / 1000.0);
    let mut cmd = Command::new(resolve_ffmpeg(cfg));
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    cmd.args(["-ss", &s(start), "-t", &s((aout - start).max(100)), "-i"]).arg(&pa);
    cmd.args(["-ss", &s(bin), "-t", &s(len), "-i"]).arg(&pb);
    let fmt = "aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=stereo";
    let mut f = format!("[0:a]{fmt},volume={:.2}dB[a];[1:a]{fmt},volume={:.2}dB,adelay={}|{}[b]", p.a.gain_db, p.b.gain_db, amix - start, amix - start);
    let mut n = 2;
    if let Some(v) = &p.v {
        let (pv, _) = path(v.track_id)?;
        cmd.arg("-i").arg(&pv);
        let d = (v.at_ms - start).max(0);
        f += &format!(";[2:a]{fmt},volume={:.2}dB,adelay={d}|{d}[v]", v.gain_db);
        if v.duck_db < -0.5 {
            // hudba pod hlasem ztlumená (sidechain podle hlasu)
            f += &format!(";[a][b]amix=inputs=2:duration=longest:normalize=0[m];[v]asplit=2[v1][v2];[m][v1]sidechaincompress=threshold=0.02:ratio=20:attack=20:release=400:makeup=1:level_sc=1:mix={:.2}[md];[md][v2]amix=inputs=2:duration=longest:normalize=0[out]", (1.0 - 10f64.powf(v.duck_db as f64 / 20.0)).clamp(0.0, 1.0));
        } else {
            f += ";[a][b][v]amix=inputs=3:duration=longest:normalize=0[out]";
        }
        n = 3;
    } else {
        f += ";[a][b]amix=inputs=2:duration=longest:normalize=0[out]";
    }
    let _ = n;
    let out = std::env::temp_dir().join(format!("orca-transition-{}.mp3", std::process::id()));
    cmd.args(["-filter_complex", &f, "-map", "[out]", "-t", &s(pre + len), "-c:a", "libmp3lame", "-b:a", "160k"]).arg(&out);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
    let o = cmd.output()?;
    if !o.status.success() { return Err(anyhow!("náhled přechodu selhal: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""))); }
    Ok(out)
}
