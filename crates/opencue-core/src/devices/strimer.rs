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

use crate::config::{Effect, Lighting, parse_color};
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

/// Colours the controller's own animations take.
pub const PALETTE: usize = 6;
type Palette = [[u8; 3]; PALETTE];

/// What the Strimer's own animations use when "All devices" has no colours of its own.
const RAINBOW: Palette = [[255, 0, 0], [255, 160, 0], [0, 255, 0], [0, 255, 255], [0, 0, 255], [255, 0, 255]];

/// The colours one of the Strimer's own animations uses: the zone's own when it has
/// `own_colors`, otherwise those of `all` ("All devices").
pub fn palette_for(cfg: &Lighting, all: &Lighting) -> Palette {
    palette(if cfg.own_colors { cfg } else { all })
}

/// `src`'s colours, repeated to fill the palette, or a rainbow when its effect has none.
fn palette(src: &Lighting) -> Palette {
    let colors: Vec<[u8; 3]> = match src.effect {
        Effect::Off | Effect::Rainbow | Effect::RainbowWave | Effect::Ambilight => return RAINBOW,
        Effect::Static => src.colors.iter().take(1).map(|c| parse_color(c)).collect(),
        _ => src.colors.iter().take(2).map(|c| parse_color(c)).collect(),
    };
    if colors.is_empty() {
        return RAINBOW;
    }
    std::array::from_fn(|i| colors[i % colors.len()])
}

/// The controller's mode for one of its own animations.
fn strimer_mode(effect: Effect) -> Option<u8> {
    Some(match effect {
        Effect::Snooker => 0x19,
        Effect::Mixing => 0x1a,
        Effect::PingPong => 0x1b,
        Effect::Runway => 0x1c,
        Effect::Painting => 0x1d,
        Effect::Tide => 0x1e,
        Effect::BlowUp => 0x1f,
        Effect::Meteor => 0x20,
        Effect::ShockWave => 0x21,
        Effect::Ripple => 0x22,
        Effect::Drizzling => 0x25,
        Effect::FadeOut => 0x26,
        Effect::ColorTransfer => 0x27,
        Effect::CrossOver => 0x28,
        Effect::Contest => 0x2a,
        Effect::Parallel => 0x2b,
        _ => return None,
    })
}

/// An effect the controller runs by itself, smoothly, instead of being sent every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    mode: u8,
    speed: u8,
    brightness: u8,
    reverse: bool,
    /// For the controller's own animations; sent as the strip's first colours.
    palette: Option<Palette>,
}

impl Builtin {
    /// The built-in version of `cfg`'s effect, at the nearest speed and brightness, if it has one.
    /// The controller's own animations take their colours from [`palette_for`].
    pub fn for_lighting(cfg: &Lighting, all: &Lighting) -> Option<Self> {
        let (mode, palette) = match cfg.effect {
            Effect::RainbowWave => (MODE_RAINBOW, None),
            Effect::Rainbow => (MODE_RAINBOW_MORPH, None),
            e => (strimer_mode(e)?, Some(palette_for(cfg, all))),
        };
        let level = |v: f32| (v.clamp(0.0, 1.0) * 4.0).round() as usize;
        Some(Self {
            mode,
            speed: SPEEDS[level((cfg.speed.clamp(1, 100) - 1) as f32 / 99.0)],
            brightness: BRIGHTNESS[level(cfg.brightness.min(100) as f32 / 100.0)],
            reverse: cfg.reverse,
            palette,
        })
    }

    fn data(self) -> [u8; 4] {
        // Direction 0 runs the wave from the top of the cable down (LED 0 is at the bottom), the
        // way OpenCUE draws it; reverse runs it upward.
        [self.mode, self.speed, u8::from(self.reverse), self.brightness]
    }
}

const DIRECT: Builtin =
    Builtin { mode: MODE_DIRECT, speed: SPEEDS[2], brightness: BRIGHTNESS[4], reverse: false, palette: None };

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

    /// This cable's strips in the apply packet's strip mask.
    const fn mask(self) -> u16 {
        0x3f << self.first_strip()
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
    confirm: Cell<Confirm>,
}

