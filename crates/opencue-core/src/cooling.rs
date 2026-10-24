//! Fan and pump curves.

use crate::config::{ChannelCooling, FanMode, Preset};

pub fn preset_points(preset: Preset, pump: bool) -> &'static [(f32, f32)] {
    match (pump, preset) {
        (false, Preset::Quiet) => &[(20., 20.), (30., 25.), (35., 35.), (40., 55.), (45., 80.), (50., 100.)],
        (false, Preset::Balanced) => &[(20., 25.), (30., 35.), (35., 50.), (40., 70.), (45., 90.), (50., 100.)],
        (false, Preset::Performance) => &[(20., 40.), (30., 55.), (35., 70.), (40., 85.), (45., 100.)],
        (true, Preset::Quiet) => &[(20., 50.), (35., 55.), (40., 70.), (45., 85.), (50., 100.)],
        (true, Preset::Balanced) => &[(20., 70.), (35., 75.), (40., 85.), (45., 100.)],
        (_, _) => &[(0., 100.)],
    }
}

/// The effective curve for a channel, whatever its mode.
pub fn curve(cfg: &ChannelCooling, pump: bool) -> Vec<(f32, f32)> {
    match cfg.mode {
        FanMode::Fixed => vec![(0.0, cfg.fixed as f32)],
        FanMode::Custom if !cfg.points.is_empty() => cfg.points.clone(),
        _ => preset_points(cfg.preset, pump).to_vec(),
    }
}

/// Linear interpolation; flat beyond the first and last points.
pub fn interpolate(points: &[(f32, f32)], temp: f32) -> f32 {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let Some(first) = pts.first() else { return 100.0 };
    if temp <= first.0 {
        return first.1;
    }
    for w in pts.windows(2) {
        let ((t0, d0), (t1, d1)) = (w[0], w[1]);
        if temp <= t1 {
            return if t1 == t0 { d1 } else { d0 + (d1 - d0) * (temp - t0) / (t1 - t0) };
        }
    }
    pts.last().unwrap().1
}

/// Turns a sensor reading into a duty, smoothed so fans don't hunt:
/// ramps up fast (temperature spikes matter), down slowly (no audible pulsing).
#[derive(Debug, Clone)]
pub struct DutyController {
    temp: Option<f32>,
    duty: Option<f32>,
}

impl Default for DutyController {
    fn default() -> Self {
        Self::new()
    }
}

impl DutyController {
    const UP_STEP: f32 = 10.0;
    const DOWN_STEP: f32 = 2.0;
    const TEMP_ALPHA: f32 = 0.35;

    pub fn new() -> Self {
        Self { temp: None, duty: None }
    }

    /// `temp` None means the sensor is unavailable: fail safe to 100%.
    pub fn update(&mut self, points: &[(f32, f32)], temp: Option<f32>, min_duty: f32, immediate: bool) -> u8 {
        let target = match temp {
            None => 100.0,
            Some(t) => {
                let smoothed = self.temp.map_or(t, |prev| prev + Self::TEMP_ALPHA * (t - prev));
                self.temp = Some(smoothed);
                interpolate(points, smoothed)
            }
        }
        .clamp(min_duty, 100.0);
        let duty = match self.duty {
            Some(d) if !immediate && target > d => (d + Self::UP_STEP).min(target),
            Some(d) if !immediate => (d - Self::DOWN_STEP).max(target),
            _ => target,
        };
        self.duty = Some(duty);
        duty.round() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_clamps() {
        let pts = [(30.0, 20.0), (40.0, 60.0)];
        assert_eq!(interpolate(&pts, 20.0), 20.0);
        assert_eq!(interpolate(&pts, 35.0), 40.0);
        assert_eq!(interpolate(&pts, 90.0), 60.0);
        assert_eq!(interpolate(&[], 50.0), 100.0);
    }

    #[test]
    fn missing_sensor_is_full_speed() {
        let mut c = DutyController::new();
        assert_eq!(c.update(&[(0.0, 30.0)], None, 20.0, false), 100);
    }

    #[test]
    fn ramps_down_slowly() {
        let mut c = DutyController::new();
        let pts = [(30.0, 30.0), (50.0, 100.0)];
        assert_eq!(c.update(&pts, Some(50.0), 20.0, false), 100);
        assert_eq!(c.update(&pts, Some(30.0), 20.0, false), 98);
    }
}
