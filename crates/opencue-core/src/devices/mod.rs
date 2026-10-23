//! Device discovery: map connected Corsair hidraw nodes to drivers.

pub mod commander_core;
pub mod lcd_cap;
pub mod mm700;
pub mod nexus;

use crate::hidraw::{HidInfo, enumerate};

pub const CORSAIR_VID: u16 = 0x1b1c;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    CommanderCore,
    LcdCap,
    Nexus,
    Mm700,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: Kind,
    pub info: HidInfo,
}

pub fn discover() -> Vec<Found> {
    enumerate(Some(CORSAIR_VID))
        .into_iter()
        .filter_map(|info| {
            let kind = match info.pid {
                p if commander_core::PIDS.contains(&p) && info.interface == 0 => Kind::CommanderCore,
                p if lcd_cap::PIDS.contains(&p) && info.interface == 0 => Kind::LcdCap,
                p if nexus::PIDS.contains(&p) && info.interface == 0 => Kind::Nexus,
                p if mm700::PIDS.contains(&p) && info.interface == mm700::INTERFACE => Kind::Mm700,
                _ => return None,
            };
            Some(Found { kind, info })
        })
        .collect()
}
