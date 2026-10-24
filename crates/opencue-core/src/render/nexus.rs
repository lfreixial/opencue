//! Screens for the 640x48 iCUE NEXUS strip.

use std::sync::Mutex;

use image::RgbaImage;
use image::imageops::FilterType;

use super::{Fonts, blend, fill_rect, fmt_temp, fonts, local_time, solid};
use crate::config::{NexusConfig, NexusMode, parse_color};
use crate::devices::nexus::{HEIGHT, WIDTH};
use crate::telemetry::{NowPlaying, Telemetry};

const GAP: i32 = 4;

/// Media mode transport buttons, left to right, as `Action::Media` names.
pub const MEDIA_CONTROLS: [&str; 5] = ["previous", "rewind", "play_pause", "forward", "next"];
const CONTROL_W: i32 = 44;
const CONTROLS_X0: i32 = WIDTH as i32 - GAP - MEDIA_CONTROLS.len() as i32 * (CONTROL_W + GAP) + GAP;
const ART: i32 = 40;
const INFO_X0: i32 = GAP + ART + 8;
const INFO_X1: i32 = CONTROLS_X0 - 10;

/// Horizontal extent [x0, x1) of button `i` of `n`.
pub fn button_span(i: usize, n: usize) -> (i32, i32) {
    let n = n.max(1) as i32;
    let w = (WIDTH as i32 - GAP * (n + 1)) / n;
    let x0 = GAP + i as i32 * (w + GAP);
    (x0, x0 + w)
}

/// Which button a touch at `x` (0..WIDTH) hits. Touches in the gaps go to the nearest button.
pub fn button_at(x: u16, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    Some(((x as usize * n) / WIDTH as usize).min(n - 1))
}

fn rounded_rect(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, r: f32, c: [u8; 3]) {
    for y in y0..y1 {
        for x in x0..x1 {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let cx = px.clamp(x0 as f32 + r, x1 as f32 - r);
            let cy = py.clamp(y0 as f32 + r, y1 as f32 - r);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let a = (r - d + 0.5).clamp(0.0, 1.0);
            if a > 0.0 {
                blend(img, x, y, c, a);
            }
        }
    }
}

/// What a touch at `x` does in media mode: an index into [`MEDIA_CONTROLS`], or `None` for the
/// track info area (which switches between players).
pub fn media_control_at(x: u16) -> Option<usize> {
    let x = x as i32;
    if x < CONTROLS_X0 - GAP / 2 {
        return None;
    }
    Some((((x - CONTROLS_X0 + GAP / 2) / (CONTROL_W + GAP)) as usize).min(MEDIA_CONTROLS.len() - 1))
}

/// Render the strip. `pressed` highlights the button currently being touched.
pub fn render(cfg: &NexusConfig, page: usize, pressed: Option<usize>, t: &Telemetry, fahrenheit: bool) -> RgbaImage {
    let mut img = solid(WIDTH, HEIGHT, [0, 0, 0]);
    let f = fonts();
    let h = HEIGHT as f32;
    match cfg.mode {
        NexusMode::Buttons => {
            let Some(p) = cfg.pages.get(page.min(cfg.pages.len().saturating_sub(1))) else { return img };
            let n = p.buttons.len();
            for (i, b) in p.buttons.iter().enumerate() {
                let (x0, x1) = button_span(i, n);
                let mut bg = parse_color(&b.color);
                if pressed == Some(i) {
                    bg = bg.map(|v| v.saturating_add(60));
                }
                rounded_rect(&mut img, x0, 3, x1, HEIGHT as i32 - 3, 8.0, bg);
                let size = f.fit_size(&b.label, 22.0, (x1 - x0 - 10) as f32);
                f.draw_centered(&mut img, &b.label, size, (x0 + x1) as f32 / 2.0, h / 2.0, parse_color(&b.text_color));
            }
            if cfg.pages.len() > 1 {
                // page dots along the bottom edge
                let total = cfg.pages.len() as i32;
                let start = WIDTH as i32 / 2 - total * 5;
                for i in 0..total {
                    let c = if i as usize == page { [0, 200, 255] } else { [70, 70, 80] };
                    fill_rect(&mut img, start + i * 10, HEIGHT as i32 - 3, 6, 2, c);
                }
            }
        }
        NexusMode::Sysinfo => {
            let cells = [
                ("CPU", fmt_temp(t.system.cpu_temp, fahrenheit), t.system.cpu_load),
                ("GPU", fmt_temp(t.system.gpu_temp, fahrenheit), t.system.gpu_load),
                ("LIQUID", fmt_temp(t.liquid_temp, fahrenheit), None),
            ];
            let w = WIDTH as f32 / 3.0;
            for (i, (label, temp, load)) in cells.iter().enumerate() {
                let cx = w * i as f32 + w / 2.0;
                f.draw_centered(&mut img, label, 13.0, cx, 11.0, [0, 200, 255]);
                let value = match load {
                    Some(l) => format!("{temp}  {l:.0}%"),
                    None => temp.clone(),
                };
                f.draw_centered(&mut img, &value, 22.0, cx, 31.0, [240, 240, 245]);
            }
        }
        NexusMode::Clock => {
            let (hh, mm, ss, date) = local_time();
            let text = format!("{hh:02}:{mm:02}:{ss:02}    {date}");
            f.draw_centered(&mut img, &text, 28.0, WIDTH as f32 / 2.0, h / 2.0, [240, 240, 245]);
        }
        NexusMode::Media => media(&mut img, f, t.now_playing.as_ref(), pressed),
    }
    img
}