/// The controller can miss mode packets sent soon after it has been reset, leaving a strip stuck
/// on its last colour. So after connecting, the first built-in effects are sent again this long
/// after they started (only then: sending a mode restarts its animation).
const CONFIRM_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Confirm {
    Waiting,
    Due(Instant),
    Done,
}

impl Strimer {
    pub fn open(info: &HidInfo) -> Result<Self> {
        let s = Self {
            dev: HidDevice::open(info)?,
            modes: [Cell::new(DIRECT), Cell::new(DIRECT)],
            confirm: Cell::new(Confirm::Waiting),
        };
        for cable in Cable::ALL {
            s.set_mode(cable, DIRECT)?;
        }
        s.apply(Cable::Atx.mask() | Cable::Gpu.mask())?;
        Ok(s)
    }

    fn send(&self, command: u8, data: &[u8]) -> Result<()> {
        let mut p = [0u8; PACKET];
        p[0] = REPORT_ID;
        p[1] = command;
        p[2..2 + data.len()].copy_from_slice(data);
        self.dev.write(&p).map(|_| ())
    }

    /// Show what was sent to the strips in `mask` (bit n is strip n); the others carry on.
    fn apply(&self, mask: u16) -> Result<()> {
        self.send(0x2c, &mask.to_be_bytes())
    }

    fn set_mode(&self, cable: Cable, mode: Builtin) -> Result<()> {
        for strip in cable.strips() {
            if let Some(palette) = mode.palette {
                self.send(CMD_COLOUR | strip, &colour_data(cable, &palette))?;
            }
            self.send(CMD_MODE | strip, &mode.data())?;
        }
        self.modes[cable.index()].set(mode);
        Ok(())
    }

    /// Show `atx` on the 24-pin cable and `gpu` on the GPU cable. A built-in effect that is
    /// already running is left alone, so it carries on instead of starting over; with `linked`,
    /// a change to either cable's built-in effect restarts both, so they stay in step.
    pub fn show(&self, atx: &Show, gpu: &Show, linked: bool) -> Result<()> {
        let shows = [(Cable::Atx, atx), (Cable::Gpu, gpu)];
        let builtin = |show: &Show| matches!(show, Show::Builtin(_));
        let mut send = shows.map(|(cable, show)| match show {
            Show::Frame(_) => true,
            Show::Builtin(mode) => *mode != self.modes[cable.index()].get(),
        });
        let confirm = matches!(self.confirm.get(), Confirm::Due(t) if t.elapsed() >= CONFIRM_AFTER);
        let restart_both = linked && shows.iter().zip(send).any(|((_, show), s)| s && builtin(show));
        for (i, (_, show)) in shows.iter().enumerate() {
            send[i] |= builtin(show) && (confirm || restart_both);
        }
        let mut mask = 0;
        for ((cable, show), _) in shows.iter().zip(send).filter(|(_, s)| *s) {
            match show {
                Show::Frame(colors) => {
                    if self.modes[cable.index()].get() != DIRECT {
                        self.set_mode(*cable, DIRECT)?;
                    }
                    let data = colour_data(*cable, colors);
                    for strip in cable.strips() {
                        self.send(CMD_COLOUR | strip, &data)?;
                    }
                }
                Show::Builtin(mode) => self.set_mode(*cable, *mode)?,
            }
            mask |= cable.mask();
        }
        let sent_builtin = shows.iter().zip(send).any(|((_, show), s)| s && builtin(show));
        match self.confirm.get() {
            _ if confirm => self.confirm.set(Confirm::Done),
            Confirm::Waiting if sent_builtin => self.confirm.set(Confirm::Due(Instant::now())),
            _ => {}
        }
        if mask != 0 { self.apply(mask) } else { Ok(()) }
    }

    /// Whether [`show`](Self::show) has a built-in effect to send again (see [`CONFIRM_AFTER`]).
    pub fn confirm_due(&self) -> bool {
        matches!(self.confirm.get(), Confirm::Due(t) if t.elapsed() >= CONFIRM_AFTER)
    }

