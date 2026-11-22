//! Generování slidů pro DAB+ MOT Slideshow (320×240 JPEG ≤ 50 kB): obal alba, moderátor, pořad, počasí, stanice.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::Result;
use image::{imageops, DynamicImage, Rgb, RgbImage};

static FONT_REG: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");

fn font(bold: bool) -> FontRef<'static> { FontRef::try_from_slice(if bold { FONT_BOLD } else { FONT_REG }).expect("font") }

pub fn parse_color(s: &str) -> Rgb<u8> {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (u8::from_str_radix(&h[0..2], 16), u8::from_str_radix(&h[2..4], 16), u8::from_str_radix(&h[4..6], 16)) { return Rgb([r, g, b]); }
    }
    Rgb([15, 23, 42])
}

fn mix(a: Rgb<u8>, b: Rgb<u8>, t: f32) -> Rgb<u8> {
    Rgb([0, 1, 2].map(|i| (a.0[i] as f32 * (1.0 - t) + b.0[i] as f32 * t).round().clamp(0.0, 255.0) as u8))
}

fn text_width(f: &FontRef, px: f32, s: &str) -> f32 {
    let sf = f.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for c in s.chars() {
        let g = sf.glyph_id(c);
        if let Some(p) = prev { w += sf.kern(p, g); }
        w += sf.h_advance(g);
        prev = Some(g);
    }
    w
}

/// Vykreslí jeden řádek textu (y = horní okraj řádku).
fn draw_line(img: &mut RgbImage, f: &FontRef, px: f32, x: f32, y: f32, color: Rgb<u8>, s: &str) {
    let sf = f.as_scaled(PxScale::from(px));
    let base = y + sf.ascent();
    let mut cx = x;
    let mut prev = None;
    let (w, h) = (img.width() as i32, img.height() as i32);
    for c in s.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev { cx += sf.kern(p, id); }
        let g = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(cx, base));
        if let Some(o) = f.outline_glyph(g) {
            let bb = o.px_bounds();
            o.draw(|gx, gy, cov| {
                let px_ = bb.min.x as i32 + gx as i32;
                let py_ = bb.min.y as i32 + gy as i32;
                if px_ >= 0 && py_ >= 0 && px_ < w && py_ < h {
                    let p = img.get_pixel_mut(px_ as u32, py_ as u32);
                    *p = mix(*p, color, cov.min(1.0));
                }
            });
        }
        cx += sf.h_advance(id);
        prev = Some(id);
    }
}

/// Zalomení textu do řádků dané šířky, max `max_lines` (poslední s „…“).
fn wrap(f: &FontRef, px: f32, s: &str, width: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if text_width(f, px, &cand) <= width { cur = cand; continue; }
        if !cur.is_empty() { lines.push(std::mem::take(&mut cur)); }
        // příliš dlouhé slovo – zkrátit
        let mut w = word.to_string();
        while text_width(f, px, &w) > width && w.chars().count() > 1 { w.pop(); }
        cur = w;
    }
    if !cur.is_empty() { lines.push(cur); }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.last_mut().unwrap();
        while text_width(f, px, &format!("{last}…")) > width && !last.is_empty() { last.pop(); }
        last.push('…');
    }
    lines
}

/// Text v bloku; vrací y pod posledním řádkem.
fn draw_block(img: &mut RgbImage, bold: bool, px: f32, x: f32, y: f32, width: f32, max_lines: usize, color: Rgb<u8>, s: &str) -> f32 {
    let f = font(bold);
    let mut yy = y;
    for l in wrap(&f, px, s, width, max_lines) { draw_line(img, &f, px, x, yy, color, &l); yy += px * 1.2; }
    yy
}

fn draw_centered(img: &mut RgbImage, bold: bool, px: f32, y: f32, color: Rgb<u8>, s: &str) {
    let f = font(bold);
    let w = img.width() as f32;
    let lines = wrap(&f, px, s, w - 24.0, 2);
    let mut yy = y;
    for l in lines {
        let tw = text_width(&f, px, &l);
        draw_line(img, &f, px, ((w - tw) / 2.0).max(4.0), yy, color, &l);
        yy += px * 1.2;
    }
}

fn fill(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, c: Rgb<u8>) {
    for yy in y..(y + h).min(img.height()) { for xx in x..(x + w).min(img.width()) { img.put_pixel(xx, yy, c); } }
}

