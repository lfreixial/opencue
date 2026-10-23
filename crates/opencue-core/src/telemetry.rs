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
}

impl Telemetry {
    pub fn pump_rpm(&self) -> Option<u16> {
        self.channels.get("pump").and_then(|c| c.rpm)
    }
}
