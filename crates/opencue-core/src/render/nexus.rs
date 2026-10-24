//! Screens for the 640x48 iCUE NEXUS strip.

use image::RgbaImage;

use super::{blend, fill_rect, fmt_temp, fonts, local_time, solid};
use crate::config::{NexusConfig, NexusMode, parse_color};
use crate::devices::nexus::{HEIGHT, WIDTH};
use crate::telemetry::Telemetry;

const GAP: i32 = 4;

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
    }
    img
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
}
