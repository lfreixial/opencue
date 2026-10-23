//! Corsair iCUE NEXUS: a 640x48 touch strip. Frames are raw RGBA; touches report an X position.

use std::time::Duration;

use super::lcd_cap::frame_packets;
use crate::hidraw::{HidDevice, HidInfo, Result};

pub const PIDS: &[u16] = &[0x1b8e];
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Touch {
    pub pressed: bool,
    pub x: u16,
}

pub struct Nexus {
    dev: HidDevice,
    pub firmware: String,
}

/// Separate handle for the touch reader thread, so frame writes never wait on it.
pub struct NexusTouch {
    dev: HidDevice,
}

impl Nexus {
    pub fn open(info: &HidInfo) -> Result<(Self, NexusTouch)> {
        let dev = HidDevice::open(info)?;
        let firmware = dev
            .get_feature_report(0x05, 33)
            .ok()
            .filter(|fw| fw.len() >= 14)
            .map(|fw| {
                format!(
                    "{}.{}.{}.{}",
                    fw[6] as char,
                    fw[8] as char,
                    fw[10] as char,
                    String::from_utf8_lossy(&fw[12..14])
                )
            })
            .unwrap_or_else(|| "?".into());
        Ok((Self { dev, firmware }, NexusTouch { dev: HidDevice::open(info)? }))
    }

    pub fn restore_hardware_mode(&self) {
        for r in [[0x03, 0x0d, 0x01, 0x01], [0x03, 0x01, 0x64, 0x01]] {
            let _ = self.dev.send_feature_report(&r);
        }
    }

    pub fn set_brightness(&self, percent: u8) -> Result<()> {
        self.dev.send_feature_report(&[0x03, 0x01, percent.min(100), 0x01])
    }

    pub fn write_rgba(&self, rgba: &[u8]) -> Result<()> {
        assert_eq!(rgba.len(), (WIDTH * HEIGHT * 4) as usize, "Nexus frame must be 640x48 RGBA");
        for p in frame_packets(rgba, 0x40) {
            self.dev.write(&p)?;
        }
        Ok(())
    }
}

impl NexusTouch {
    pub fn read(&self, timeout: Duration) -> Result<Option<Touch>> {
        let d = self.dev.read(64, timeout)?;
        if d.len() < 8 {
            return Ok(None);
        }
        Ok(Some(Touch { pressed: d[5] == 1, x: u16::from_le_bytes([d[6], d[7]]) }))
    }
}
