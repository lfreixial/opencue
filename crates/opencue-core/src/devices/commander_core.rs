//! Corsair iCUE Commander Core: the pump / fan / RGB hub inside Elite Capellix and Elite LCD AIOs.
//!
//! Reports have no report id:
//!   host -> device: 00 08 <command...> <payload...>
//!   device -> host: 00 <command echo> <status> <data...>
//! Data is read and written through "endpoints": close, open, read/write, close.

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::hidraw::{HidDevice, HidError, HidInfo, Result};

pub const PIDS: &[u16] = &[0x0c1c, 0x0c32];

const CMD_SOFTWARE_MODE: &[u8] = &[0x01, 0x03, 0x00, 0x02];
const CMD_HARDWARE_MODE: &[u8] = &[0x01, 0x03, 0x00, 0x01];
const CMD_GET_FIRMWARE: &[u8] = &[0x02, 0x13];
const CMD_GET_PUMP_VERSION: &[u8] = &[0x02, 0x57];
const CMD_GET_RADIATOR: &[u8] = &[0x02, 0x58];
const CMD_OPEN_ENDPOINT: &[u8] = &[0x0d, 0x01];
const CMD_OPEN_COLOR_ENDPOINT: &[u8] = &[0x0d, 0x00];
const CMD_CLOSE_ENDPOINT: &[u8] = &[0x05, 0x01, 0x01];
const CMD_READ: &[u8] = &[0x08, 0x01];
const CMD_WRITE: &[u8] = &[0x06, 0x01];
const CMD_WRITE_COLOR: &[u8] = &[0x06, 0x00];
const CMD_WRITE_COLOR_MORE: &[u8] = &[0x07, 0x00];
const EP_SET_LED_PORTS: &[u8] = &[0x1e];

const EP_LED_COUNTS: &[u8] = &[0x20];
const EP_SPEEDS: &[u8] = &[0x17];
const EP_SET_SPEED: &[u8] = &[0x18];
const EP_TEMPS: &[u8] = &[0x21];
const EP_FAN_STATUS: &[u8] = &[0x1a];
const EP_COLOR: &[u8] = &[0x22];

const DT_SET_SPEED: &[u8] = &[0x07, 0x00];
const DT_SET_COLOR: &[u8] = &[0x12, 0x00];

pub const CHANNELS: u8 = 7; // 0 = pump, 1..=6 = fan / RGB ports
pub const PUMP_MIN_DUTY: u8 = 50;

/// LED device type code sent when enabling an RGB port, by LED count.
fn led_type(count: u16) -> Option<u8> {
    Some(match count {
        4 => 0x03,
        8 => 0x05,
        10 => 0x01,
        12 => 0x04,
        16 => 0x02,
        21 | 24 | 29 => 0x08,
        34 => 0x06,
        _ => return None,
    })
}

pub fn led_device_name(count: u16) -> &'static str {
    match count {
        4 => "ML PRO RGB fan",
        8 => "RGB fan (8 LED)",
        10 => "RGB LED strip",
        12 => "HD RGB fan",
        16 => "LL RGB fan",
        21 | 24 | 29 => "Pump",
        34 => "QL RGB fan",
        _ => "RGB device",
    }
}

const AIO_MODELS: &[(u8, u16, bool, &str)] = &[
    (1, 240, false, "H100i ELITE CAPELLIX"),
    (2, 240, false, "H100i ELITE CAPELLIX"),
    (1, 280, false, "H115i ELITE CAPELLIX"),
    (1, 360, false, "H150i ELITE CAPELLIX"),
    (2, 360, false, "H150i ELITE CAPELLIX"),
    (1, 420, false, "H170i ELITE CAPELLIX"),
    (3, 240, true, "H100i ELITE LCD"),
    (3, 360, true, "H150i ELITE LCD"),
    (3, 420, true, "H170i ELITE LCD"),
    (3, 240, false, "H100i ELITE CAPELLIX"),
    (3, 360, false, "H150i ELITE CAPELLIX"),
    (3, 420, false, "H170i ELITE CAPELLIX"),
    (5, 240, true, "H100i ELITE LCD XT"),
    (6, 240, true, "H100i ELITE LCD XT"),
    (5, 280, true, "H115i ELITE LCD XT"),
    (5, 360, true, "H150i ELITE LCD XT"),
    (6, 360, true, "H150i ELITE LCD XT"),
    (5, 420, true, "H170i ELITE LCD XT"),
    (5, 240, false, "H100i ELITE CAPELLIX XT"),
    (6, 240, false, "H100i ELITE CAPELLIX XT"),
    (5, 280, false, "H115i ELITE CAPELLIX XT"),
    (5, 360, false, "H150i ELITE CAPELLIX XT"),
    (6, 360, false, "H150i ELITE CAPELLIX XT"),
    (5, 420, false, "H170i ELITE CAPELLIX XT"),
];

