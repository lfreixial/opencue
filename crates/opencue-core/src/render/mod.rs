//! Shared drawing helpers for the LCD and Nexus renderers (used by the daemon and GUI previews).

pub mod lcd;
pub mod nexus;

use std::sync::OnceLock;

use fontdue::{Font, FontSettings};
use image::{Rgba, RgbaImage};

const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
];

const FALLBACK_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

pub struct Fonts {
    main: Font,
    fallbacks: Vec<Font>,
}

fn load(path: &str) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

fn fc_match(pattern: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    (!path.is_empty()).then_some(path)
}

pub fn fonts() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(|| {
        let main = FONT_CANDIDATES
            .iter()
            .find_map(|p| load(p))
            .or_else(|| fc_match("sans:bold").and_then(|p| load(&p)))
            .expect("no usable TrueType font found (install noto-fonts or ttf-dejavu)");
        let fallbacks = FALLBACK_CANDIDATES.iter().filter_map(|p| load(p)).collect();
        Fonts { main, fallbacks }
    })
}

impl Fonts {
    fn font_for(&self, ch: char) -> &Font {
        if self.main.has_glyph(ch) {
            return &self.main;
        }
        self.fallbacks.iter().find(|f| f.has_glyph(ch)).unwrap_or(&self.main)
    }

    pub fn text_width(&self, text: &str, size: f32) -> f32 {
        text.chars().map(|c| self.font_for(c).metrics(c, size).advance_width).sum()
    }

    /// Draw text with its horizontal centre at `cx` and its vertical centre (cap height) at `cy`.
    pub fn draw_centered(&self, img: &mut RgbaImage, text: &str, size: f32, cx: f32, cy: f32, color: [u8; 3]) {
        let x = cx - self.text_width(text, size) / 2.0;
        self.draw(img, text, size, x, cy + size * 0.36, color);
    }

    /// Draw text with its baseline at `y`.
    pub fn draw(&self, img: &mut RgbaImage, text: &str, size: f32, mut x: f32, y: f32, color: [u8; 3]) {
        for ch in text.chars() {
            let font = self.font_for(ch);
            let (m, bitmap) = font.rasterize(ch, size);
            let gx = x.round() as i32 + m.xmin;
            let gy = y.round() as i32 - m.height as i32 - m.ymin;
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = bitmap[row * m.width + col];
                    if a > 0 {
                        blend(img, gx + col as i32, gy + row as i32, color, a as f32 / 255.0);
                    }
                }
            }
            x += m.advance_width;
        }
    }

    /// Largest size (<= max) at which `text` fits in `width`.
    pub fn fit_size(&self, text: &str, max: f32, width: f32) -> f32 {
        let w = self.text_width(text, max);
        if w <= width || w == 0.0 { max } else { (max * width / w).max(6.0) }
    }
}

pub fn blend(img: &mut RgbaImage, x: i32, y: i32, c: [u8; 3], a: f32) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
        return;
    }
    let p = img.get_pixel_mut(x as u32, y as u32);
    for (dst, &src) in p.0.iter_mut().zip(&c) {
        *dst = (*dst as f32 + (src as f32 - *dst as f32) * a).round() as u8;
    }
    p.0[3] = 255;
}

pub fn fill_rect(img: &mut RgbaImage, x: i32, y: i32, w: i32, h: i32, c: [u8; 3]) {
    for yy in y.max(0)..(y + h).min(img.height() as i32) {
        for xx in x.max(0)..(x + w).min(img.width() as i32) {
            img.put_pixel(xx as u32, yy as u32, Rgba([c[0], c[1], c[2], 255]));
        }
    }
}

pub fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbaImage {
    RgbaImage::from_pixel(w, h, Rgba([c[0], c[1], c[2], 255]))
}

/// Local wall-clock time as (hour, minute, second, "Mon 23 Oct").
pub fn local_time() -> (u32, u32, u32, String) {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let date = format!("{} {} {}", DAYS[tm.tm_wday as usize % 7], tm.tm_mday, MONTHS[tm.tm_mon as usize % 12]);
    (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32, date)
}

pub fn fmt_temp(c: Option<f32>, fahrenheit: bool) -> String {
    match c {
        None => "--".into(),
        Some(v) if fahrenheit => format!("{:.0}°F", v * 9.0 / 5.0 + 32.0),
        Some(v) => format!("{v:.1}°C"),
    }
}
