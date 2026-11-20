//! Lian Li Strimer Plus V2 controller (ENE `0cf2:a200`, "LianLi-Strimer Plus-1.5").
//!
//! The controller drives twelve parallel strips: six of 20 LEDs on the 24-pin ATX cable and six
//! of 27 on the 8-pin GPU cable (a triple 8-pin uses all six; a dual 8-pin only lights four).
//! The protocol is write-only and follows OpenRGB's Strimer L Connect driver: a colour packet per
//! strip, a mode packet that selects direct control, then one packet that applies it all.

use crate::hidraw::{HidDevice, HidInfo, Result};

pub const VID: u16 = 0x0cf2;
pub const PIDS: &[u16] = &[0xa200];
pub const INTERFACE: i32 = 1;
pub const STRIPS_PER_CABLE: usize = 6;

const REPORT_ID: u8 = 0xe0;
/// Report id plus the 254-byte output report.
const PACKET: usize = 255;
const CMD_MODE: u8 = 0x10;
const CMD_COLOUR: u8 = 0x30;
const MODE_DIRECT: u8 = 0x01;
const MODE_RAINBOW: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cable {
    /// 24-pin motherboard cable.
    Atx,
    /// 8-pin GPU cable.
    Gpu,
}

impl Cable {
    pub const ALL: [Cable; 2] = [Cable::Atx, Cable::Gpu];

    /// LEDs along one strip; every strip of a cable shows the same colours.
    pub const fn leds(self) -> usize {
        match self {
            Cable::Atx => 20,
            Cable::Gpu => 27,
        }
    }

    const fn first_strip(self) -> u8 {
        match self {
            Cable::Atx => 0,
            Cable::Gpu => STRIPS_PER_CABLE as u8,
        }
    }

    fn strips(self) -> impl Iterator<Item = u8> {
        self.first_strip()..self.first_strip() + STRIPS_PER_CABLE as u8
    }
}

pub struct Strimer {
    dev: HidDevice,
}

impl Strimer {
    pub fn open(info: &HidInfo) -> Result<Self> {
        let s = Self { dev: HidDevice::open(info)? };
        s.set_mode(MODE_DIRECT)?;
        Ok(s)
    }

    fn send(&self, command: u8, data: &[u8]) -> Result<()> {
        let mut p = [0u8; PACKET];
        p[0] = REPORT_ID;
        p[1] = command;
        p[2..2 + data.len()].copy_from_slice(data);
        self.dev.write(&p).map(|_| ())
    }

    fn apply(&self) -> Result<()> {
        self.send(0x2c, &[0x0f, 0xff])
    }

    /// Mode for every strip, at normal speed and full brightness (effects set their own).
    fn set_mode(&self, mode: u8) -> Result<()> {
        for strip in Cable::ALL.into_iter().flat_map(Cable::strips) {
            self.send(CMD_MODE | strip, &[mode, 0x00, 0x01, 0x00])?;
        }
        self.apply()
    }

    /// Show one frame per cable. Each frame is copied onto all six of the cable's strips.
    pub fn write_colors(&self, atx: &[[u8; 3]], gpu: &[[u8; 3]]) -> Result<()> {
        for (cable, colors) in [(Cable::Atx, atx), (Cable::Gpu, gpu)] {
            let data = colour_data(cable, colors);
            for strip in cable.strips() {
                self.send(CMD_COLOUR | strip, &data)?;
            }
        }
        self.apply()
    }

    /// Back to the controller's own rainbow, which it runs without software.
    pub fn restore_hardware_mode(&self) {
        let _ = self.set_mode(MODE_RAINBOW);
    }
}

/// One strip's colours in the controller's R, B, G byte order; missing LEDs are off.
fn colour_data(cable: Cable, colors: &[[u8; 3]]) -> Vec<u8> {
    (0..cable.leds()).flat_map(|i| colors.get(i).map_or([0; 3], |&[r, g, b]| [r, b, g])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_sent_red_blue_green_and_padded() {
        let data = colour_data(Cable::Atx, &[[1, 2, 3]]);
        assert_eq!(data.len(), 60);
        assert_eq!(&data[..6], &[1, 3, 2, 0, 0, 0]);
    }

    #[test]
    fn longest_strip_fits_one_report() {
        assert!(2 + colour_data(Cable::Gpu, &[]).len() <= PACKET);
    }

    #[test]
    fn cables_use_separate_strips() {
        assert_eq!(Cable::Atx.strips().collect::<Vec<_>>(), (0..6).collect::<Vec<_>>());
        assert_eq!(Cable::Gpu.strips().collect::<Vec<_>>(), (6..12).collect::<Vec<_>>());
    }
}
