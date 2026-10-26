//! The OpenCUE daemon: owns the hardware, runs fan curves, lighting, the LCD and the Nexus.
//!
//! Every device gets its own worker thread. A worker that hits an error (unplugged device,
//! resume from suspend, protocol hiccup) exits and the manager reopens the device a few
//! seconds later, so one misbehaving device never takes the others down.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use opencue_core::config::{Action, Config, LcdMode, NexusMode, Profile, Sensor};
use opencue_core::cooling::{DutyController, curve};
use opencue_core::devices::commander_core::{self, CommanderCore, channel_name, led_device_name};
use opencue_core::devices::lcd_cap::LcdCap;
use opencue_core::devices::mm700::{self, Mm700};
use opencue_core::devices::nexus::{Nexus, Touch};
use opencue_core::devices::{Found, Kind, discover};
use opencue_core::gamestats::{self, LogTracker};
use opencue_core::ipc::{Request, Response, socket_path};
use opencue_core::lighting;
use opencue_core::render;
use opencue_core::sensors::{SensorReader, SystemSensors};
use opencue_core::telemetry::{ChannelState, DeviceState, LightingZone, Telemetry};

use crate::{actions, media};

/// A gap this long between loop iterations means the machine was suspended.
const RESUME_GAP: Duration = Duration::from_secs(5);
const FRAME: Duration = Duration::from_millis(33);

pub struct Shared {
    config: RwLock<Config>,
    rev: AtomicU64,
    telemetry: Mutex<Telemetry>,
    stop: Arc<AtomicBool>,
    rescan: AtomicBool,
    nexus_page: AtomicUsize,
    /// Media commands for the MPRIS thread.
    media: mpsc::Sender<String>,
    epoch: Instant,
}

impl Shared {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    fn rev(&self) -> u64 {
        self.rev.load(Ordering::Acquire)
    }

    fn set_config(&self, cfg: Config) {
        let cfg = cfg.normalize();
        if let Err(e) = cfg.save() {
            log::error!("cannot save config: {e}");
        }
        *self.config.write().unwrap() = cfg;
        self.rev.fetch_add(1, Ordering::Release);
    }

    fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.config();
        f(&mut cfg);
        self.set_config(cfg);
    }

    fn telemetry<T>(&self, f: impl FnOnce(&mut Telemetry) -> T) -> T {
        f(&mut self.telemetry.lock().unwrap())
    }

    fn set_device(&self, state: DeviceState) {
        self.telemetry(|t| {
            t.devices.retain(|d| d.key != state.key);
            t.devices.push(state);
            t.devices.sort_by(|a, b| a.key.cmp(&b.key));
        });
    }

    fn device_error(&self, found: &Found, error: Option<String>) {
        let key = device_key(found);
        self.telemetry(|t| {
            if let Some(d) = t.devices.iter_mut().find(|d| d.key == key) {
                d.error = error;
            } else {
                t.devices.push(DeviceState {
                    key,
                    kind: found.kind,
                    name: found.info.name.clone(),
                    firmware: "?".into(),
                    serial: found.info.serial.clone(),
                    error,
                });
            }
        });
    }

    fn remove_device(&self, key: &str) {
        self.telemetry(|t| t.devices.retain(|d| d.key != key));
    }

    fn elapsed(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }
}

fn device_key(found: &Found) -> String {
    let id = if found.info.serial.is_empty() { &found.info.usb_path } else { &found.info.serial };
    format!("{:?}:{id}", found.kind).to_lowercase()
}

fn sensor_value(sensor: Sensor, liquid: Option<f32>, sys: &SystemSensors) -> Option<f32> {
    match sensor {
        Sensor::Liquid => liquid,
        Sensor::Cpu => sys.cpu_temp,
        Sensor::Gpu => sys.gpu_temp,
    }
}

