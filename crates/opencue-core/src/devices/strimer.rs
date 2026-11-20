//! Lian Li Strimer Plus V2 controller (ENE `0cf2:a200`, "LianLi-Strimer Plus-1.5").
//!
//! The controller drives twelve parallel strips: six of 20 LEDs on the 24-pin ATX cable and six
//! of 27 on the 8-pin GPU cable (a triple 8-pin uses all six; a dual 8-pin only lights four).
//! The protocol is write-only and follows OpenRGB's Strimer L Connect driver: a colour packet per
//! strip, a mode packet that selects direct control, then one packet that applies it all.
//!
//! The controller takes about 3 ms per packet whatever its size, so drawing every frame tops out
//! near 26 fps (13 packets), which looks steppy on moving effects. Those that the controller has
//! built in therefore run on the controller itself (see [`Builtin`]).

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::config::{Effect, Lighting};
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
const MODE_RAINBOW_MORPH: u8 = 0x04;
const MODE_RAINBOW: u8 = 0x05;
/// Mode packet speed byte, slowest to fastest.
const SPEEDS: [u8; 5] = [0x02, 0x01, 0x00, 0xfe, 0xff];
/// Mode packet brightness byte for 0%, 25%, 50%, 75% and 100%.
const BRIGHTNESS: [u8; 5] = [0x08, 0x03, 0x02, 0x01, 0x00];

/// An effect the controller runs by itself, smoothly, instead of being sent every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    mode: u8,
    speed: u8,
    brightness: u8,
    reverse: bool,
}

impl Builtin {
    /// The built-in version of `cfg`'s effect, at the nearest speed and brightness, if it has one.
    pub fn for_lighting(cfg: &Lighting) -> Option<Self> {
        let mode = match cfg.effect {
            Effect::RainbowWave => MODE_RAINBOW,
            Effect::Rainbow => MODE_RAINBOW_MORPH,
            _ => return None,
        };
        let level = |v: f32| (v.clamp(0.0, 1.0) * 4.0).round() as usize;
        Some(Self {
            mode,
            speed: SPEEDS[level((cfg.speed.clamp(1, 100) - 1) as f32 / 99.0)],
            brightness: BRIGHTNESS[level(cfg.brightness.min(100) as f32 / 100.0)],
            reverse: cfg.reverse,
        })
    }

    fn data(self) -> [u8; 4] {
        [self.mode, self.speed, if self.reverse { 0x00 } else { 0x01 }, self.brightness]
    }
}

const DIRECT: Builtin = Builtin { mode: MODE_DIRECT, speed: SPEEDS[2], brightness: BRIGHTNESS[4], reverse: false };

/// What one cable shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Show {
    /// Colours along one strip, copied onto all six of the cable's strips.
    Frame(Vec<[u8; 3]>),
    Builtin(Builtin),
}

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

    const fn index(self) -> usize {
        match self {
            Cable::Atx => 0,
            Cable::Gpu => 1,
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
    /// The mode each cable was last set to.
    modes: [Cell<Builtin>; 2],
    /// When a built-in effect was last switched to, until it has been sent a second time.
    confirm: Cell<Option<Instant>>,
}

/// The controller sometimes misses mode packets sent right after it has been reset or switched,
/// leaving a strip stuck on its last colour, so a new built-in effect is sent again after this.
const CONFIRM_AFTER: Duration = Duration::from_secs(1);

impl Strimer {
    pub fn open(info: &HidInfo) -> Result<Self> {
        let s = Self {
            dev: HidDevice::open(info)?,
            modes: [Cell::new(DIRECT), Cell::new(DIRECT)],
            confirm: Cell::new(None),
        };
        for cable in Cable::ALL {
            s.set_mode(cable, DIRECT)?;
        }
        s.apply()?;
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

    fn set_mode(&self, cable: Cable, mode: Builtin) -> Result<()> {
        for strip in cable.strips() {
            self.send(CMD_MODE | strip, &mode.data())?;
        }
        self.modes[cable.index()].set(mode);
        Ok(())
    }

    /// Show `atx` on the 24-pin cable and `gpu` on the GPU cable. A built-in effect that is
    /// already running is left alone, so it carries on instead of starting over (apart from
    /// being sent once more, see [`CONFIRM_AFTER`]).
    pub fn show(&self, atx: &Show, gpu: &Show) -> Result<()> {
        let confirm = self.confirm.get().is_some_and(|t| t.elapsed() >= CONFIRM_AFTER);
        if confirm {
            self.confirm.set(None);
        }
        let mut changed = false;
        for (cable, show) in [(Cable::Atx, atx), (Cable::Gpu, gpu)] {
            let current = self.modes[cable.index()].get();
            match show {
                Show::Frame(colors) => {
                    if current != DIRECT {
                        self.set_mode(cable, DIRECT)?;
                    }
                    let data = colour_data(cable, colors);
                    for strip in cable.strips() {
                        self.send(CMD_COLOUR | strip, &data)?;
                    }
                    changed = true;
                }
                Show::Builtin(mode) if *mode != current => {
                    self.set_mode(cable, *mode)?;
                    self.confirm.set(Some(Instant::now()));
                    changed = true;
                }
                Show::Builtin(mode) if confirm => {
                    self.set_mode(cable, *mode)?;
                    changed = true;
                }
                Show::Builtin(_) => {}
            }
        }
        if changed { self.apply() } else { Ok(()) }
    }

    /// Show one frame per cable. Each frame is copied onto all six of the cable's strips.
    pub fn write_colors(&self, atx: &[[u8; 3]], gpu: &[[u8; 3]]) -> Result<()> {
        self.show(&Show::Frame(atx.to_vec()), &Show::Frame(gpu.to_vec()))
    }

    /// Back to the controller's own rainbow, which it runs without software.
    pub fn restore_hardware_mode(&self) {
        let rainbow = Builtin { mode: MODE_RAINBOW, ..DIRECT };
        let _ = Cable::ALL.into_iter().try_for_each(|cable| self.set_mode(cable, rainbow)).and_then(|()| self.apply());
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
    fn moving_rainbows_run_on_the_controller() {
        let wave = Lighting { effect: Effect::RainbowWave, ..Default::default() };
        let b = Builtin::for_lighting(&wave).unwrap();
        assert_eq!(b.data(), [MODE_RAINBOW, 0x00, 0x01, 0x00]);
        let slow_dim = Lighting { speed: 1, brightness: 30, reverse: true, ..wave };
        assert_eq!(Builtin::for_lighting(&slow_dim).unwrap().data(), [MODE_RAINBOW, 0x02, 0x00, 0x03]);
        let fast = Lighting { effect: Effect::Rainbow, speed: 100, ..Default::default() };
        assert_eq!(Builtin::for_lighting(&fast).unwrap().data()[..2], [MODE_RAINBOW_MORPH, 0xff]);
        for effect in [Effect::Static, Effect::Spinner, Effect::Ambilight, Effect::Temperature] {
            assert_eq!(Builtin::for_lighting(&Lighting { effect, ..Default::default() }), None);
        }
    }

    #[test]
    fn cables_use_separate_strips() {
        assert_eq!(Cable::Atx.strips().collect::<Vec<_>>(), (0..6).collect::<Vec<_>>());
        assert_eq!(Cable::Gpu.strips().collect::<Vec<_>>(), (6..12).collect::<Vec<_>>());
    }
}
