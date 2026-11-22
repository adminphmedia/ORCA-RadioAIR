//! Dekódování audio souborů (MP3, AAC/M4A, FLAC, WAV, AIFF, OGG Vorbis, ALAC) přes Symphonia
//! a převzorkování na vzorkovací frekvenci enginu.

use anyhow::{anyhow, Context, Result};
use rubato::{FftFixedIn, Resampler};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

pub const SUPPORTED_EXT: &[&str] = &["mp3", "mp2", "m4a", "aac", "mp4", "flac", "wav", "aif", "aiff", "ogg", "oga"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
        && !path.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("._") || n.starts_with('.')).unwrap_or(false)
}

/// Dekodér vracející prokládané stereo f32 v původní frekvenci.
pub struct AudioDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    pub sample_rate: u32,
    pub channels: usize,
    pub duration_frames: Option<u64>,
    sample_buf: Option<SampleBuffer<f32>>,
    skip_until_ts: u64,
}

impl AudioDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("nelze otevřít {}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) { hint.with_extension(ext); }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions { enable_gapless: true, ..Default::default() }, &MetadataOptions::default())
            .with_context(|| format!("neznámý formát {}", path.display()))?;
        let format = probed.format;
        let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or_else(|| anyhow!("soubor neobsahuje audio"))?;
        let sample_rate = track.codec_params.sample_rate.ok_or_else(|| anyhow!("neznámá vzorkovací frekvence"))?;
        let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2).max(1);
        let duration_frames = track.codec_params.n_frames;
        let decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
        let track_id = track.id;
        Ok(Self { format, decoder, track_id, sample_rate, channels, duration_frames, sample_buf: None, skip_until_ts: 0 })
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.duration_frames.map(|f| (f as f64 * 1000.0 / self.sample_rate as f64) as i64)
    }

    /// Přesný posun na pozici v ms.
    pub fn seek_ms(&mut self, ms: i64) -> Result<()> {
        if ms <= 0 { return Ok(()); }
        let t = Time::from(ms as f64 / 1000.0);
        let seeked = self.format.seek(SeekMode::Accurate, SeekTo::Time { time: t, track_id: Some(self.track_id) })?;
        self.decoder.reset();
        self.skip_until_ts = seeked.required_ts;
        Ok(())
    }

    /// Další blok jako prokládané stereo. Vrací false na konci souboru.
    pub fn next_stereo(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        out.clear();
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymError::ResetRequired) => { self.decoder.reset(); continue; }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id() != self.track_id { continue; }
            let pkt_ts = packet.ts();
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymError::DecodeError(_)) => continue, // poškozený rámec – přeskočit
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            let spec = *decoded.spec();
            let frames = decoded.frames();
            if frames == 0 { continue; }
            let need = decoded.capacity() as u64;
            if self.sample_buf.as_ref().map(|b| (b.capacity() as u64) < need * spec.channels.count() as u64).unwrap_or(true) {
                self.sample_buf = Some(SampleBuffer::<f32>::new(need, spec));
            }
            let sb = self.sample_buf.as_mut().unwrap();
            sb.copy_interleaved_ref(decoded);
            let ch = spec.channels.count().max(1);
            let samples = sb.samples();
            // přeskočení vzorků před požadovanou pozicí po seeku
            let skip = if self.skip_until_ts > pkt_ts { ((self.skip_until_ts - pkt_ts) as usize).min(frames) } else { 0 };
            if skip >= frames { continue; }
            self.skip_until_ts = 0;
            out.reserve((frames - skip) * 2);
            for f in skip..frames {
                let base = f * ch;
                let (l, r) = if ch == 1 { (samples[base], samples[base]) } else { (samples[base], samples[base + 1]) };
                out.push(l);
                out.push(r);
            }
            return Ok(true);
        }
    }
}

/// Stereo stream převzorkovaný na cílovou frekvenci.
pub struct StereoStream {
    dec: AudioDecoder,
    resampler: Option<FftFixedIn<f32>>,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    chunk: usize,
    pending_delay: usize,
    flushed: bool,
    eof: bool,
    tmp: Vec<f32>,
}

impl StereoStream {
    pub fn open(path: &Path, target_rate: u32, start_ms: i64) -> Result<Self> {
        let mut dec = AudioDecoder::open(path)?;
        if start_ms > 0 {
            if let Err(e) = dec.seek_ms(start_ms) {
                log::warn!("seek selhal ({e}), dekóduji od začátku a zahazuji");
                dec = AudioDecoder::open(path)?;
                let mut to_skip = (start_ms as f64 * dec.sample_rate as f64 / 1000.0) as usize * 2;
                let mut buf = Vec::new();
                while to_skip > 0 {
                    if !dec.next_stereo(&mut buf)? { break; }
                    if buf.len() <= to_skip { to_skip -= buf.len(); } else { to_skip = 0; }
                }
            }
        }
        let chunk = 1024;
        let resampler = if dec.sample_rate != target_rate {
            Some(FftFixedIn::<f32>::new(dec.sample_rate as usize, target_rate as usize, chunk, 2, 2)?)
        } else { None };
        let pending_delay = resampler.as_ref().map(|r| r.output_delay()).unwrap_or(0);
        Ok(Self { dec, resampler, in_l: vec![], in_r: vec![], chunk, pending_delay, flushed: false, eof: false, tmp: vec![] })
    }

    pub fn source_duration_ms(&self) -> Option<i64> { self.dec.duration_ms() }

    /// Doplní do `out` další prokládané stereo vzorky. Vrací false = konec.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        out.clear();
        if self.resampler.is_none() {
            if self.eof { return Ok(false); }
            let more = self.dec.next_stereo(out)?;
            if !more { self.eof = true; }
            return Ok(more || !out.is_empty());
        }
        let rs = self.resampler.as_mut().unwrap();
        while self.in_l.len() < self.chunk && !self.eof {
            if !self.dec.next_stereo(&mut self.tmp)? { self.eof = true; break; }
            for fr in self.tmp.chunks_exact(2) { self.in_l.push(fr[0]); self.in_r.push(fr[1]); }
        }
        let res = if self.in_l.len() >= self.chunk {
            let l: Vec<f32> = self.in_l.drain(..self.chunk).collect();
            let r: Vec<f32> = self.in_r.drain(..self.chunk).collect();
            rs.process(&[l, r], None)?
        } else if !self.in_l.is_empty() {
            let l = std::mem::take(&mut self.in_l);
            let r = std::mem::take(&mut self.in_r);
            rs.process_partial(Some(&[l, r]), None)?
        } else if self.eof && !self.flushed {
            // vyprázdnění zpoždění resampleru
            self.flushed = true;
            rs.process_partial::<Vec<f32>>(None, None)?
        } else {
            return Ok(false);
        };
        let (ol, or) = (&res[0], &res[1]);
        let n = ol.len().min(or.len());
        let mut start = 0;
        if self.pending_delay > 0 {
            start = self.pending_delay.min(n);
            self.pending_delay -= start;
        }
        out.reserve((n - start) * 2);
        for i in start..n { out.push(ol[i]); out.push(or[i]); }
        Ok(true)
    }
}