const TEXT: [u8; 3] = [240, 240, 245];
const DIM: [u8; 3] = [150, 150, 165];
const ACCENT: [u8; 3] = [0, 200, 255];
const BUTTON: [u8; 3] = [30, 30, 40];

fn media(img: &mut RgbaImage, f: &Fonts, np: Option<&NowPlaying>, pressed: Option<usize>) {
    let art_y = (HEIGHT as i32 - ART) / 2;
    match np.and_then(|np| np.art.as_deref()).and_then(cover) {
        Some(art) => draw_rounded_image(img, &art, GAP, art_y, 5.0),
        None => {
            rounded_rect(img, GAP, art_y, GAP + ART, art_y + ART, 5.0, BUTTON);
            f.draw_centered(img, "♪", 24.0, (GAP + ART / 2) as f32, (HEIGHT / 2) as f32, DIM);
        }
    }

    let width = (INFO_X1 - INFO_X0) as f32;
    match np {
        None => {
            f.draw(img, "Nothing playing", 15.0, INFO_X0 as f32, 22.0, DIM);
            f.draw(img, "Start music or a video in any app", 11.0, INFO_X0 as f32, 37.0, [90, 90, 105]);
        }
        Some(np) => {
            let title = if np.title.is_empty() { &np.player } else { &np.title };
            f.draw(img, &ellipsize(f, title, 15.0, width), 15.0, INFO_X0 as f32, 17.0, TEXT);
            let sub: Vec<&str> =
                [np.artist.as_str(), np.album.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
            let sub = match (sub.is_empty(), np.title.is_empty()) {
                (false, _) => sub.join(" — "),
                (true, false) => np.player.clone(),
                (true, true) if np.playing => "Playing".into(),
                (true, true) => "Not playing".into(),
            };
            f.draw(img, &ellipsize(f, &sub, 11.0, width), 11.0, INFO_X0 as f32, 30.0, DIM);

            let pos = np.position_now_us();
            let time = match np.length_us {
                Some(len) if len > 0 => format!("{} / {}", fmt_duration(pos), fmt_duration(len)),
                _ => fmt_duration(pos),
            };
            let time_w = f.text_width(&time, 10.0).ceil() as i32;
            f.draw(img, &time, 10.0, (INFO_X1 - time_w) as f32, 43.0, DIM);
            let (bx0, bx1) = (INFO_X0, INFO_X1 - time_w - 8);
            if let Some(len) = np.length_us.filter(|&l| l > 0)
                && bx1 > bx0
            {
                rounded_rect(img, bx0, 38, bx1, 41, 1.5, [55, 55, 70]);
                let fill = ((bx1 - bx0) as f64 * (pos as f64 / len as f64).clamp(0.0, 1.0)).round() as i32;
                if fill > 0 {
                    rounded_rect(img, bx0, 38, bx0 + fill, 41, 1.5, ACCENT);
                }
            }
        }
    }

    let playing = np.is_some_and(|np| np.playing);
    let icon = if np.is_some() { TEXT } else { [80, 80, 95] };
    for (i, name) in MEDIA_CONTROLS.iter().enumerate() {
        let x0 = CONTROLS_X0 + i as i32 * (CONTROL_W + GAP);
        let mut bg = if *name == "play_pause" && np.is_some() { [0, 90, 120] } else { BUTTON };
        if pressed == Some(i) {
            bg = bg.map(|v| v.saturating_add(60));
        }
        rounded_rect(img, x0, 4, x0 + CONTROL_W, HEIGHT as i32 - 4, 8.0, bg);
        let (cx, cy) = ((x0 + CONTROL_W / 2) as f32, HEIGHT as f32 / 2.0);
        match *name {
            "previous" => {
                fill_rect(img, cx as i32 - 7, cy as i32 - 6, 2, 12, icon);
                triangle(img, [(cx + 6.0, cy - 6.0), (cx + 6.0, cy + 6.0), (cx - 4.0, cy)], icon);
            }
            "next" => {
                triangle(img, [(cx - 6.0, cy - 6.0), (cx - 6.0, cy + 6.0), (cx + 4.0, cy)], icon);
                fill_rect(img, cx as i32 + 5, cy as i32 - 6, 2, 12, icon);
            }
            "rewind" => {
                for dx in [0.0, 8.0] {
                    triangle(img, [(cx + dx, cy - 6.0), (cx + dx, cy + 6.0), (cx + dx - 8.0, cy)], icon);
                }
            }
            "forward" => {
                for dx in [0.0, -8.0] {
                    triangle(img, [(cx + dx, cy - 6.0), (cx + dx, cy + 6.0), (cx + dx + 8.0, cy)], icon);
                }
            }
            _ if playing => {
                fill_rect(img, cx as i32 - 6, cy as i32 - 7, 4, 14, icon);
                fill_rect(img, cx as i32 + 2, cy as i32 - 7, 4, 14, icon);
            }
            _ => triangle(img, [(cx - 5.0, cy - 8.0), (cx - 5.0, cy + 8.0), (cx + 8.0, cy)], icon),
        }
    }
}

/// "3:07" or "1:02:45".
fn fmt_duration(us: u64) -> String {
    let s = us / 1_000_000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Cut `text` to fit `width`, ending in "…" when shortened.
fn ellipsize(f: &Fonts, text: &str, size: f32, width: f32) -> String {
    if f.text_width(text, size) <= width {
        return text.into();
    }
    let mut out: String = text.into();
    while !out.is_empty() && f.text_width(&format!("{out}…"), size) > width {
        out.pop();
    }
    format!("{}…", out.trim_end())
}

/// Antialiased filled triangle (4x4 supersampling).
fn triangle(img: &mut RgbaImage, p: [(f32, f32); 3], c: [u8; 3]) {
    let edge = |a: (f32, f32), b: (f32, f32), x: f32, y: f32| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
    let area = edge(p[0], p[1], p[2].0, p[2].1);
    let (x0, x1) = (p.iter().map(|q| q.0).fold(f32::MAX, f32::min), p.iter().map(|q| q.0).fold(f32::MIN, f32::max));
    let (y0, y1) = (p.iter().map(|q| q.1).fold(f32::MAX, f32::min), p.iter().map(|q| q.1).fold(f32::MIN, f32::max));
    for y in y0.floor() as i32..y1.ceil() as i32 {
        for x in x0.floor() as i32..x1.ceil() as i32 {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (px, py) = (x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                    let w = [edge(p[0], p[1], px, py), edge(p[1], p[2], px, py), edge(p[2], p[0], px, py)];
                    if w.iter().all(|&v| v * area >= 0.0) {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                blend(img, x, y, c, hits as f32 / 16.0);
            }
        }
    }
}

fn draw_rounded_image(img: &mut RgbaImage, src: &RgbaImage, x0: i32, y0: i32, r: f32) {
    let (w, h) = (src.width() as f32, src.height() as f32);
    for (sx, sy, p) in src.enumerate_pixels() {
        let (px, py) = (sx as f32 + 0.5, sy as f32 + 0.5);
        let (cx, cy) = (px.clamp(r, w - r), py.clamp(r, h - r));
        let a = (r - ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() + 0.5).clamp(0.0, 1.0) * p.0[3] as f32 / 255.0;
        if a > 0.0 {
            blend(img, x0 + sx as i32, y0 + sy as i32, [p.0[0], p.0[1], p.0[2]], a);
        }
    }
}

/// Cover art scaled for the strip; the last image is cached so redraws don't decode it again.
fn cover(path: &str) -> Option<RgbaImage> {
    static CACHE: Mutex<Option<(String, Option<RgbaImage>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    if cache.as_ref().is_none_or(|(p, _)| p != path) {
        let img = image::ImageReader::open(path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.decode().ok())
            .map(|i| i.resize_to_fill(ART as u32, ART as u32, FilterType::Triangle).to_rgba8());
        *cache = Some((path.into(), img));
    }
    cache.as_ref().and_then(|(_, img)| img.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_maps_to_buttons() {
        assert_eq!(button_at(0, 4), Some(0));
        assert_eq!(button_at(639, 4), Some(3));
        assert_eq!(button_at(320, 4), Some(2));
        assert_eq!(button_at(5000, 4), Some(3));
        assert_eq!(button_at(10, 0), None);
    }

    #[test]
    fn touch_maps_to_media_controls() {
        assert_eq!(media_control_at(0), None);
        assert_eq!(media_control_at(300), None);
        assert_eq!(media_control_at(CONTROLS_X0 as u16 + 5), Some(0));
        assert_eq!(media_control_at(639), Some(4));
        let middle = CONTROLS_X0 + 2 * (CONTROL_W + GAP) + CONTROL_W / 2;
        assert_eq!(media_control_at(middle as u16), Some(2));
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(187_000_000), "3:07");
        assert_eq!(fmt_duration(3_765_000_000), "1:02:45");
    }
}
