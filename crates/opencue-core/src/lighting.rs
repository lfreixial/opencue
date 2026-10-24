//! RGB effects. Every effect is a pure function of time, so the daemon and the GUI preview
//! render exactly the same thing.

use crate::config::{Effect, Lighting, parse_color};

pub fn hsv(h: f32, s: f32, v: f32) -> [u8; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    let (r, g, b) = match i as i32 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

fn lerp(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8)
}

fn scale(c: [u8; 3], k: f32) -> [u8; 3] {
    c.map(|v| (v as f32 * k.clamp(0.0, 1.0)).round() as u8)
}

/// Sample a multi-stop gradient at 0..1, wrapping around when `wrap` is set.
fn gradient(colors: &[[u8; 3]], pos: f32, wrap: bool) -> [u8; 3] {
    match colors.len() {
        0 => [0, 0, 0],
        1 => colors[0],
        n => {
            let segments = if wrap { n } else { n - 1 } as f32;
            let p = if wrap { pos.rem_euclid(1.0) } else { pos.clamp(0.0, 1.0) } * segments;
            let i = (p.floor() as usize).min(segments as usize - 1);
            lerp(colors[i], colors[(i + 1) % n], p - i as f32)
        }
    }
}

fn hash(a: u32, b: u32) -> f32 {
    let mut x = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

/// Render one frame for `n` LEDs.
/// * `t` - seconds since start
/// * `phase` - per-zone offset (0..1) so fans in a row don't move in lockstep
/// * `temp` - current reading of the effect's sensor, for the Temperature effect
pub fn render(cfg: &Lighting, t: f64, n: usize, phase: f32, temp: Option<f32>) -> Vec<[u8; 3]> {
    let colors: Vec<[u8; 3]> =
        if cfg.colors.is_empty() { vec![[255, 255, 255]] } else { cfg.colors.iter().map(|c| parse_color(c)).collect() };
    let c0 = colors[0];
    let speed = cfg.speed.clamp(1, 100) as f32 / 50.0; // 1.0 at the default speed
    let tt = (t as f32) * speed * if cfg.reverse { -1.0 } else { 1.0 };
    let pos = |i: usize| if n == 0 { 0.0 } else { i as f32 / n as f32 };

    let mut out: Vec<[u8; 3]> = match cfg.effect {
        Effect::Off => vec![[0, 0, 0]; n],
        Effect::Static => vec![c0; n],
        Effect::Gradient => (0..n).map(|i| gradient(&colors, pos(i), false)).collect(),
        Effect::Breathing => {
            let cycle = tt * 0.25;
            let idx = cycle.floor().rem_euclid(colors.len() as f32) as usize;
            let k = 0.5 - 0.5 * (cycle.fract() * std::f32::consts::TAU).cos();
            vec![scale(colors[idx], k); n]
        }
        Effect::ColorShift => vec![gradient(&colors, tt * 0.1 + phase, true); n],
        Effect::Rainbow => vec![hsv(tt * 0.1 + phase, 1.0, 1.0); n],
        Effect::RainbowWave => (0..n).map(|i| hsv(pos(i) + tt * 0.25 + phase, 1.0, 1.0)).collect(),
        Effect::ColorWave => (0..n).map(|i| gradient(&colors, pos(i) - tt * 0.25 + phase, true)).collect(),
        Effect::Spinner => {
            let head = (tt * 0.6 + phase).rem_euclid(1.0) * n as f32;
            (0..n)
                .map(|i| {
                    let d = (head - i as f32).rem_euclid(n.max(1) as f32);
                    let k = (1.0 - d / (n as f32 * 0.35)).max(0.0);
                    lerp(colors.get(1).copied().unwrap_or([0, 0, 0]), c0, k * k)
                })
                .collect()
        }
        Effect::Marquee => {
            let shift = (tt * 4.0).floor() as i64;
            (0..n)
                .map(|i| {
                    if (i as i64 + shift).rem_euclid(3) == 0 { c0 } else { colors.get(1).copied().unwrap_or([0, 0, 0]) }
                })
                .collect()
        }
        Effect::Twinkle => {
            let frame = (tt * 3.0) as i64;
            let bg = colors.get(1).copied().unwrap_or([0, 0, 0]);
            (0..n)
                .map(|i| {
                    let seed = (i as u32).wrapping_add((phase * 1000.0) as u32);
                    let r = hash(seed, frame as u32);
                    if r > 0.85 { lerp(bg, c0, (r - 0.85) / 0.15) } else { bg }
                })
                .collect()
        }
        Effect::Temperature => {
            // colors: [cool, hot]; maps 30°C..50°C (liquid) — sensible for AIO loops.
            let cool = c0;
            let hot = colors.get(1).copied().unwrap_or([255, 0, 0]);
            let (lo, hi) = match cfg.sensor {
                crate::config::Sensor::Liquid => (28.0, 45.0),
                _ => (40.0, 85.0),
            };
            let k = temp.map_or(1.0, |v| (v - lo) / (hi - lo));
            vec![lerp(cool, hot, k); n]
        }
    };
    let b = cfg.brightness.min(100) as f32 / 100.0;
    if b < 1.0 {
        out.iter_mut().for_each(|c| *c = scale(*c, b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_and_brightness() {
        let cfg =
            Lighting { effect: Effect::Static, colors: vec!["#ff8000".into()], brightness: 50, ..Default::default() };
        assert_eq!(render(&cfg, 0.0, 2, 0.0, None), vec![[128, 64, 0], [128, 64, 0]]);
    }

    #[test]
    fn every_effect_renders_n_leds() {
        for e in Effect::ALL {
            let cfg = Lighting { effect: e, ..Default::default() };
            assert_eq!(render(&cfg, 1.234, 24, 0.3, Some(35.0)).len(), 24);
        }
    }
}
