//! Screens for the round 480x480 pump LCD.

use std::path::Path;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageReader, RgbaImage};

use super::{fmt_temp, fonts, local_time, solid};
use crate::config::{LcdConfig, LcdMode, parse_color};
use crate::devices::lcd_cap::SIZE;
use crate::telemetry::Telemetry;

const MAX_FRAMES: usize = 900;

pub struct Frame {
    pub image: RgbaImage,
    pub delay: Duration,
}

/// Decode an image or animated GIF into 480x480 frames.
pub fn load_media(path: &Path, cover: bool, background: [u8; 3]) -> anyhow::Result<Vec<Frame>> {
    let is_gif = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gif"));
    let raw: Vec<(RgbaImage, Duration)> = if is_gif {
        let decoder = GifDecoder::new(std::io::BufReader::new(std::fs::File::open(path)?))?;
        decoder
            .into_frames()
            .take(MAX_FRAMES)
            .map(|f| {
                f.map(|f| {
                    let (n, d) = f.delay().numer_denom_ms();
                    let ms = n.checked_div(d).unwrap_or(100);
                    // Browsers treat 0-10 ms GIF delays as 100 ms; match that.
                    let ms = if ms <= 10 { 100 } else { ms };
                    (f.into_buffer(), Duration::from_millis(ms as u64))
                })
            })
            .collect::<Result<_, _>>()?
    } else {
        let img = ImageReader::open(path)?.with_guessed_format()?.decode()?;
        vec![(img.to_rgba8(), Duration::from_secs(1))]
    };
    anyhow::ensure!(!raw.is_empty(), "image has no frames");
    Ok(raw.into_iter().map(|(img, delay)| Frame { image: fit(img, cover, background), delay }).collect())
}

fn fit(img: RgbaImage, cover: bool, background: [u8; 3]) -> RgbaImage {
    let (w, h) = img.dimensions();
    let dynimg = DynamicImage::ImageRgba8(img);
    if cover {
        let side = w.min(h);
        let cropped = dynimg.crop_imm((w - side) / 2, (h - side) / 2, side, side);
        cropped.resize_exact(SIZE, SIZE, FilterType::Triangle).to_rgba8()
    } else {
        let scaled = dynimg.resize(SIZE, SIZE, FilterType::Triangle).to_rgba8();
        let mut canvas = solid(SIZE, SIZE, background);
        let (x, y) = ((SIZE - scaled.width()) / 2, (SIZE - scaled.height()) / 2);
        image::imageops::overlay(&mut canvas, &scaled, x as i64, y as i64);
        canvas
    }
}

pub fn encode_jpeg(img: &RgbaImage) -> Vec<u8> {
    let rgb = DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let mut out = Vec::with_capacity(64 * 1024);
    JpegEncoder::new_with_quality(&mut out, 90).encode_image(&rgb).expect("in-memory JPEG encoding");
    out
}

/// Ring gauge: 270° arc, value 0..1, drawn anti-aliased.
fn draw_gauge(img: &mut RgbaImage, value: f32, accent: [u8; 3], track: [u8; 3]) {
    let c = SIZE as f32 / 2.0;
    let (r_out, r_in) = (c - 22.0, c - 52.0);
    let start = 135f32.to_radians();
    let sweep = 270f32.to_radians();
    let filled = sweep * value.clamp(0.0, 1.0);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let r = (dx * dx + dy * dy).sqrt();
            let edge = (r - r_in).min(r_out - r).min(1.0);
            if edge <= -1.0 {
                continue;
            }
            let a = (dy.atan2(dx) - start).rem_euclid(std::f32::consts::TAU);
            if a > sweep {
                continue;
            }
            let color = if a <= filled {
                // brighten towards the head of the arc
                let k = 0.55 + 0.45 * (a / filled.max(0.001));
                accent.map(|v| (v as f32 * k) as u8)
            } else {
                track
            };
            super::blend(img, x as i32, y as i32, color, ((edge + 1.0) / 2.0).clamp(0.0, 1.0));
        }
    }
}