/// Sleep in short steps so a stop request is noticed quickly.
fn nap(shared: &Shared, d: Duration) {
    let end = Instant::now() + d;
    while !shared.stopping() {
        let Some(left) = end.checked_duration_since(Instant::now()) else { break };
        thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// Lighting frame for one zone.
fn zone_frame(
    profile: &Profile,
    zone: &str,
    leds: usize,
    phase: f32,
    t: f64,
    liquid: Option<f32>,
    sys: &SystemSensors,
) -> Vec<[u8; 3]> {
    let cfg = profile.lighting_for(zone);
    lighting::render(&cfg, t, leds, phase, sensor_value(cfg.sensor, liquid, sys))
}

fn zones_animated(profile: &Profile, zones: &[&str]) -> bool {
    zones.iter().any(|z| profile.lighting_for(z).effect.is_animated())
}

// ---------------------------------------------------------------------------------------------
// Commander Core: fans, pump, liquid temperature and RGB

fn run_commander_core(sh: &Shared, found: &Found, has_lcd: bool) -> Result<()> {
    let cc = CommanderCore::open(&found.info, has_lcd)?;
    let key = device_key(found);
    sh.set_device(DeviceState {
        key: key.clone(),
        kind: Kind::CommanderCore,
        name: cc.model.clone(),
        firmware: cc.firmware.clone(),
        serial: found.info.serial.clone(),
        error: None,
    });
    let zones: Vec<(u8, String)> = cc.led_counts.keys().map(|&ch| (ch, format!("cc:{}", channel_name(ch)))).collect();
    sh.telemetry(|t| {
        t.zones.retain(|z| !z.id.starts_with("cc:"));
        for (ch, id) in &zones {
            let count = cc.led_counts[ch];
            let name = if *ch == 0 { "Pump".to_string() } else { format!("Fan {ch} · {}", led_device_name(count)) };
            t.zones.push(LightingZone { id: id.clone(), name, leds: count as usize });
        }
    });

    let result = commander_core_loop(sh, &cc, &zones);

    if sh.stopping() && sh.config().settings.restore_hardware_on_exit {
        cc.restore_hardware_mode();
    }
    sh.telemetry(|t| {
        t.zones.retain(|z| !z.id.starts_with("cc:"));
        t.channels.clear();
        t.liquid_temp = None;
    });
    result
}

fn commander_core_loop(sh: &Shared, cc: &CommanderCore, zones: &[(u8, String)]) -> Result<()> {
    let zone_ids: Vec<&str> = zones.iter().map(|(_, id)| id.as_str()).collect();
    let mut controllers: BTreeMap<u8, DutyController> = BTreeMap::new();
    let mut last_duties: BTreeMap<u8, u8> = BTreeMap::new();
    let mut last_duty_write = Instant::now();
    let mut next_status = Instant::now();
    let mut last_colors: Option<BTreeMap<u8, Vec<[u8; 3]>>> = None;
    let mut last_color_write = Instant::now();
    let mut seen_rev = u64::MAX;
    let mut liquid = None;
    let mut last_iter = Instant::now();

    while !sh.stopping() {
        let now = Instant::now();
        if now - last_iter > RESUME_GAP {
            bail!("resumed from suspend; reinitialising");
        }
        last_iter = now;
        let cfg = sh.config();
        let rev = sh.rev();
        let profile = cfg.profile();
        let sys = sh.telemetry(|t| t.system.clone());

        // Cooling, once a second (immediately after a config change).
        if now >= next_status || rev != seen_rev {
            next_status = now + Duration::from_secs(1);
            let st = cc.status()?;
            liquid = st.liquid_temp;
            let emergency = liquid.is_some_and(|t| t >= cfg.settings.critical_liquid_temp)
                || sys.cpu_temp.is_some_and(|t| t >= cfg.settings.critical_cpu_temp);
            let mut duties = BTreeMap::new();
            for &ch in &cc.fan_channels {
                let pump = ch == 0;
                let ccfg = profile.cooling_for(&channel_name(ch));
                let min = if pump { commander_core::PUMP_MIN_DUTY as f32 } else { cfg.settings.min_fan_duty as f32 };
                let ctrl = controllers.entry(ch).or_default();
                let temp = sensor_value(ccfg.sensor, liquid, &sys);
                let duty = ctrl.update(&curve(&ccfg, pump), temp, min, rev != seen_rev);
                duties.insert(ch, if emergency { 100 } else { duty });
            }
            if duties != last_duties || now - last_duty_write > Duration::from_secs(10) {
                cc.set_duties(&duties)?;
                last_duties = duties.clone();
                last_duty_write = now;
            }
            sh.telemetry(|t| {
                t.liquid_temp = liquid;
                t.emergency = emergency;
                t.channels = cc
                    .fan_channels
                    .iter()
                    .map(|&ch| {
                        let name = if ch == 0 { "Pump".into() } else { format!("Fan {ch}") };
                        (
                            channel_name(ch),
                            ChannelState { name, rpm: st.rpm.get(&ch).copied(), duty: duties.get(&ch).copied() },
                        )
                    })
                    .collect();
            });
            if emergency && rev == seen_rev {
                log::warn!("critical temperature (liquid {liquid:?}, cpu {:?}): all fans at 100%", sys.cpu_temp);
            }
        }
        seen_rev = rev;

        // Lighting
        let t = sh.elapsed();
        let n = zones.len().max(1) as f32;
        let colors: BTreeMap<u8, Vec<[u8; 3]>> = zones
            .iter()
            .enumerate()
            .map(|(i, (ch, id))| {
                (*ch, zone_frame(profile, id, cc.led_counts[ch] as usize, i as f32 / n, t, liquid, &sys))
            })
            .collect();
        if last_colors.as_ref() != Some(&colors) || now - last_color_write > Duration::from_secs(5) {
            cc.write_colors(&colors)?;
            last_colors = Some(colors);
            last_color_write = now;
        }

        let pause = if zones_animated(profile, &zone_ids) { FRAME } else { Duration::from_millis(200) };
        thread::sleep(pause.saturating_sub(now.elapsed()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Pump LCD

struct Media {
    key: String,
    frames: Vec<(Vec<u8>, Duration)>,
}

fn run_lcd(sh: &Shared, found: &Found) -> Result<()> {
    let lcd = LcdCap::open(&found.info)?;
    sh.set_device(DeviceState {
        key: device_key(found),
        kind: Kind::LcdCap,
        name: "Pump LCD".into(),
        firmware: "-".into(),
        serial: found.info.serial.clone(),
        error: None,
    });
    let result = lcd_loop(sh, &lcd);
    if sh.stopping() && sh.config().settings.restore_hardware_on_exit {
        lcd.restore_hardware_mode();
    }
    result
}

fn lcd_loop(sh: &Shared, lcd: &LcdCap) -> Result<()> {
    let mut applied: Option<(u8, u16)> = None;
    let mut media: Option<Media> = None;
    let mut frame = 0usize;
    let mut next = Instant::now();
    let mut seen_rev = u64::MAX;
    let mut last_iter = Instant::now();

    while !sh.stopping() {
        let now = Instant::now();
        if now - last_iter > RESUME_GAP {
            bail!("resumed from suspend; reinitialising");
        }
        last_iter = now;
        let cfg = sh.config();
        let rev = sh.rev();
        let lc = cfg.profile().lcd.clone();
        if applied != Some((lc.brightness, lc.rotation)) {
            lcd.set_brightness(lc.brightness)?;
            lcd.set_rotation(lc.rotation)?;
            applied = Some((lc.brightness, lc.rotation));
        }
        if rev != seen_rev {
            next = now; // redraw immediately on config change
            seen_rev = rev;
        }

        let want_media = lc.mode == LcdMode::Image && !lc.image.is_empty();
        if want_media {
            let key = format!("{}|{}|{}", lc.image, lc.cover, lc.background);
            if media.as_ref().is_none_or(|m| m.key != key) {
                let frames = match render::lcd::load_media(
                    lc.image.as_ref(),
                    lc.cover,
                    opencue_core::config::parse_color(&lc.background),
                ) {
                    Ok(frames) => {
                        log::info!("LCD: loaded {} ({} frames)", lc.image, frames.len());
                        frames.iter().map(|f| (render::lcd::encode_jpeg(&f.image), f.delay)).collect()
                    }
                    Err(e) => {
                        log::error!("LCD: cannot load {}: {e:#}", lc.image);
                        Vec::new()
                    }
                };
                media = Some(Media { key, frames });
                frame = 0;
                next = Instant::now();
            }
        }

        if Instant::now() >= next {
            match media.as_ref().filter(|m| want_media && !m.frames.is_empty()) {
                Some(m) => {
                    let (jpeg, delay) = &m.frames[frame % m.frames.len()];
                    lcd.write_jpeg(jpeg)?;
                    frame = (frame + 1) % m.frames.len();
                    // Still images are re-sent periodically so the panel never falls back to its own screen.
                    next += if m.frames.len() == 1 { Duration::from_millis(500) } else { *delay };
                    if next < Instant::now() {
                        next = Instant::now(); // running behind (slow USB); don't try to catch up
                    }
                }
                None => {
                    let t = sh.telemetry(|t| t.clone());
                    let img = render::lcd::render(&lc, &t, cfg.settings.fahrenheit);
                    lcd.write_jpeg(&render::lcd::encode_jpeg(&img))?;
                    next = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        thread::sleep(next.saturating_duration_since(Instant::now()).min(Duration::from_millis(50)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// iCUE NEXUS

fn run_nexus(sh: &Arc<Shared>, found: &Found) -> Result<()> {
    let (nexus, touch) = Nexus::open(&found.info)?;
    sh.set_device(DeviceState {
        key: device_key(found),
        kind: Kind::Nexus,
        name: "iCUE NEXUS".into(),
        firmware: nexus.firmware.clone(),
        serial: found.info.serial.clone(),
        error: None,
    });

    let (tx, rx) = mpsc::channel::<Touch>();
    let reader_stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let reader_stop = reader_stop.clone();
        thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                match touch.read(Duration::from_millis(200)) {
                    Ok(Some(ev)) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("Nexus touch reader stopped: {e}");
                        break;
                    }
                }
            }
        })
    };

    let result = nexus_loop(sh, &nexus, &rx);
    reader_stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    if sh.stopping() && sh.config().settings.restore_hardware_on_exit {
        nexus.restore_hardware_mode();
    }
    result
}

fn nexus_loop(sh: &Arc<Shared>, nexus: &Nexus, touches: &mpsc::Receiver<Touch>) -> Result<()> {
    let mut brightness = None;
    let mut pressed: Option<usize> = None;
    let mut touching = false;
    let mut next_render = Instant::now();
    let mut seen_rev = u64::MAX;
    let mut seen_page = usize::MAX;

    while !sh.stopping() {
        let cfg = sh.config();
        let nx = &cfg.nexus;
        if brightness != Some(nx.brightness) {
            nexus.set_brightness(nx.brightness)?;
            brightness = Some(nx.brightness);
        }
        let page = sh.nexus_page.load(Ordering::Relaxed) % nx.pages.len().max(1);
        if sh.rev() != seen_rev || page != seen_page {
            seen_rev = sh.rev();
            seen_page = page;
            next_render = Instant::now();
        }

        if Instant::now() >= next_render {
            let t = sh.telemetry(|t| t.clone());
            let img = render::nexus::render(nx, page, pressed, &t, cfg.settings.fahrenheit);
            nexus.write_rgba(img.as_raw())?;
            // The progress bar moves, and track changes should show up quickly.
            let live = matches!(nx.mode, NexusMode::Media | NexusMode::Gaming);
            let every = if live { Duration::from_millis(500) } else { Duration::from_secs(1) };
            next_render = Instant::now() + every;
        }

        match touches
            .recv_timeout(next_render.saturating_duration_since(Instant::now()).min(Duration::from_millis(200)))
        {
            Ok(ev) => {
                log::debug!("Nexus touch: pressed={} x={}", ev.pressed, ev.x);
                if ev.pressed && !touching {
                    touching = true;
                    if nx.mode == NexusMode::Media {
                        let control = render::nexus::media_control_at(ev.x);
                        let cmd = control.map_or("switch_player", |i| render::nexus::MEDIA_CONTROLS[i]);
                        log::info!("Nexus: media '{cmd}' (touch x={})", ev.x);
                        pressed = control;
                        let _ = sh.media.send(cmd.into());
                    } else if nx.mode == NexusMode::Buttons {
                        let buttons = &nx.pages[page].buttons;
                        if let Some(i) = render::nexus::button_at(ev.x, buttons.len()) {
                            log::info!("Nexus: '{}' pressed (touch x={})", buttons[i].label, ev.x);
                            pressed = Some(i);
                            let action = buttons[i].action.clone();
                            let sh = sh.clone();
                            thread::spawn(move || {
                                if let Err(e) = perform(&sh, &action) {
                                    log::error!("Nexus action {action:?} failed: {e:#}");
                                }
                            });
                        }
                    }
                    next_render = Instant::now();
                } else if !ev.pressed && touching {
                    touching = false;
                    pressed = None;
                    next_render = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("Nexus touch reader stopped"),
        }
    }
    Ok(())
}

pub fn perform(sh: &Shared, action: &Action) -> Result<()> {
    match action {
        Action::None => Ok(()),
        Action::Keys(combo) => actions::press_combo(&actions::parse_combo(combo)?),
        // Volume is the system's; everything else goes to the media player.
        Action::Media(key) if matches!(key.as_str(), "volume_up" | "volume_down" | "mute") => {
            actions::press_combo(&[actions::key_code(key).ok_or_else(|| anyhow!("unknown media key {key}"))?])
        }
        Action::Media(cmd) => {
            sh.media.send(cmd.clone()).map_err(|_| anyhow!("media thread stopped"))?;
            Ok(())
        }
        Action::Command(cmd) => actions::run_command(cmd),
        Action::Url(url) => actions::open_url(url),
        Action::Text(text) => actions::type_text(text),
        Action::Profile(name) => {
            if !sh.config().profiles.contains_key(name) {
                bail!("no profile named {name}");
            }
            sh.update_config(|c| c.active_profile = name.clone());
            Ok(())
        }
        Action::Page(which) => {
            let pages = sh.config().nexus.pages.len().max(1);
            let cur = sh.nexus_page.load(Ordering::Relaxed) % pages;
            let next = match which.as_str() {
                "next" => (cur + 1) % pages,
                "prev" | "previous" => (cur + pages - 1) % pages,
                n => {
                    n.parse::<usize>().context("page must be next, prev or a number")?.saturating_sub(1).min(pages - 1)
                }
            };
            sh.nexus_page.store(next, Ordering::Relaxed);
            sh.telemetry(|t| t.nexus_page = next);
            Ok(())
        }
        Action::CycleLcd => {
            sh.update_config(|c| {
                let lcd = &mut c.profile_mut().lcd;
                let modes = LcdMode::ALL;
                let mut i = modes.iter().position(|m| *m == lcd.mode).unwrap_or(0);
                loop {
                    i = (i + 1) % modes.len();
                    let m = modes[i];
                    if !(m == LcdMode::Image && lcd.image.is_empty()) {
                        lcd.mode = m;
                        break;
                    }
                }
            });
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// MM700 mousepad

fn run_mm700(sh: &Shared, found: &Found) -> Result<()> {
    let pad = Mm700::open(&found.info)?;
    sh.set_device(DeviceState {
        key: device_key(found),
        kind: Kind::Mm700,
        name: "MM700 RGB mousepad".into(),
        firmware: pad.firmware.clone(),
        serial: found.info.serial.clone(),
        error: None,
    });
    sh.telemetry(|t| {
        t.zones.retain(|z| z.id != "mm700");
        t.zones.push(LightingZone { id: "mm700".into(), name: "MM700 mousepad".into(), leds: mm700::ZONES });
    });
    let result = (|| -> Result<()> {
        let mut last: Option<Vec<[u8; 3]>> = None;
        let mut last_write = Instant::now();
        let mut last_keepalive = Instant::now();
        while !sh.stopping() {
            let now = Instant::now();
            let cfg = sh.config();
            let (liquid, sys) = sh.telemetry(|t| (t.liquid_temp, t.system.clone()));
            let colors = zone_frame(cfg.profile(), "mm700", mm700::ZONES, 0.5, sh.elapsed(), liquid, &sys);
            if last.as_ref() != Some(&colors) || now - last_write > Duration::from_secs(5) {
                pad.write_colors(&colors)?;
                last = Some(colors);
                last_write = now;
            }
            if now - last_keepalive > Duration::from_secs(20) {
                pad.keepalive()?;
                last_keepalive = now;
            }
            let pause = if zones_animated(cfg.profile(), &["mm700"]) { FRAME } else { Duration::from_millis(200) };
            thread::sleep(pause.saturating_sub(now.elapsed()));
        }
        Ok(())
    })();
    if sh.stopping() && sh.config().settings.restore_hardware_on_exit {
        pad.restore_hardware_mode();
    }
    sh.telemetry(|t| t.zones.retain(|z| z.id != "mm700"));
    result
}

// ---------------------------------------------------------------------------------------------
// Media players (MPRIS)

/// Tracks the active player while the Nexus shows it, and runs media commands at any time.
/// Without any player, commands fall back to media key presses.
fn run_media(sh: &Shared, commands: mpsc::Receiver<String>) {
    let mut mpris = media::Mpris::new();
    let mut tracking = false;
    while !sh.stopping() {
        let active = sh.config().nexus.mode == NexusMode::Media;
        let wait = if active { Duration::from_millis(500) } else { Duration::from_secs(1) };
        let cmd = match commands.recv_timeout(wait) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if !active && cmd.is_none() {
            if tracking {
                sh.telemetry(|t| t.now_playing = None);
                tracking = false;
            }
            continue;
        }
        mpris.refresh();
        if let Some(cmd) = cmd {
            let res = if mpris.has_player() || cmd == "switch_player" {
                mpris.command(&cmd)
            } else {
                actions::key_code(&cmd)
                    .ok_or_else(|| anyhow!("no media player is running"))
                    .and_then(|k| actions::press_combo(&[k]))
            };
            if let Err(e) = res {
                log::warn!("media '{cmd}': {e:#}");
            }
        }
        if active {
            let np = mpris.now_playing();
            sh.telemetry(|t| t.now_playing = np);
            tracking = true;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Game stats (MangoHud logs)

/// Follows the running game's MangoHud log while the Nexus shows game stats, and deletes logs of
/// games that have closed.
fn run_game_stats(sh: &Shared) {
    let mut tracker = LogTracker::new();
    let mut tracking = false;
    let mut last_clean: Option<Instant> = None;
    while !sh.stopping() {
        if last_clean.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            gamestats::clean_old_logs();
            last_clean = Some(Instant::now());
        }
        if sh.config().nexus.mode == NexusMode::Gaming {
            let stats = tracker.poll();
            sh.telemetry(|t| t.game = stats);
            tracking = true;
            nap(sh, Duration::from_millis(500));
        } else {
            if tracking {
                sh.telemetry(|t| t.game = None);
                tracker = LogTracker::new();
                tracking = false;
            }
            nap(sh, Duration::from_secs(1));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// IPC

fn handle_client(sh: Arc<Shared>, stream: UnixStream) {
    let Ok(write_half) = stream.try_clone() else { return };
    let mut writer = std::io::BufWriter::new(write_half);
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::Error(format!("bad request: {e}")),
            Ok(Request::Telemetry) => {
                let mut t = sh.telemetry(|t| t.clone());
                t.config_rev = sh.rev();
                Response::Telemetry(Box::new(t))
            }
            Ok(Request::GetConfig) => Response::Config(Box::new(sh.config())),
            Ok(Request::SetConfig(cfg)) => {
                sh.set_config(*cfg);
                Response::Ok
            }
            Ok(Request::SetProfile(name)) => match perform(&sh, &Action::Profile(name)) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e.to_string()),
            },
            Ok(Request::RunAction(action)) => match perform(&sh, &action) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(format!("{e:#}")),
            },
            Ok(Request::Rescan) => {
                sh.rescan.store(true, Ordering::Relaxed);
                Response::Ok
            }
        };
        let Ok(mut out) = serde_json::to_string(&response) else { break };
        out.push('\n');
        if writer.write_all(out.as_bytes()).and_then(|_| writer.flush()).is_err() {
            break;
        }
    }
}

fn start_ipc(sh: Arc<Shared>) -> Result<()> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!("another opencue daemon is already running ({})", path.display());
    }
    let _ = std::fs::remove_file(&path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let sh = sh.clone();
            thread::spawn(move || handle_client(sh, stream));
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Manager

struct Worker {
    handle: JoinHandle<()>,
    done: Arc<AtomicBool>,
    started: Instant,
}

/// Failing device: when to try again, current backoff, last error (logged once).
struct Retry {
    at: Instant,
    backoff: Duration,
    error: String,
}

fn spawn_worker(sh: &Arc<Shared>, f: Found, has_lcd: bool) -> Result<Worker> {
    let done = Arc::new(AtomicBool::new(false));
    let key = device_key(&f);
    let handle = {
        let (sh, done) = (sh.clone(), done.clone());
        thread::Builder::new().name(key.clone()).spawn(move || {
            // A panic is treated like any other error, so the device is reopened instead of
            // freezing until the daemon restarts.
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match f.kind {
                Kind::CommanderCore => run_commander_core(&sh, &f, has_lcd),
                Kind::LcdCap => run_lcd(&sh, &f),
                Kind::Nexus => run_nexus(&sh, &f),
                Kind::Mm700 => run_mm700(&sh, &f),
            }))
            .unwrap_or_else(|panic| {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                Err(anyhow!("crashed: {msg}"))
            });
            match res {
                Ok(()) => sh.remove_device(&key),
                Err(e) => sh.device_error(&f, Some(format!("{e:#}"))),
            }
            done.store(true, Ordering::Relaxed);
        })?
    };
    Ok(Worker { handle, done, started: Instant::now() })
}

pub fn run() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGHUP, stop.clone())?;

    let (media_tx, media_rx) = mpsc::channel();
    let sh = Arc::new(Shared {
        config: RwLock::new(Config::load()),
        rev: AtomicU64::new(0),
        telemetry: Mutex::new(Telemetry::default()),
        stop,
        rescan: AtomicBool::new(false),
        nexus_page: AtomicUsize::new(0),
        media: media_tx,
        epoch: Instant::now(),
    });
    start_ipc(sh.clone())?;
    log::info!("OpenCUE daemon {} started; socket {}", env!("CARGO_PKG_VERSION"), socket_path().display());

    let sensors = {
        let sh = sh.clone();
        thread::spawn(move || {
            let mut reader = SensorReader::new();
            while !sh.stopping() {
                let s = reader.read();
                sh.telemetry(|t| t.system = s);
                nap(&sh, Duration::from_secs(1));
            }
        })
    };

    let media = {
        let sh = sh.clone();
        thread::Builder::new().name("media".into()).spawn(move || run_media(&sh, media_rx))?
    };

    let game_stats = {
        let sh = sh.clone();
        thread::Builder::new().name("game-stats".into()).spawn(move || run_game_stats(&sh))?
    };

    let mut workers: HashMap<String, Worker> = HashMap::new();
    let mut retry: HashMap<String, Retry> = HashMap::new();
    while !sh.stopping() {
        let now = Instant::now();

        // Reap workers that exited; back off the ones that failed.
        let finished: Vec<String> =
            workers.iter().filter(|(_, w)| w.done.load(Ordering::Relaxed)).map(|(k, _)| k.clone()).collect();
        for key in finished {
            let _ = workers.remove(&key).unwrap().handle.join();
            let err = sh.telemetry(|t| t.devices.iter().find(|d| d.key == key).and_then(|d| d.error.clone()));
            if let Some(err) = err {
                let r = retry.entry(key.clone()).or_insert(Retry {
                    at: now,
                    backoff: Duration::from_millis(500),
                    error: String::new(),
                });
                r.backoff = (r.backoff * 2).min(Duration::from_secs(30));
                r.at = now + r.backoff;
                if r.error != err {
                    log::error!("{key}: {err}");
                    r.error = err;
                }
            }
        }
        // A worker that has stayed up for a while has recovered.
        retry.retain(|key, _| {
            let healthy = workers.get(key).is_some_and(|w| w.started.elapsed() > Duration::from_secs(15));
            if healthy {
                log::info!("{key}: recovered");
            }
            !healthy
        });

        let found = discover();
        let has_lcd = found.iter().any(|f| f.kind == Kind::LcdCap);
        let present: Vec<String> = found.iter().map(device_key).collect();
        for f in found {
            let key = device_key(&f);
            if workers.contains_key(&key) || retry.get(&key).is_some_and(|r| now < r.at) {
                continue;
            }
            workers.insert(key, spawn_worker(&sh, f, has_lcd)?);
        }
        // Forget devices that were unplugged.
        retry.retain(|k, _| present.contains(k));
        sh.telemetry(|t| t.devices.retain(|d| present.contains(&d.key)));

        for _ in 0..20 {
            if sh.stopping() || sh.rescan.swap(false, Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    log::info!("shutting down; handing devices back to hardware mode");
    for (_, w) in workers {
        let _ = w.handle.join();
    }
    let _ = sensors.join();
    let _ = media.join();
    let _ = game_stats.join();
    let _ = std::fs::remove_file(socket_path());
    Ok(())
}
