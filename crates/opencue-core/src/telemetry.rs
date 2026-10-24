//! Live state shared by the daemon with the GUI and CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::devices::Kind;
use crate::sensors::SystemSensors;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelState {
    pub name: String,
    pub rpm: Option<u16>,
    pub duty: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    pub key: String,
    pub kind: Kind,
    pub name: String,
    pub firmware: String,
    pub serial: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LightingZone {
    /// "cc:pump", "cc:fan1", "mm700"
    pub id: String,
    pub name: String,
    pub leds: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Telemetry {
    pub devices: Vec<DeviceState>,
    pub system: SystemSensors,
    pub liquid_temp: Option<f32>,
    /// "pump", "fan1".."fan6" -> state, only connected channels.
    pub channels: BTreeMap<String, ChannelState>,
    pub zones: Vec<LightingZone>,
    pub nexus_page: usize,
    /// Set when the daemon has forced everything to 100% because of a critical temperature.
    pub emergency: bool,
    /// Bumped whenever the daemon's config changes (GUI uses it to refetch).
    #[serde(default)]
    pub config_rev: u64,
    /// What the chosen media player is playing (only tracked while the Nexus shows it).
    #[serde(default)]
    pub now_playing: Option<NowPlaying>,
}

/// A snapshot of an MPRIS media player.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NowPlaying {
    /// Player name, e.g. "Spotify".
    pub player: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Local file holding the cover art, once it has been fetched.
    pub art: Option<String>,
    pub playing: bool,
    pub length_us: Option<u64>,
    /// Position when sampled; see [`NowPlaying::position_us`].
    pub position_us: u64,
    pub rate: f64,
    /// Unix time in milliseconds when `position_us` was read.
    pub sampled_ms: u64,
    /// How many players are available (tap the track info to switch).
    pub players: usize,
}

impl NowPlaying {
    /// The playback position now, extrapolated from the last sample.
    pub fn position_now_us(&self) -> u64 {
        let mut pos = self.position_us as f64;
        if self.playing {
            let elapsed_ms = unix_ms().saturating_sub(self.sampled_ms) as f64;
            pos += elapsed_ms * 1000.0 * self.rate;
        }
        let pos = pos.max(0.0) as u64;
        self.length_us.map_or(pos, |l| pos.min(l))
    }
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Telemetry {
    pub fn pump_rpm(&self) -> Option<u16> {
        self.channels.get("pump").and_then(|c| c.rpm)
    }
}