    /// Show one frame per cable. Each frame is copied onto all six of the cable's strips.
    pub fn write_colors(&self, atx: &[[u8; 3]], gpu: &[[u8; 3]]) -> Result<()> {
        self.show(&Show::Frame(atx.to_vec()), &Show::Frame(gpu.to_vec()), false)
    }

    /// Back to the controller's own rainbow, which it runs without software.
    pub fn restore_hardware_mode(&self) {
        let rainbow = Builtin { mode: MODE_RAINBOW, ..DIRECT };
        let _ = Cable::ALL
            .into_iter()
            .try_for_each(|cable| self.set_mode(cable, rainbow))
            .and_then(|()| self.apply(Cable::Atx.mask() | Cable::Gpu.mask()));
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
        let all = Lighting::default();
        let wave = Lighting { effect: Effect::RainbowWave, ..Default::default() };
        let b = Builtin::for_lighting(&wave, &all).unwrap();
        assert_eq!(b.data(), [MODE_RAINBOW, 0x00, 0x00, 0x00]);
        let slow_dim = Lighting { speed: 1, brightness: 30, reverse: true, ..wave };
        assert_eq!(Builtin::for_lighting(&slow_dim, &all).unwrap().data(), [MODE_RAINBOW, 0x02, 0x01, 0x03]);
        let fast = Lighting { effect: Effect::Rainbow, speed: 100, ..Default::default() };
        assert_eq!(Builtin::for_lighting(&fast, &all).unwrap().data()[..2], [MODE_RAINBOW_MORPH, 0xff]);
        for effect in [Effect::Static, Effect::Spinner, Effect::Ambilight, Effect::Temperature] {
            assert_eq!(Builtin::for_lighting(&Lighting { effect, ..Default::default() }, &all), None);
        }
    }

    #[test]
    fn strimer_animations_use_the_all_devices_colours() {
        let two = Lighting {
            effect: Effect::ColorWave,
            colors: vec!["#ff0000".into(), "#0000ff".into()],
            ..Default::default()
        };
        let (red, blue) = ([255, 0, 0], [0, 0, 255]);
        assert_eq!(palette(&two), [red, blue, red, blue, red, blue]);
        let one = Lighting { effect: Effect::Static, ..two.clone() };
        assert_eq!(palette(&one), [red; PALETTE]);
        assert_eq!(palette(&Lighting { effect: Effect::RainbowWave, ..two.clone() }), RAINBOW);

        let ping_pong = Lighting { effect: Effect::PingPong, ..Default::default() };
        let b = Builtin::for_lighting(&ping_pong, &two).unwrap();
        assert_eq!((b.data()[0], b.palette), (0x1b, Some([red, blue, red, blue, red, blue])));
        for effect in Effect::STRIMER {
            assert!(Builtin::for_lighting(&Lighting { effect, ..Default::default() }, &two).is_some(), "{effect:?}");
        }
    }

    #[test]
    fn own_colours_replace_the_all_devices_ones() {
        let all = Lighting { effect: Effect::Static, colors: vec!["#ff0000".into()], ..Default::default() };
        let mut cfg =
            Lighting { effect: Effect::Meteor, colors: vec!["#00ff00".into(), "#0000ff".into()], ..Default::default() };
        assert_eq!(palette_for(&cfg, &all), [[255, 0, 0]; PALETTE]);
        cfg.own_colors = true;
        assert_eq!(palette_for(&cfg, &all)[..2], [[0, 255, 0], [0, 0, 255]]);
    }

    #[test]
    fn apply_masks_match_the_tested_packets() {
        assert_eq!(Cable::Atx.mask().to_be_bytes(), [0x00, 0x3f]);
        assert_eq!(Cable::Gpu.mask().to_be_bytes(), [0x0f, 0xc0]);
        assert_eq!((Cable::Atx.mask() | Cable::Gpu.mask()).to_be_bytes(), [0x0f, 0xff]);
    }

    #[test]
    fn cables_use_separate_strips() {
        assert_eq!(Cable::Atx.strips().collect::<Vec<_>>(), (0..6).collect::<Vec<_>>());
        assert_eq!(Cable::Gpu.strips().collect::<Vec<_>>(), (6..12).collect::<Vec<_>>());
    }
}