pub fn channel_name(ch: u8) -> String {
    if ch == 0 { "pump".into() } else { format!("fan{ch}") }
}

#[derive(Debug, Clone, Default)]
pub struct CoreStatus {
    pub connected: BTreeMap<u8, bool>,
    pub rpm: BTreeMap<u8, u16>,
    pub liquid_temp: Option<f32>,
    pub probes: BTreeMap<u8, f32>,
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

pub struct CommanderCore {
    dev: HidDevice,
    report_size: usize,
    has_lcd: bool,
    pub firmware: String,
    pub model: String,
    /// channel -> LED count, for every RGB-capable channel.
    pub led_counts: BTreeMap<u8, u16>,
    /// channels with a fan or pump attached.
    pub fan_channels: Vec<u8>,
}

impl CommanderCore {
    pub fn open(info: &HidInfo, has_lcd: bool) -> Result<Self> {
        let mut cc = Self {
            dev: HidDevice::open(info)?,
            report_size: if info.pid == 0x0c1c { 96 } else { 64 },
            has_lcd,
            firmware: "?".into(),
            model: "Unknown AIO".into(),
            led_counts: BTreeMap::new(),
            fan_channels: Vec::new(),
        };
        cc.transfer(CMD_SOFTWARE_MODE, &[])?;
        let fw = cc.transfer(CMD_GET_FIRMWARE, &[])?;
        cc.firmware = format!("{}.{}.{}", fw[3], fw[4], u16le(&fw, 5));
        cc.model = cc.detect_model()?;
        cc.led_counts = cc.read_led_counts()?;
        cc.fan_channels = cc.read_fan_status()?.into_iter().filter(|(_, ok)| *ok).map(|(ch, _)| ch).collect();
        cc.setup_lighting()?;
        log::info!(
            "Commander Core {}: {} fw {}, fans {:?}, leds {:?}",
            info.serial,
            cc.model,
            cc.firmware,
            cc.fan_channels,
            cc.led_counts
        );
        Ok(cc)
    }

    pub fn restore_hardware_mode(&self) {
        let _ = self.transfer_once(CMD_HARDWARE_MODE, &[]);
    }

    fn transfer_once(&self, command: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        let mut packet = vec![0u8; self.report_size + 1];
        packet[1] = 0x08;
        packet[2..2 + command.len()].copy_from_slice(command);
        packet[2 + command.len()..2 + command.len() + payload.len()].copy_from_slice(payload);
        self.dev.drain(self.report_size)?;
        self.dev.write(&packet)?;
        let deadline = Instant::now() + Duration::from_secs(1);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let res = self.dev.read(self.report_size, left)?;
            if res.is_empty() {
                break;
            }
            if res.len() >= 3 && res[0] == 0x00 && res[1] == command[0] {
                return Ok(res);
            }
        }
        Err(HidError::Protocol(format!("Commander Core: no reply to {:02x?}", command)))
    }

