//! Corsair MM700 RGB mousepad: three lighting zones.

use std::time::Duration;

use crate::hidraw::{HidDevice, HidError, HidInfo, Result};

pub const PIDS: &[u16] = &[0x1b9b, 0x1bc9];
pub const INTERFACE: i32 = 1;
pub const ZONES: usize = 3;
const REPORT: usize = 64;

pub struct Mm700 {
    dev: HidDevice,
    pub firmware: String,
}

impl Mm700 {
    pub fn open(info: &HidInfo) -> Result<Self> {
        let mut m = Self { dev: HidDevice::open(info)?, firmware: "?".into() };
        m.transfer(&[0x01, 0x03, 0x00, 0x02], &[])?; // software mode
        let fw = m.transfer(&[0x02, 0x13], &[])?;
        m.firmware = format!("{}.{}.{}", fw[3], fw[4], fw[5]);
        m.transfer(&[0x0d, 0x00, 0x01], &[])?; // activate LEDs
        Ok(m)
    }

    fn transfer(&self, command: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        let mut p = vec![0u8; REPORT + 1];
        p[1] = 0x08;
        p[2..2 + command.len()].copy_from_slice(command);
        p[2 + command.len()..2 + command.len() + payload.len()].copy_from_slice(payload);
        self.dev.drain(REPORT)?;
        self.dev.write(&p)?;
        let res = self.dev.read(REPORT, Duration::from_secs(1))?;
        if res.len() < 6 {
            return Err(HidError::Protocol(format!("MM700: no reply to {command:02x?}")));
        }
        Ok(res)
    }

    pub fn restore_hardware_mode(&self) {
        let _ = self.transfer(&[0x01, 0x03, 0x00, 0x01], &[]);
    }

    pub fn keepalive(&self) -> Result<()> {
        self.transfer(&[0x12], &[]).map(|_| ())
    }

    pub fn write_colors(&self, colors: &[[u8; 3]]) -> Result<()> {
        // Planar layout: R of every zone, then G, then B.
        let mut payload = vec![(ZONES * 3) as u8, 0, 0, 0];
        payload.resize(4 + ZONES * 3, 0);
        for z in 0..ZONES {
            let [r, g, b] = colors.get(z).copied().unwrap_or([0, 0, 0]);
            payload[4 + z] = r;
            payload[4 + ZONES + z] = g;
            payload[4 + 2 * ZONES + z] = b;
        }
        self.transfer(&[0x06, 0x00], &payload).map(|_| ())
    }
}
