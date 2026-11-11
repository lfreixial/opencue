//! Screen sampling and the per-profile device layout. Coordinates are normalized to the
//! selected monitor: (0, 0) is its top left, (1, 1) its bottom right.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// Ambient colours need a grid, not a video-resolution image. A frame fits inside an 8 KiB
// pipe buffer, and even at 60 Hz moves less pixel data than the old 160x90 / 20 Hz capture.
pub const WIDTH: usize = 64;
pub const HEIGHT: usize = 36;
pub const FPS: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

impl Default for Position {
    fn default() -> Self {
        Self { x: 0.5, y: 0.5 }
    }
}

fn bounded(v: f32, lo: f32, hi: f32, fallback: f32) -> f32 {
    if v.is_finite() { v.clamp(lo, hi) } else { fallback }
}

impl Position {
    pub fn clamped(self) -> Self {
        Self { x: bounded(self.x, 0.0, 1.0, 0.5), y: bounded(self.y, 0.0, 1.0, 0.5) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub positions: BTreeMap<String, Position>,
    /// Width and height of each sampling region, as a fraction of the monitor.
    pub sample_size: f32,
}

impl Default for Layout {
    fn default() -> Self {
        Self { positions: BTreeMap::new(), sample_size: 0.2 }
    }
}

impl Layout {
    pub fn normalize(&mut self) {
        self.sample_size = bounded(self.sample_size, 0.05, 1.0, 0.2);
        self.positions.values_mut().for_each(|p| *p = p.clamped());
    }

    pub fn position(&self, zone: &str) -> Position {
        self.positions
            .get(zone)
            .copied()
            .unwrap_or_else(|| match zone {
                "mm700" => Position { x: 0.5, y: 0.9 },
                "cc:pump" => Position { x: 0.5, y: 0.5 },
                _ => {
                    if let Some(n @ 1..=6) = zone.strip_prefix("cc:fan").and_then(|s| s.parse::<usize>().ok()) {
                        Position { x: if n % 2 == 1 { 0.1 } else { 0.9 }, y: 0.2 + ((n - 1) / 2) as f32 * 0.3 }
                    } else {
                        Position::default()
                    }
                }
            })
            .clamped()
    }
}

/// A small RGB frame kept only in memory; no screen recordings are saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenFrame {
    #[serde(with = "pixel_data")]
    pub pixels: Vec<[u8; 3]>,
}

mod pixel_data {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(pixels: &[[u8; 3]], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(pixels.as_flattened()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<[u8; 3]>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let bytes = STANDARD.decode(encoded).map_err(D::Error::custom)?;
        if bytes.len() != super::WIDTH * super::HEIGHT * 3 {
            return Err(D::Error::custom("invalid Ambilight frame size"));
        }
        Ok(bytes.as_chunks::<3>().0.to_vec())
    }
}

impl ScreenFrame {
    pub fn sample(&self, layout: &Layout, zone: &str, brightness: u8) -> [u8; 3] {
        if self.pixels.len() != WIDTH * HEIGHT {
            return [0; 3];
        }
        let p = layout.position(zone);
        let half = bounded(layout.sample_size, 0.05, 1.0, 0.2) / 2.0;
        let x0 = ((p.x - half).max(0.0) * WIDTH as f32).floor() as usize;
        let x1 = (((p.x + half).min(1.0) * WIDTH as f32).ceil() as usize).clamp(x0 + 1, WIDTH);
        let y0 = ((p.y - half).max(0.0) * HEIGHT as f32).floor() as usize;
        let y1 = (((p.y + half).min(1.0) * HEIGHT as f32).ceil() as usize).clamp(y0 + 1, HEIGHT);
        let mut sum = [0u64; 3];
        for y in y0..y1 {
            for x in x0..x1 {
                for (s, c) in sum.iter_mut().zip(self.pixels[y * WIDTH + x]) {
                    *s += c as u64;
                }
            }
        }
        let count = ((x1 - x0) * (y1 - y0)) as f64;
        sum.map(|v| (v as f64 / count * brightness.min(100) as f64 / 100.0).round() as u8)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureState {
    pub sequence: u64,
    pub active: bool,
    pub status: String,
    pub frame: Option<ScreenFrame>,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self { sequence: 0, active: false, status: "Choose a monitor to start Ambilight.".into(), frame: None }
    }
}

pub fn render(frame: Option<&ScreenFrame>, layout: &Layout, zone: &str, brightness: u8, leds: usize) -> Vec<[u8; 3]> {
    vec![frame.map_or([0; 3], |f| f.sample(layout, zone, brightness)); leds]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_frames_round_trip_without_json_pixel_arrays() {
        let frame =
            ScreenFrame { pixels: (0..WIDTH * HEIGHT).map(|i| [i as u8, (i / 7) as u8, (i / 23) as u8]).collect() };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.len() < WIDTH * HEIGHT * 4 + 32);
        assert_eq!(serde_json::from_str::<ScreenFrame>(&json).unwrap(), frame);
        assert!(serde_json::from_str::<ScreenFrame>(r#"{"pixels":"AA=="}"#).is_err());
    }

    #[test]
    fn devices_follow_their_side_of_the_monitor() {
        let frame = ScreenFrame {
            pixels: (0..WIDTH * HEIGHT)
                .map(|i| if i % WIDTH < WIDTH / 2 { [255, 0, 0] } else { [0, 0, 255] })
                .collect(),
        };
        let mut layout = Layout::default();
        assert_eq!(frame.sample(&layout, "cc:fan1", 100), [255, 0, 0]);
        assert_eq!(frame.sample(&layout, "cc:fan2", 50), [0, 0, 128]);
        layout.positions.insert("cc:fan1".into(), Position { x: 0.9, y: 0.5 });
        assert_eq!(frame.sample(&layout, "cc:fan1", 100), [0, 0, 255]);
    }

    #[test]
    fn edges_average_only_visible_pixels_and_missing_capture_is_dark() {
        let frame = ScreenFrame { pixels: vec![[40, 80, 120]; WIDTH * HEIGHT] };
        for x in [0.0, 1.0] {
            for y in [0.0, 1.0] {
                let layout =
                    Layout { positions: BTreeMap::from([("test".into(), Position { x, y })]), sample_size: 0.05 };
                assert_eq!(frame.sample(&layout, "test", 100), [40, 80, 120]);
            }
        }
        assert_eq!(render(None, &Layout::default(), "test", 100, 8), vec![[0; 3]; 8]);
        assert_eq!(ScreenFrame { pixels: vec![] }.sample(&Layout::default(), "test", 100), [0; 3]);
    }

    #[test]
    fn layout_survives_reload_and_old_profiles_get_defaults() {
        let mut cfg: crate::config::Config =
            serde_json::from_str(r#"{"profiles":{"Default":{"lighting":{"*":{"effect":"ambilight"}}}}}"#).unwrap();
        cfg.profile_mut().ambilight.positions.insert("mm700".into(), Position { x: 0.8, y: 0.6 });
        let reloaded: crate::config::Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(reloaded, cfg);
        assert_eq!(reloaded.profile().ambilight.position("mm700"), Position { x: 0.8, y: 0.6 });
    }

    #[test]
    fn malformed_positions_and_sizes_are_bounded() {
        let mut layout = Layout {
            sample_size: f32::NAN,
            positions: BTreeMap::from([("test".into(), Position { x: -5.0, y: 9.0 })]),
        };
        layout.normalize();
        assert_eq!(layout.sample_size, 0.2);
        assert_eq!(layout.position("test"), Position { x: 0.0, y: 1.0 });
    }

    #[test]
    fn vertical_positions_and_sample_area_control_the_average() {
        let frame = ScreenFrame {
            pixels: (0..WIDTH * HEIGHT)
                .map(|i| if i / WIDTH < HEIGHT / 2 { [240, 80, 0] } else { [0, 80, 240] })
                .collect(),
        };
        let mut layout = Layout::default();
        layout.positions.insert("test".into(), Position { x: 0.5, y: 0.1 });
        assert_eq!(frame.sample(&layout, "test", 100), [240, 80, 0]);
        layout.positions.insert("test".into(), Position { x: 0.5, y: 0.9 });
        assert_eq!(frame.sample(&layout, "test", 100), [0, 80, 240]);
        layout.positions.insert("test".into(), Position::default());
        layout.sample_size = 1.0;
        assert_eq!(frame.sample(&layout, "test", 100), [120, 80, 120]);
    }
}