pub fn render(cfg: &LcdConfig, t: &Telemetry, fahrenheit: bool) -> RgbaImage {
    let bg = parse_color(&cfg.background);
    let accent = parse_color(&cfg.accent);
    let track = [40, 40, 48];
    let mut img = solid(SIZE, SIZE, bg);
    let f = fonts();
    let c = SIZE as f32 / 2.0;
    let white = [240, 240, 245];
    let grey = [150, 150, 160];

    let gauge = |img: &mut RgbaImage, value: Option<f32>, lo: f32, hi: f32, text: String, label: &str, sub: String| {
        draw_gauge(img, value.map_or(0.0, |v| (v - lo) / (hi - lo)), accent, track);
        let size = f.fit_size(&text, 110.0, 300.0);
        f.draw_centered(img, &text, size, c, c - 10.0, white);
        f.draw_centered(img, label, 30.0, c, c + 75.0, accent);
        if !sub.is_empty() {
            f.draw_centered(img, &sub, 26.0, c, c + 115.0, grey);
        }
    };

    match cfg.mode {
        LcdMode::Liquid | LcdMode::Image => {
            gauge(&mut img, t.liquid_temp, 20.0, 60.0, fmt_temp(t.liquid_temp, fahrenheit), "LIQUID", pump_line(t))
        }
        LcdMode::Cpu => gauge(
            &mut img,
            t.system.cpu_temp,
            20.0,
            100.0,
            fmt_temp(t.system.cpu_temp, fahrenheit),
            "CPU",
            t.system.cpu_load.map(|l| format!("{l:.0}% load")).unwrap_or_default(),
        ),
        LcdMode::Gpu => gauge(
            &mut img,
            t.system.gpu_temp,
            20.0,
            100.0,
            fmt_temp(t.system.gpu_temp, fahrenheit),
            "GPU",
            t.system.gpu_load.map(|l| format!("{l:.0}% load")).unwrap_or_default(),
        ),
        LcdMode::Pump => {
            let rpm = t.pump_rpm();
            gauge(
                &mut img,
                rpm.map(|r| r as f32),
                0.0,
                3000.0,
                rpm.map_or("--".into(), |r| r.to_string()),
                "PUMP RPM",
                fmt_temp(t.liquid_temp, fahrenheit),
            )
        }
        LcdMode::Dashboard => {
            draw_gauge(&mut img, t.liquid_temp.map_or(0.0, |v| (v - 20.0) / 40.0), accent, track);
            f.draw_centered(&mut img, "LIQUID", 24.0, c, 120.0, accent);
            f.draw_centered(&mut img, &fmt_temp(t.liquid_temp, fahrenheit), 84.0, c, 185.0, white);
            f.draw_centered(&mut img, "CPU", 24.0, c - 85.0, 285.0, grey);
            f.draw_centered(&mut img, &fmt_temp(t.system.cpu_temp, fahrenheit), 44.0, c - 85.0, 330.0, white);
            f.draw_centered(&mut img, "GPU", 24.0, c + 85.0, 285.0, grey);
            f.draw_centered(&mut img, &fmt_temp(t.system.gpu_temp, fahrenheit), 44.0, c + 85.0, 330.0, white);
            if let Some(rpm) = t.pump_rpm() {
                f.draw_centered(&mut img, &format!("{rpm} RPM"), 22.0, c, 385.0, grey);
            }
        }
        LcdMode::Clock => {
            let (h, m, s, date) = local_time();
            draw_gauge(&mut img, s as f32 / 60.0, accent, track);
            f.draw_centered(&mut img, &format!("{h:02}:{m:02}"), 120.0, c, c - 10.0, white);
            f.draw_centered(&mut img, &date, 32.0, c, c + 80.0, accent);
        }
        LcdMode::Text => {
            let lines: Vec<&str> = cfg.text.lines().take(4).collect();
            let n = lines.len().max(1) as f32;
            for (i, line) in lines.iter().enumerate() {
                let size = f.fit_size(line, 90.0 / n.sqrt(), 340.0);
                let y = c + (i as f32 - (n - 1.0) / 2.0) * size * 1.25;
                f.draw_centered(&mut img, line, size, c, y, accent);
            }
        }
    }
    img
}

fn pump_line(t: &Telemetry) -> String {
    t.pump_rpm().map(|r| format!("Pump {r} RPM")).unwrap_or_default()
}