/// Svislý přechod pozadí.
fn background(w: u32, h: u32, base: Rgb<u8>) -> RgbImage {
    let top = mix(base, Rgb([255, 255, 255]), 0.08);
    let bot = mix(base, Rgb([0, 0, 0]), 0.35);
    RgbImage::from_fn(w, h, |_, y| mix(top, bot, y as f32 / h as f32))
}

/// Obrázek vyplní čtverec/obdélník (oříznutí na střed).
fn cover_fit(src: &DynamicImage, w: u32, h: u32) -> RgbImage {
    let (sw, sh) = (src.width().max(1), src.height().max(1));
    let scale = (w as f32 / sw as f32).max(h as f32 / sh as f32);
    let (nw, nh) = (((sw as f32 * scale).ceil() as u32).max(w), ((sh as f32 * scale).ceil() as u32).max(h));
    let r = src.resize_exact(nw, nh, imageops::FilterType::Triangle).to_rgb8();
    imageops::crop_imm(&r, (nw - w) / 2, (nh - h) / 2, w, h).to_image()
}

/// Obrázek se vejde do obdélníku (bez ořezu).
fn contain(src: &DynamicImage, w: u32, h: u32) -> RgbImage {
    src.resize(w, h, imageops::FilterType::Triangle).to_rgb8()
}

fn paste(dst: &mut RgbImage, src: &RgbImage, x: u32, y: u32) { imageops::overlay(dst, src, x as i64, y as i64); }

pub fn load_image(bytes: &[u8]) -> Option<DynamicImage> { image::load_from_memory(bytes).ok() }
pub fn load_path(p: &str) -> Option<DynamicImage> { if p.is_empty() { None } else { std::fs::read(p).ok().and_then(|b| load_image(&b)) } }

/// Převod libovolného nahraného obrázku (JPEG, PNG, WebP, GIF, BMP, TIFF; HEIC/AVIF přes ffmpeg)
/// na uložitelný soubor: zmenší na max. 2000 px, s průhledností PNG, jinak JPEG. Vrací (data, přípona).
pub fn normalize_upload(bytes: &[u8], ffmpeg: &std::path::Path) -> Result<(Vec<u8>, &'static str)> {
    if bytes.is_empty() { anyhow::bail!("prázdný soubor"); }
    let img = match image::load_from_memory(bytes) {
        Ok(i) => i,
        Err(_) => {
            // např. HEIC z iPhonu / Macu – převod přes ffmpeg
            let dir = std::env::temp_dir().join(format!("orca-img-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir)?;
            let (inp, out) = (dir.join("in.bin"), dir.join("out.png"));
            std::fs::write(&inp, bytes)?;
            let ok = std::process::Command::new(ffmpeg).args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(&inp).args(["-frames:v", "1"]).arg(&out)
                .stdin(std::process::Stdio::null()).output().map(|o| o.status.success()).unwrap_or(false);
            let r = if ok { std::fs::read(&out).ok().and_then(|b| image::load_from_memory(&b).ok()) } else { None };
            let _ = std::fs::remove_dir_all(&dir);
            r.ok_or_else(|| anyhow::anyhow!("soubor není podporovaný obrázek (JPG, PNG, WebP, GIF, BMP, TIFF, HEIC)"))?
        }
    };
    let img = if img.width() > 2000 || img.height() > 2000 { img.resize(2000, 2000, image::imageops::FilterType::Lanczos3) } else { img };
    let alpha = img.color().has_alpha();
    let mut buf = std::io::Cursor::new(vec![]);
    if alpha {
        img.write_to(&mut buf, image::ImageFormat::Png)?;
        Ok((buf.into_inner(), "png"))
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90).encode_image(&img.to_rgb8())?;
        Ok((buf.into_inner(), "jpg"))
    }
}

/// Zakódování do JPEG tak, aby se vešlo do limitu velikosti.
pub fn encode_jpeg(img: &RgbImage, max_kb: u32) -> Result<Vec<u8>> {
    let mut q = 88u8;
    loop {
        let mut buf = vec![];
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q).encode_image(img)?;
        if buf.len() <= max_kb.max(5) as usize * 1024 || q <= 30 { return Ok(buf); }
        q -= 8;
    }
}

pub struct Common<'a> {
    pub w: u32,
    pub h: u32,
    pub brand: Rgb<u8>,
    pub logo: Option<&'a DynamicImage>,
    pub station: &'a str,
    /// Spodní lišta (např. „Ranní show · s Petrou“)
    pub footer: &'a str,
}

