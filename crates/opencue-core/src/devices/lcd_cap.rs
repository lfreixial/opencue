//! Corsair LCD cap for Elite LCD pumps: a round 480x480 screen fed with JPEG frames.

use crate::hidraw::{HidDevice, HidInfo, Result};

pub const PIDS: &[u16] = &[0x0c39, 0x0c33];
pub const SIZE: u32 = 480;
const PACKET: usize = 1024;
const HEADER: usize = 8;

pub struct LcdCap {
    dev: HidDevice,
}

/// Split a frame into 1024-byte output reports: 02 05 <flag> <last> <index> 00 <len u16le> <data>.
pub(crate) fn frame_packets(data: &[u8], flag: u8) -> Vec<Vec<u8>> {
    let chunk = PACKET - HEADER;
    let total = data.len().div_ceil(chunk).max(1);
    (0..total)
        .map(|i| {
            let part = &data[i * chunk..((i + 1) * chunk).min(data.len())];
            let mut p = vec![0u8; PACKET];
            p[0] = 0x02;
            p[1] = 0x05;
            p[2] = flag;
            p[3] = (i + 1 == total) as u8;
            p[4] = i as u8;
            p[6..8].copy_from_slice(&(part.len() as u16).to_le_bytes());
            p[HEADER..HEADER + part.len()].copy_from_slice(part);
            p
        })
        .collect()
}

impl LcdCap {
    pub fn open(info: &HidInfo) -> Result<Self> {
        Ok(Self { dev: HidDevice::open(info)? })
    }

    pub fn restore_hardware_mode(&self) {
        for r in [[0x03, 0x1e, 0x01, 0x01], [0x03, 0x1d, 0x00, 0x01], [0x03, 0x0b, 0x40, 0x01]] {
            let _ = self.dev.send_feature_report(&r);
        }
    }

    /// 0..=100 percent; the panel takes 0..=0x40.
    pub fn set_brightness(&self, percent: u8) -> Result<()> {
        let v = (percent.min(100) as u32 * 64 / 100) as u8;
        self.dev.send_feature_report(&[0x03, 0x0b, v, 0x01])
    }

    pub fn set_rotation(&self, degrees: u16) -> Result<()> {
        let r = match degrees {
            90 => 1,
            180 => 2,
            270 => 3,
            _ => 0,
        };
        self.dev.send_feature_report(&[0x03, 0x0c, r, 0x01])
    }

    pub fn write_jpeg(&self, jpeg: &[u8]) -> Result<()> {
        for p in frame_packets(jpeg, 0x00) {
            self.dev.write(&p)?;
        }
        Ok(())
    }
}
