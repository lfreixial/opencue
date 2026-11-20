//! Device discovery: map connected hidraw nodes (Corsair, and the Lian Li Strimer) to drivers.

pub mod commander_core;
pub mod lcd_cap;
pub mod mm700;
pub mod nexus;
pub mod strimer;

use crate::hidraw::{HidInfo, enumerate};

pub const CORSAIR_VID: u16 = 0x1b1c;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    CommanderCore,
    LcdCap,
    Nexus,
    Mm700,
    Strimer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: Kind,
    pub info: HidInfo,
}

/// The driver for one hidraw node, if OpenCUE has one.
pub fn kind_of(info: &HidInfo) -> Option<Kind> {
    let (vid, p, iface) = (info.vid, info.pid, info.interface);
    Some(match vid {
        CORSAIR_VID if commander_core::PIDS.contains(&p) && iface == 0 => Kind::CommanderCore,
        CORSAIR_VID if lcd_cap::PIDS.contains(&p) && iface == 0 => Kind::LcdCap,
        CORSAIR_VID if nexus::PIDS.contains(&p) && iface == 0 => Kind::Nexus,
        CORSAIR_VID if mm700::PIDS.contains(&p) && iface == mm700::INTERFACE => Kind::Mm700,
        strimer::VID if strimer::PIDS.contains(&p) && iface == strimer::INTERFACE => Kind::Strimer,
        _ => return None,
    })
}

/// Whether a hidraw node belongs to a vendor OpenCUE talks to (for `opencue devices`).
pub fn is_known_vendor(info: &HidInfo) -> bool {
    info.vid == CORSAIR_VID || (info.vid == strimer::VID && strimer::PIDS.contains(&info.pid))
}

pub fn discover() -> Vec<Found> {
    enumerate(None).into_iter().filter_map(|info| Some(Found { kind: kind_of(&info)?, info })).collect()
}