    fn transfer(&self, command: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        let mut last = None;
        for _ in 0..3 {
            match self.transfer_once(command, payload) {
                Ok(r) => return Ok(r),
                Err(e @ HidError::Protocol(_)) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap())
    }

    fn read_endpoint(&self, ep: &[u8]) -> Result<Vec<u8>> {
        self.transfer(CMD_CLOSE_ENDPOINT, ep)?;
        self.transfer(CMD_OPEN_ENDPOINT, ep)?;
        let data = self.transfer(CMD_READ, ep)?;
        self.transfer(CMD_CLOSE_ENDPOINT, ep)?;
        Ok(data)
    }

    fn write_endpoint(&self, ep: &[u8], data_type: &[u8], data: &[u8], len_includes_type: bool) -> Result<Vec<u8>> {
        let len = data.len() + if len_includes_type { 2 } else { 0 };
        let mut buf = Vec::with_capacity(4 + data_type.len() + data.len());
        buf.extend_from_slice(&(len as u16).to_le_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(data_type);
        buf.extend_from_slice(data);
        self.transfer(CMD_CLOSE_ENDPOINT, ep)?;
        self.transfer(CMD_OPEN_ENDPOINT, ep)?;
        let res = self.transfer(CMD_WRITE, &buf)?;
        self.transfer(CMD_CLOSE_ENDPOINT, ep)?;
        Ok(res)
    }

    fn detect_model(&self) -> Result<String> {
        let pump = self.transfer(CMD_GET_PUMP_VERSION, &[])?[3];
        let radiator = u16le(&self.transfer(CMD_GET_RADIATOR, &[])?, 3);
        Ok(AIO_MODELS
            .iter()
            .find(|(v, r, lcd, _)| *v == pump && *r == radiator && *lcd == self.has_lcd)
            .map(|m| m.3.to_string())
            .unwrap_or_else(|| format!("Corsair AIO (pump v{pump}, {radiator} mm)")))
    }

    fn read_fan_status(&self) -> Result<BTreeMap<u8, bool>> {
        let res = self.read_endpoint(EP_FAN_STATUS)?;
        let count = res[5].min(CHANNELS) as usize;
        // 0x07: connected and spinning, 0x02: connected but stopped (zero RPM)
        Ok((0..count).map(|ch| (ch as u8, matches!(res[6 + ch], 0x07 | 0x02))).collect())
    }

    fn read_led_counts(&self) -> Result<BTreeMap<u8, u16>> {
        let res = self.read_endpoint(EP_LED_COUNTS)?;
        log::debug!("LED endpoint: {:02x?}", &res[..40.min(res.len())]);
        let mut counts = BTreeMap::new();
        for ch in 0..CHANNELS as usize {
            let off = 6 + ch * 4;
            if off + 4 > res.len() {
                break;
            }
            // Byte 0 is the status (2 = connected); byte 1 is not part of it (the pump reports 0x03 there).
            let (status, num) = (res[off], u16le(&res, off + 2));
            if status == 2 && led_type(num).is_some() {
                counts.insert(ch as u8, num);
            }
        }
        Ok(counts)
    }

    pub fn status(&self) -> Result<CoreStatus> {
        let mut st = CoreStatus { connected: self.read_fan_status()?, ..Default::default() };
        let res = self.read_endpoint(EP_SPEEDS)?;
        for ch in 0..res[5].min(CHANNELS) {
            let off = 6 + ch as usize * 2;
            if st.connected.get(&ch) == Some(&true) && off + 2 <= res.len() {
                st.rpm.insert(ch, u16le(&res, off));
            }
        }
        let res = self.read_endpoint(EP_TEMPS)?;
        for i in 0..res[5] as usize {
            let off = 6 + i * 3;
            if off + 3 > res.len() || res[off] != 0x00 {
                continue;
            }
            let t = i16::from_le_bytes([res[off + 1], res[off + 2]]) as f32 / 10.0;
            if !(0.0..100.0).contains(&t) {
                continue;
            }
            if i == 0 {
                st.liquid_temp = Some(t)
            } else {
                st.probes.insert(i as u8, t);
            }
        }
        Ok(st)
    }

    /// Set duty (percent) per channel. The pump never goes below PUMP_MIN_DUTY.
    pub fn set_duties(&self, duties: &BTreeMap<u8, u8>) -> Result<()> {
        if duties.is_empty() {
            return Ok(());
        }
        let mut buf = vec![duties.len() as u8];
        for (&ch, &duty) in duties {
            let duty = if ch == 0 { duty.max(PUMP_MIN_DUTY) } else { duty }.min(100);
            buf.extend_from_slice(&[ch, 0x00, duty, 0x00]); // 0x00 = fixed percent mode
        }
        for _ in 0..5 {
            let res = self.write_endpoint(EP_SET_SPEED, DT_SET_SPEED, &buf, true)?;
            if res[2] == 0x00 {
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
        log::warn!("Commander Core did not acknowledge speed change");
        Ok(())
    }

    fn setup_lighting(&self) -> Result<()> {
        for port in 0..CHANNELS {
            self.transfer(&[0x14, port, 0x01], &[])?;
        }
        sleep(Duration::from_millis(500));
        let mut ports = vec![0x0d, 0x00, 0x07];
        for ch in 0..CHANNELS {
            match self.led_counts.get(&ch).and_then(|&n| led_type(n)) {
                Some(t) => ports.extend_from_slice(&[0x01, t]),
                None => ports.push(0x00),
            }
        }
        self.write_endpoint(EP_SET_LED_PORTS, &[], &ports, false)?;
        self.transfer(CMD_CLOSE_ENDPOINT, EP_COLOR)?;
        self.transfer(CMD_OPEN_COLOR_ENDPOINT, EP_COLOR)?;
        Ok(())
    }

    /// colors: channel -> one RGB triple per LED on that channel.
    pub fn write_colors(&self, colors: &BTreeMap<u8, Vec<[u8; 3]>>) -> Result<()> {
        let mut rgb = Vec::new();
        for (ch, &count) in &self.led_counts {
            let leds = colors.get(ch);
            for i in 0..count as usize {
                rgb.extend_from_slice(&leds.and_then(|l| l.get(i)).copied().unwrap_or([0, 0, 0]));
            }
        }
        let mut buf = Vec::with_capacity(rgb.len() + 6);
        buf.extend_from_slice(&((rgb.len() + 2) as u16).to_le_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(DT_SET_COLOR);
        buf.extend_from_slice(&rgb);
        let chunk = self.report_size - 3;
        for (i, part) in buf.chunks(chunk).enumerate() {
            self.transfer(if i == 0 { CMD_WRITE_COLOR } else { CMD_WRITE_COLOR_MORE }, part)?;
        }
        Ok(())
    }
}