fn footer(img: &mut RgbImage, c: &Common) {
    let (w, h) = (c.w, c.h);
    let bh = (h as f32 * 0.17) as u32;
    let bar = mix(c.brand, Rgb([0, 0, 0]), 0.55);
    fill(img, 0, h - bh, w, bh, bar);
    let mut tx = 10.0;
    if let Some(l) = c.logo {
        let li = contain(l, (w as f32 * 0.25) as u32, bh - 8);
        paste(img, &li, w - li.width() - 8, h - bh + (bh - li.height()) / 2);
        let _ = &mut tx;
    }
    let txt = if c.footer.is_empty() { c.station } else { c.footer };
    let px = bh as f32 * 0.42;
    let maxw = w as f32 * if c.logo.is_some() { 0.68 } else { 0.92 };
    let f = font(true);
    let line = wrap(&f, px, txt, maxw, 1).into_iter().next().unwrap_or_default();
    draw_line(img, &f, px, tx, (h - bh) as f32 + (bh as f32 - px * 1.15) / 2.0, Rgb([255, 255, 255]), &line);
}

/// Slide „právě hraje“: obal + interpret + název.
pub fn now_playing(c: &Common, cover: Option<&DynamicImage>, label: &str, artist: &str, title: &str) -> RgbImage {
    let (w, h) = (c.w, c.h);
    let mut img = background(w, h, c.brand);
    let body_h = h - (h as f32 * 0.17) as u32;
    let side = (body_h as f32 * 0.86) as u32;
    let pad = (body_h - side) / 2;
    let text_x;
    match cover.or(c.logo) {
        Some(cv) => { let ci = cover_fit(cv, side, side); paste(&mut img, &ci, pad, pad); text_x = (pad * 2 + side) as f32; }
        None => text_x = 12.0,
    }
    let tw = w as f32 - text_x - 10.0;
    let mut y = pad as f32 + 2.0;
    y = draw_block(&mut img, false, h as f32 * 0.058, text_x, y, tw, 1, Rgb([190, 205, 230]), label) + 4.0;
    y = draw_block(&mut img, true, h as f32 * 0.078, text_x, y, tw, 3, Rgb([255, 255, 255]), artist) + 4.0;
    draw_block(&mut img, false, h as f32 * 0.07, text_x, y, tw, 3, Rgb([225, 232, 245]), title);
    footer(&mut img, c);
    img
}

/// Slide moderátora / pořadu: fotka + texty.
pub fn person(c: &Common, photo: Option<&DynamicImage>, label: &str, name: &str, detail: &str) -> RgbImage {
    now_playing(c, photo, label, name, detail)
}

/// Slide pořadu: obrázek přes celé pozadí (ztmavený) + název a čas.
pub fn show(c: &Common, image: Option<&DynamicImage>, label: &str, name: &str, time: &str, desc: &str) -> RgbImage {
    let (w, h) = (c.w, c.h);
    let mut img = match image {
        Some(i) => { let mut b = cover_fit(i, w, h); for p in b.pixels_mut() { *p = mix(*p, Rgb([0, 0, 0]), 0.55); } b }
        None => background(w, h, c.brand),
    };
    let mut y = h as f32 * 0.1;
    y = draw_block(&mut img, false, h as f32 * 0.06, 14.0, y, w as f32 - 28.0, 1, Rgb([200, 215, 240]), label) + 2.0;
    y = draw_block(&mut img, true, h as f32 * 0.11, 14.0, y, w as f32 - 28.0, 2, Rgb([255, 255, 255]), name) + 2.0;
    y = draw_block(&mut img, true, h as f32 * 0.065, 14.0, y, w as f32 - 28.0, 1, Rgb([250, 204, 21]), time) + 4.0;
    draw_block(&mut img, false, h as f32 * 0.058, 14.0, y, w as f32 - 28.0, 3, Rgb([230, 236, 245]), desc);
    footer(&mut img, c);
    img
}

/// Slide počasí.
pub fn weather(c: &Common, place: &str, temp: &str, text: &str, minmax: &str, label: &str) -> RgbImage {
    let (w, h) = (c.w, c.h);
    let t: f32 = temp.replace(',', ".").parse().unwrap_or(10.0);
    let tint = if t < 0.0 { Rgb([30, 64, 175]) } else if t < 12.0 { Rgb([14, 116, 144]) } else if t < 24.0 { Rgb([21, 128, 61]) } else { Rgb([194, 65, 12]) };
    let mut img = background(w, h, mix(c.brand, tint, 0.6));
    let mut y = h as f32 * 0.07;
    y = draw_block(&mut img, false, h as f32 * 0.062, 14.0, y, w as f32 - 28.0, 1, Rgb([220, 230, 245]), label);
    y = draw_block(&mut img, true, h as f32 * 0.085, 14.0, y, w as f32 - 28.0, 1, Rgb([255, 255, 255]), place);
    if !temp.is_empty() {
        let big = format!("{temp} °C");
        let f = font(true);
        let px = h as f32 * 0.25;
        draw_line(&mut img, &f, px, 14.0, y, Rgb([255, 255, 255]), &big);
        y += px * 1.12;
        y = draw_block(&mut img, false, h as f32 * 0.072, 14.0, y, w as f32 - 28.0, 2, Rgb([240, 245, 250]), text);
    } else {
        y = draw_block(&mut img, true, h as f32 * 0.1, 14.0, y + 10.0, w as f32 - 28.0, 3, Rgb([255, 255, 255]), text);
    }
    draw_block(&mut img, false, h as f32 * 0.06, 14.0, y + 2.0, w as f32 - 28.0, 1, Rgb([210, 225, 240]), minmax);
    footer(&mut img, c);
    img
}

/// Slide stanice: logo + slogan + web.
pub fn station(c: &Common, slogan: &str, url: &str) -> RgbImage {
    let (w, h) = (c.w, c.h);
    let mut img = background(w, h, c.brand);
    let mut y = h as f32 * 0.12;
    match c.logo {
        Some(l) => { let li = contain(l, (w as f32 * 0.72) as u32, (h as f32 * 0.42) as u32); paste(&mut img, &li, (w - li.width()) / 2, y as u32); y += li.height() as f32 + 10.0; }
        None => { draw_centered(&mut img, true, h as f32 * 0.13, y, Rgb([255, 255, 255]), c.station); y += h as f32 * 0.32; }
    }
    if !slogan.is_empty() { draw_centered(&mut img, false, h as f32 * 0.075, y, Rgb([235, 240, 250]), slogan); y += h as f32 * 0.2; }
    if !url.is_empty() { draw_centered(&mut img, true, h as f32 * 0.065, y, Rgb([250, 204, 21]), url); }
    img
}

/// Celoplošný obrázek (reklama): vložený do rozměru slidu bez textů, okraje v barvě stanice.
pub fn full(c: &Common, src: &DynamicImage) -> RgbImage {
    let mut canvas = RgbImage::from_pixel(c.w, c.h, mix(c.brand, Rgb([0, 0, 0]), 0.4));
    let li = contain(src, c.w, c.h);
    paste(&mut canvas, &li, (c.w - li.width()) / 2, (c.h - li.height()) / 2);
    canvas
}

/// Zmenšený obrázek pro EPG (logo, fotka, obrázek pořadu): přesné rozměry, PNG/JPEG.
pub fn resized(src: &DynamicImage, w: u32, h: u32, png: bool, bg: Rgb<u8>) -> Result<Vec<u8>> {
    let mut canvas = RgbImage::from_pixel(w, h, bg);
    let li = contain(src, w, h);
    paste(&mut canvas, &li, (w - li.width()) / 2, (h - li.height()) / 2);
    let mut buf = std::io::Cursor::new(vec![]);
    if png { DynamicImage::ImageRgb8(canvas).write_to(&mut buf, image::ImageFormat::Png)?; Ok(buf.into_inner()) } else { encode_jpeg(&canvas, 200) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn render_all_under_limit() {
        let c = Common { w: 320, h: 240, brand: parse_color("#0f172a"), logo: None, station: "ORCA RadioAIR", footer: "Ranní show · s Petrou Novou" };
        for img in [
            now_playing(&c, None, "Právě hraje", "Lucie", "Medvídek – dlouhý název písně se zalomením"),
            show(&c, None, "Právě vysíláme", "Ranní show", "06:00–09:00", "Dobré ráno s hudbou, zprávami a dopravou."),
            weather(&c, "Jihlava", "7", "Polojasno, slabý vítr", "min 2 / max 9 °C", "Počasí"),
            station(&c, "Vaše rádio z Vysočiny", "www.orca.cz"),
        ] {
            assert_eq!((img.width(), img.height()), (320, 240));
            let j = encode_jpeg(&img, 50).unwrap();
            assert!(j.len() <= 50 * 1024);
        }
    }
}
