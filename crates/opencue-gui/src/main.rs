//! OpenCUE control app. Talks to the daemon over IPC; all previews use the daemon's own renderers.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use image::RgbaImage;
use opencue_core::config::{
    Action, ChannelCooling, Config, Effect, FanMode, GameMetric, LcdMode, Lighting, NexusButton, NexusMode, NexusPage,
    Preset, Sensor, format_color, media_dir, parse_color,
};
use opencue_core::cooling::curve;
use opencue_core::gamestats::{self, MangoHudStatus};
use opencue_core::ipc::{Client, Request, Response};
use opencue_core::lighting::{self, hsv};
use opencue_core::render::{self, fmt_temp, lcd::Frame};
use opencue_core::telemetry::Telemetry;
use slint::{
    Color, ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel,
};

mod screenshot;

slint::include_modules!();

const CHANNELS: [&str; 7] = ["pump", "fan1", "fan2", "fan3", "fan4", "fan5", "fan6"];
const ACTIONS: [&str; 9] = [
    "Do nothing",
    "Key combination",
    "Media key",
    "Run command",
    "Open website",
    "Type text",
    "Switch profile",
    "Change Nexus page",
    "Cycle LCD screen",
];
const MEDIA: [(&str, &str); 10] = [
    ("play_pause", "Play / Pause"),
    ("next", "Next track"),
    ("previous", "Previous track"),
    ("forward", "Forward 10 s"),
    ("rewind", "Rewind 10 s"),
    ("stop", "Stop"),
    ("switch_player", "Switch media player"),
    ("volume_up", "Volume up"),
    ("volume_down", "Volume down"),
    ("mute", "Mute"),
];

struct LcdAnim {
    key: String,
    frames: Vec<Frame>,
    index: usize,
}

struct App {
    cfg: Config,
    tel: Telemetry,
    connected: bool,
    seen_rev: Option<u64>,
    last_edit: Instant,
    epoch: Instant,
    /// Width / height of the curve editor's graph area.
    curve_aspect: f32,
    lcd_anim: Option<LcdAnim>,
    lcd_loading: Option<String>,
    curve_points: Rc<VecModel<CurvePoint>>,
    leds: Rc<VecModel<Color>>,
    tx: mpsc::Sender<Request>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> T {
    APP.with(|a| f(a.borrow_mut().as_mut().expect("app initialised")))
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn to_color(hex: &str) -> Color {
    let [r, g, b] = parse_color(hex);
    Color::from_rgb_u8(r, g, b)
}

fn from_color(c: Color) -> String {
    format_color([c.red(), c.green(), c.blue()])
}

fn strings(items: impl IntoIterator<Item = impl AsRef<str>>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.into_iter().map(ss).collect::<Vec<_>>()))
}

fn to_image(img: &RgbaImage) -> slint::Image {
    slint::Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

fn sensor_index(s: Sensor) -> i32 {
    match s {
        Sensor::Liquid => 0,
        Sensor::Cpu => 1,
        Sensor::Gpu => 2,
    }
}

fn sensor_from(i: i32) -> Sensor {
    match i {
        1 => Sensor::Cpu,
        2 => Sensor::Gpu,
        _ => Sensor::Liquid,
    }
}

fn temp_range(s: Sensor) -> (f32, f32) {
    match s {
        Sensor::Liquid => (20.0, 60.0),
        _ => (20.0, 100.0),
    }
}

fn color_count(e: Effect) -> i32 {
    match e {
        Effect::Off | Effect::Rainbow | Effect::RainbowWave => 0,
        Effect::Static => 1,
        _ => 2,
    }
}

/// Replace a model's rows in place (keeps elements alive, so a drag in progress isn't interrupted).
fn update_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

// ------------------------------------------------------------------------------- daemon link

/// Sends requests to the daemon on a background thread, coalescing bursts of config updates
/// (e.g. while dragging a slider) so only the latest one is sent.
fn spawn_sender() -> mpsc::Sender<Request> {
    let (tx, rx) = mpsc::channel::<Request>();
    thread::spawn(move || {
        let mut client: Option<Client> = None;
        while let Ok(mut req) = rx.recv() {
            while let Ok(next) = rx.try_recv() {
                if matches!(req, Request::SetConfig(_)) && matches!(next, Request::SetConfig(_)) {
                    req = next;
                } else {
                    send(&mut client, &req);
                    req = next;
                }
            }
            send(&mut client, &req);
        }
    });
    tx
}

fn send(client: &mut Option<Client>, req: &Request) {
    for _ in 0..2 {
        if client.is_none() {
            *client = Client::connect().ok();
        }
        let Some(c) = client.as_mut() else { return };
        match c.request(req) {
            Ok(Response::Error(e)) => {
                log::warn!("daemon: {e}");
                return;
            }
            Ok(_) => return,
            Err(_) => *client = None,
        }
    }
}

/// Polls telemetry once a second (and the config when the daemon reports it changed).
fn spawn_poller(ui: slint::Weak<AppWindow>) {
    thread::spawn(move || {
        let mut client: Option<Client> = None;
        let mut last_rev: Option<u64> = None;
        loop {
            if client.is_none() {
                client = Client::connect().ok();
                last_rev = None;
            }
            let mut tel = None;
            let mut cfg = None;
            if let Some(c) = client.as_mut() {
                match c.request(&Request::Telemetry) {
                    Ok(Response::Telemetry(t)) => {
                        if last_rev != Some(t.config_rev)
                            && let Ok(Response::Config(c2)) = c.request(&Request::GetConfig)
                        {
                            cfg = Some(*c2);
                            last_rev = Some(t.config_rev);
                        }
                        tel = Some(*t);
                    }
                    _ => client = None,
                }
            }
            let connected = tel.is_some();
            let _ = ui.upgrade_in_event_loop(move |ui| on_poll(&ui, connected, tel, cfg));
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn on_poll(ui: &AppWindow, connected: bool, tel: Option<Telemetry>, cfg: Option<Config>) {
    let reload = with_app(|app| {
        let was = app.connected;
        app.connected = connected;
        if let Some(t) = tel {
            app.tel = t;
        }
        // Adopt the daemon's config unless the user is mid-edit (our own pending change wins).
        match cfg {
            Some(c) if app.last_edit.elapsed() > Duration::from_millis(1500) || !was => {
                let changed = c != app.cfg;
                app.cfg = c;
                app.seen_rev = Some(app.tel.config_rev);
                changed
            }
            _ => false,
        }
    });
    ui.set_connected(connected);
    if reload {
        sync_all(ui);
    }
    sync_dashboard(ui);
    sync_cooling_live(ui);
}

/// Store a changed config: send it to the daemon, or save it directly if the daemon is down.
fn commit(app: &mut App) {
    app.last_edit = Instant::now();
    app.cfg = std::mem::take(&mut app.cfg).normalize();
    if app.connected {
        let _ = app.tx.send(Request::SetConfig(Box::new(app.cfg.clone())));
    } else if let Err(e) = app.cfg.save() {
        log::error!("cannot save config: {e}");
    }
}

// ------------------------------------------------------------------------------- sync to UI

fn sync_all(ui: &AppWindow) {
    sync_profiles(ui);
    sync_dashboard(ui);
    sync_cooling(ui);
    sync_lighting(ui);
    sync_lcd(ui);
    sync_nexus(ui);
    sync_settings(ui);
}

fn sync_profiles(ui: &AppWindow) {
    with_app(|app| {
        let names: Vec<&String> = app.cfg.profiles.keys().collect();
        let idx = names.iter().position(|n| **n == app.cfg.active_profile).unwrap_or(0);
        ui.set_profiles(strings(names));
        ui.set_profile_index(idx as i32);
    });
}

fn sync_dashboard(ui: &AppWindow) {
    with_app(|app| {
        let t = &app.tel;
        let f = app.cfg.settings.fahrenheit;
        ui.set_liquid_text(ss(fmt_temp(t.liquid_temp, f)));
        ui.set_cpu_text(ss(fmt_temp(t.system.cpu_temp, f)));
        ui.set_gpu_text(ss(fmt_temp(t.system.gpu_temp, f)));
        let load = |l: Option<f32>, name: &str| match l {
            Some(l) => format!("{l:.0}% · {name}"),
            None => name.to_string(),
        };
        ui.set_cpu_sub(ss(load(t.system.cpu_load, &t.system.cpu_name)));
        ui.set_gpu_sub(ss(load(t.system.gpu_load, &t.system.gpu_name)));
        ui.set_pump_text(ss(t.pump_rpm().map(|r| format!("Pump {r} RPM")).unwrap_or_else(|| "Pump —".into())));
        ui.set_emergency(t.emergency);
        let channels: Vec<ChannelInfo> = CHANNELS
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let st = t.channels.get(*id);
                ChannelInfo {
                    id: ss(id),
                    name: ss(if i == 0 { "Pump".to_string() } else { format!("Fan {i}") }),
                    rpm: ss(st.and_then(|s| s.rpm).map_or("—".into(), |r| r.to_string())),
                    duty: st.and_then(|s| s.duty).unwrap_or(0) as i32,
                    connected: st.is_some(),
                }
            })
            .collect();
        ui.set_channels(ModelRc::new(VecModel::from(channels)));
        let devices: Vec<DeviceRow> = t
            .devices
            .iter()
            .map(|d| DeviceRow {
                name: ss(&d.name),
                detail: ss(match d.firmware.as_str() {
                    "?" | "-" | "" => String::new(),
                    fw => format!("firmware {fw}"),
                }),
                status: ss(d.error.clone().unwrap_or_default()),
                ok: d.error.is_none(),
            })
            .collect();
        ui.set_devices(ModelRc::new(VecModel::from(devices)));
        let n = t.devices.iter().filter(|d| d.error.is_none()).count();
        ui.set_status_text(ss(if app.connected {
            format!("{n} device(s) active")
        } else {
            "systemctl --user start opencue".into()
        }));
        ui.set_lcd_present(t.devices.iter().any(|d| d.kind == opencue_core::devices::Kind::LcdCap));
        ui.set_nexus_present(t.devices.iter().any(|d| d.kind == opencue_core::devices::Kind::Nexus));

        // Lighting zones follow what the daemon discovered.
        let mut zones = vec![Zone { id: ss("*"), name: ss("All devices") }];
        zones.extend(t.zones.iter().map(|z| Zone { id: ss(&z.id), name: ss(&z.name) }));
        let current: Vec<Zone> = ui.get_zones().iter().collect();
        if current != zones {
            ui.set_zones(ModelRc::new(VecModel::from(zones)));
        }
    });
}

fn selected_channel(ui: &AppWindow) -> &'static str {
    CHANNELS[(ui.get_selected_channel().max(0) as usize).min(CHANNELS.len() - 1)]
}

fn sync_cooling(ui: &AppWindow) {
    let ch = selected_channel(ui);
    with_app(|app| {
        let c = app.cfg.profile().cooling_for(ch);
        let pump = ch == "pump";
        ui.set_is_pump(pump);
        ui.set_channel_title(ss(if pump { "PUMP".into() } else { format!("FAN {}", &ch[3..]) }));
        ui.set_fan_mode(match (c.mode, c.preset) {
            (FanMode::Fixed, _) => 4,
            (FanMode::Custom, _) => 5,
            (_, Preset::Quiet) => 0,
            (_, Preset::Balanced) => 1,
            (_, Preset::Performance) => 2,
            (_, Preset::Max) => 3,
        });
        ui.set_fan_sensor(sensor_index(c.sensor));
        ui.set_fixed_duty(c.fixed as f32);
        let (lo, hi) = temp_range(c.sensor);
        ui.set_t_min(lo);
        ui.set_t_max(hi);
        let pts = display_points(&c, pump, lo, hi);
        // Path coordinates: x spans 0..1000*aspect, y spans 0..1000 (top = 100%).
        let w = 1000.0 * app.curve_aspect;
        let y = |d: f32| 1000.0 - d * 10.0;
        let mut line = format!("M 0 {:.1}", y(pts[0].1));
        for (t, d) in &pts {
            line += &format!(" L {:.1} {:.1}", (t - lo) / (hi - lo) * w, y(*d));
        }
        line += &format!(" L {w:.1} {:.1}", y(pts.last().unwrap().1));
        ui.set_curve_fill(ss(format!("{line} L {w:.1} 1000 L 0 1000 Z")));
        ui.set_curve_path(ss(line));
        let handles: Vec<CurvePoint> = if matches!(c.mode, FanMode::Fixed) {
            Vec::new()
        } else {
            pts.iter().map(|&(t, d)| CurvePoint { t, d }).collect()
        };
        update_model(&app.curve_points, handles);
    });
    sync_cooling_live(ui);
}

/// The curve as drawn: clamped into the visible temperature range.
fn display_points(c: &ChannelCooling, pump: bool, lo: f32, hi: f32) -> Vec<(f32, f32)> {
    let mut pts: Vec<(f32, f32)> =
        curve(c, pump).into_iter().map(|(t, d)| (t.clamp(lo, hi), d.clamp(0.0, 100.0))).collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    pts.dedup_by(|a, b| a.0 == b.0 && a.0 == lo);
    if pts.is_empty() {
        pts.push((lo, 100.0));
    }
    pts
}

fn sync_cooling_live(ui: &AppWindow) {
    let ch = selected_channel(ui);
    with_app(|app| {
        let c = app.cfg.profile().cooling_for(ch);
        let temp = match c.sensor {
            Sensor::Liquid => app.tel.liquid_temp,
            Sensor::Cpu => app.tel.system.cpu_temp,
            Sensor::Gpu => app.tel.system.gpu_temp,
        };
        ui.set_current_temp(if matches!(c.mode, FanMode::Fixed) { -100.0 } else { temp.unwrap_or(-100.0) });
    });
}

fn zone_id(ui: &AppWindow) -> String {
    ui.get_zones()
        .row_data(ui.get_selected_zone().max(0) as usize)
        .map(|z| z.id.to_string())
        .unwrap_or_else(|| "*".into())
}

fn sync_lighting(ui: &AppWindow) {
    let zone = zone_id(ui);
    with_app(|app| {
        let p = app.cfg.profile();
        let l = p.lighting_for(&zone);
        ui.set_zone_custom(p.lighting.contains_key(&zone));
        ui.set_effect_names(strings(Effect::ALL.iter().map(|e| e.label())));
        ui.set_effect_index(Effect::ALL.iter().position(|e| *e == l.effect).unwrap_or(0) as i32);
        ui.set_color1(to_color(l.colors.first().map_or("#ffffff", |s| s)));
        ui.set_color2(to_color(l.colors.get(1).map_or("#000000", |s| s)));
        ui.set_color_count(color_count(l.effect));
        ui.set_uses_sensor(l.effect == Effect::Temperature);
        ui.set_speed(l.speed as f32);
        ui.set_brightness(l.brightness as f32);
        ui.set_reverse(l.reverse);
        ui.set_light_sensor(sensor_index(l.sensor));
    });
}

fn sync_lcd(ui: &AppWindow) {
    with_app(|app| {
        let l = &app.cfg.profile().lcd;
        ui.set_lcd_modes(strings(LcdMode::ALL.iter().map(|m| m.label())));
        ui.set_lcd_mode(LcdMode::ALL.iter().position(|m| *m == l.mode).unwrap_or(0) as i32);
        ui.set_lcd_image_name(ss(std::path::Path::new(&l.image).file_name().and_then(|n| n.to_str()).unwrap_or("")));
        ui.set_lcd_brightness(l.brightness as f32);
        ui.set_lcd_rotation(match l.rotation {
            90 => 1,
            180 => 2,
            270 => 3,
            _ => 0,
        });
        ui.set_lcd_cover(l.cover);
        ui.set_lcd_accent(to_color(&l.accent));
        ui.set_lcd_text(ss(&l.text));
    });
    refresh_lcd_preview(ui);
}

fn action_index(a: &Action) -> i32 {
    match a {
        Action::None => 0,
        Action::Keys(_) => 1,
        Action::Media(_) => 2,
        Action::Command(_) => 3,
        Action::Url(_) => 4,
        Action::Text(_) => 5,
        Action::Profile(_) => 6,
        Action::Page(_) => 7,
        Action::CycleLcd => 8,
    }
}

fn action_value(a: &Action) -> String {
    match a {
        Action::Keys(v)
        | Action::Media(v)
        | Action::Command(v)
        | Action::Url(v)
        | Action::Text(v)
        | Action::Profile(v)
        | Action::Page(v) => v.clone(),
        Action::None | Action::CycleLcd => String::new(),
    }
}

fn action_hint(i: i32) -> &'static str {
    match i {
        1 => "e.g. ctrl+shift+t, super+e, f5",
        3 => "e.g. spotify, or any shell command",
        4 => "https://…",
        5 => "Text to type",
        6 => "Profile name",
        7 => "next, prev, or a page number",
        _ => "",
    }
}

fn sync_game_stats(ui: &AppWindow, chosen: &[GameMetric]) {
    let (a, b) = GameMetric::ALL.split_at(GameMetric::ALL.len().div_ceil(2));
    let on =
        |half: &[GameMetric]| ModelRc::new(VecModel::from(half.iter().map(|m| chosen.contains(m)).collect::<Vec<_>>()));
    ui.set_game_metrics_a(strings(a.iter().map(|m| m.description())));
    ui.set_game_metrics_b(strings(b.iter().map(|m| m.description())));
    ui.set_game_metrics_a_on(on(a));
    ui.set_game_metrics_b_on(on(b));
    ui.set_game_metrics_count(ss(format!("{} OF {}", chosen.len(), GameMetric::MAX_SHOWN)));
    let status = gamestats::mangohud_status();
    ui.set_mangohud_status(ss(match status {
        MangoHudStatus::Ready => "MangoHud is set up: games started with MangoHud send their FPS to the Nexus.".into(),
        MangoHudStatus::NotSetUp => format!(
            "MangoHud is installed but not set up. This adds 3 lines to {} so it logs frame times for OpenCUE.",
            gamestats::mangohud_config().display()
        ),
        MangoHudStatus::NotInstalled => "MangoHud is not installed (sudo pacman -S mangohud lib32-mangohud).".into(),
    }));
    ui.set_mangohud_can_setup(status == MangoHudStatus::NotSetUp);
}

fn sync_nexus(ui: &AppWindow) {
    with_app(|app| {
        let nx = &app.cfg.nexus;
        ui.set_nexus_mode(match nx.mode {
            NexusMode::Buttons => 0,
            NexusMode::Sysinfo => 1,
            NexusMode::Clock => 2,
            NexusMode::Media => 3,
            NexusMode::Gaming => 4,
        });
        sync_game_stats(ui, &nx.game_metrics);
        ui.set_nexus_brightness(nx.brightness as f32);
        ui.set_nexus_pages(strings(
            nx.pages.iter().map(|p| if p.name.is_empty() { "Page".to_string() } else { p.name.clone() }),
        ));
        let page = (ui.get_nexus_page().max(0) as usize).min(nx.pages.len() - 1);
        ui.set_nexus_page(page as i32);
        let buttons = &nx.pages[page].buttons;
        ui.set_nexus_buttons(strings(
            buttons.iter().map(|b| if b.label.is_empty() { "(no label)".to_string() } else { b.label.clone() }),
        ));
        let bi = (ui.get_nexus_button().max(0) as usize).min(buttons.len().saturating_sub(1));
        ui.set_nexus_button(bi as i32);
        let b = buttons.get(bi).cloned().unwrap_or_default();
        ui.set_btn_label(ss(&b.label));
        ui.set_btn_color(to_color(&b.color));
        ui.set_btn_text_color(to_color(&b.text_color));
        ui.set_action_names(strings(ACTIONS));
        let ai = action_index(&b.action);
        ui.set_btn_action(ai);
        ui.set_btn_value(ss(action_value(&b.action)));
        ui.set_btn_value_hint(ss(action_hint(ai)));
        ui.set_btn_value_is_media(ai == 2);
        ui.set_btn_value_is_keys(ai == 1);
        ui.set_media_names(strings(MEDIA.iter().map(|m| m.1)));
        ui.set_btn_media_index(MEDIA.iter().position(|m| m.0 == action_value(&b.action)).unwrap_or(0) as i32);
    });
    refresh_nexus_preview(ui);
}

fn sync_settings(ui: &AppWindow) {
    with_app(|app| {
        let s = &app.cfg.settings;
        ui.set_fahrenheit(s.fahrenheit);
        ui.set_restore_hw(s.restore_hardware_on_exit);
        ui.set_min_fan(s.min_fan_duty as f32);
        ui.set_crit_liquid(s.critical_liquid_temp);
        ui.set_crit_cpu(s.critical_cpu_temp);
    });
}

// ------------------------------------------------------------------------------- previews

fn refresh_nexus_preview(ui: &AppWindow) {
    with_app(|app| {
        let page = (ui.get_nexus_page().max(0) as usize).min(app.cfg.nexus.pages.len() - 1);
        let sel = (app.cfg.nexus.mode == NexusMode::Buttons).then(|| ui.get_nexus_button().max(0) as usize);
        let img = render::nexus::render(&app.cfg.nexus, page, sel, &app.tel, app.cfg.settings.fahrenheit);
        ui.set_nexus_image(to_image(&img));
    });
}

fn refresh_lcd_preview(ui: &AppWindow) {
    let weak = ui.as_weak();
    with_app(|app| {
        let lcd = app.cfg.profile().lcd.clone();
        if lcd.mode == LcdMode::Image && !lcd.image.is_empty() {
            let key = format!("{}|{}|{}", lcd.image, lcd.cover, lcd.background);
            if let Some(anim) = app.lcd_anim.as_mut().filter(|a| a.key == key) {
                if !anim.frames.is_empty() {
                    let f = &anim.frames[anim.index % anim.frames.len()];
                    ui.set_lcd_image(to_image(&f.image));
                }
                return;
            }
            if app.lcd_loading.as_deref() != Some(&key) {
                app.lcd_loading = Some(key.clone());
                thread::spawn(move || {
                    let frames = render::lcd::load_media(lcd.image.as_ref(), lcd.cover, parse_color(&lcd.background))
                        .unwrap_or_else(|e| {
                            log::error!("cannot load {}: {e:#}", lcd.image);
                            Vec::new()
                        });
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        with_app(|app| {
                            app.lcd_loading = None;
                            app.lcd_anim = Some(LcdAnim { key, frames, index: 0 });
                        });
                        refresh_lcd_preview(&ui);
                    });
                });
            }
            return;
        }
        app.lcd_anim = None;
        let img = render::lcd::render(&lcd, &app.tel, app.cfg.settings.fahrenheit);
        ui.set_lcd_image(to_image(&img));
    });
}

/// Drives the GIF preview at the GIF's own frame rate.
fn schedule_gif(ui: slint::Weak<AppWindow>) {
    let delay = with_app(|app| {
        let anim = app.lcd_anim.as_mut()?;
        if anim.frames.len() < 2 {
            return None;
        }
        anim.index = (anim.index + 1) % anim.frames.len();
        Some(anim.frames[anim.index].delay)
    });
    if let Some(ui) = ui.upgrade()
        && ui.get_page() == 3
        && delay.is_some()
    {
        refresh_lcd_preview(&ui);
    }
    let next = delay.unwrap_or(Duration::from_millis(250));
    Timer::single_shot(next, move || schedule_gif(ui));
}

// ------------------------------------------------------------------------------- editing helpers

fn edit(ui: &AppWindow, f: impl FnOnce(&mut Config), resync: fn(&AppWindow)) {
    with_app(|app| {
        f(&mut app.cfg);
        commit(app);
    });
    resync(ui);
}

fn edit_channel(ui: &AppWindow, f: impl FnOnce(&mut ChannelCooling, bool)) {
    let ch = selected_channel(ui);
    edit(
        ui,
        |cfg| {
            let c = cfg.profile_mut().cooling.entry(ch.to_string()).or_default();
            f(c, ch == "pump");
        },
        sync_cooling,
    );
}

fn edit_button(ui: &AppWindow, f: impl FnOnce(&mut NexusButton)) {
    let page = ui.get_nexus_page().max(0) as usize;
    let bi = ui.get_nexus_button().max(0) as usize;
    edit(
        ui,
        |cfg| {
            if let Some(b) = cfg.nexus.pages.get_mut(page).and_then(|p| p.buttons.get_mut(bi)) {
                f(b);
            }
        },
        sync_nexus,
    );
}

/// Read the lighting controls back into a `Lighting`.
fn lighting_from_ui(ui: &AppWindow) -> Lighting {
    let effect = Effect::ALL[(ui.get_effect_index().max(0) as usize).min(Effect::ALL.len() - 1)];
    Lighting {
        effect,
        colors: vec![from_color(ui.get_color1()), from_color(ui.get_color2())],
        speed: ui.get_speed().round().clamp(1.0, 100.0) as u8,
        brightness: ui.get_brightness().round().clamp(0.0, 100.0) as u8,
        sensor: sensor_from(ui.get_light_sensor()),
        reverse: ui.get_reverse(),
    }
}

fn key_name(text: &str) -> Option<String> {
    use slint::platform::Key;
    let specials = [
        (Key::Return, "enter"),
        (Key::Escape, "esc"),
        (Key::Tab, "tab"),
        (Key::Backspace, "backspace"),
        (Key::Delete, "delete"),
        (Key::Insert, "insert"),
        (Key::Home, "home"),
        (Key::End, "end"),
        (Key::PageUp, "pageup"),
        (Key::PageDown, "pagedown"),
        (Key::UpArrow, "up"),
        (Key::DownArrow, "down"),
        (Key::LeftArrow, "left"),
        (Key::RightArrow, "right"),
        (Key::Space, "space"),
        (Key::SysReq, "print"),
        (Key::Pause, "pause"),
        (Key::Menu, "compose"),
        (Key::F1, "f1"),
        (Key::F2, "f2"),
        (Key::F3, "f3"),
        (Key::F4, "f4"),
        (Key::F5, "f5"),
        (Key::F6, "f6"),
        (Key::F7, "f7"),
        (Key::F8, "f8"),
        (Key::F9, "f9"),
        (Key::F10, "f10"),
        (Key::F11, "f11"),
        (Key::F12, "f12"),
    ];
    let c = text.chars().next()?;
    if let Some((_, name)) = specials.iter().find(|(k, _)| char::from(*k) == c) {
        return Some(name.to_string());
    }
    if c == ' ' {
        return Some("space".into());
    }
    // Shifted symbols are recorded as their unshifted key (shift is already a modifier).
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    const PLAIN: &str = "`1234567890-=[]\\;',./";
    if let Some(i) = SHIFTED.find(c) {
        return Some(PLAIN[i..i + 1].to_string());
    }
    (c.is_ascii_graphic()).then(|| c.to_ascii_lowercase().to_string())
}

fn daemon_exe() -> std::path::PathBuf {
    std::env::current_exe().map(|p| p.with_file_name("opencue")).unwrap_or_else(|_| "opencue".into())
}

fn start_daemon() {
    let ok = std::process::Command::new("systemctl")
        .args(["--user", "start", "opencue.service"])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        // Service not installed: run the daemon directly.
        let _ = std::process::Command::new(daemon_exe())
            .arg("daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

fn autostart_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "opencue.service"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
}

// ------------------------------------------------------------------------------- main

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // Matches opencue.desktop, so the compositor shows the right window icon.
    let args: Vec<String> = std::env::args().collect();
    let arg = |flag: &str, n: usize| args.iter().position(|a| a == flag).and_then(|i| args.get(i + n)).cloned();
    // `--screenshot <page> <file.png>` renders off-screen and exits.
    let screenshot =
        arg("--screenshot", 1).map(|page| (page, arg("--screenshot", 2).unwrap_or_else(|| "opencue.png".into())));
    let offscreen = match screenshot {
        Some(_) => Some(screenshot::install()?),
        None => {
            slint::BackendSelector::new().select()?;
            None
        }
    };
    if let Err(e) = slint::set_xdg_app_id("opencue") {
        log::debug!("cannot set app id: {e}");
    }
    let ui = AppWindow::new()?;
    let curve_points = Rc::new(VecModel::<CurvePoint>::default());
    let leds = Rc::new(VecModel::<Color>::default());
    ui.set_curve_points(ModelRc::from(curve_points.clone()));
    ui.set_led_preview(ModelRc::from(leds.clone()));
    ui.set_version(ss(env!("CARGO_PKG_VERSION")));
    ui.set_autostart(autostart_enabled());

    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            cfg: Config::load(),
            tel: Telemetry::default(),
            connected: false,
            seen_rev: None,
            last_edit: Instant::now() - Duration::from_secs(60),
            epoch: Instant::now(),
            curve_aspect: 2.0,
            lcd_anim: None,
            lcd_loading: None,
            curve_points,
            leds,
            tx: spawn_sender(),
        })
    });

    // -- colour helpers used inside the .slint file
    let util = ui.global::<Util>();
    util.on_hue_color(|h| {
        let [r, g, b] = hsv(h, 1.0, 1.0);
        Color::from_rgb_u8(r, g, b)
    });
    util.on_parse_hex(|text, fallback| {
        let t = text.trim().trim_start_matches('#');
        if t.len() == 6 && u32::from_str_radix(t, 16).is_ok() { to_color(t) } else { fallback }
    });
    util.on_to_hex(|c| ss(from_color(c)));

    // -- profiles
    let w = ui.as_weak();
    ui.on_profile_selected(move |i| {
        let ui = w.unwrap();
        edit(
            &ui,
            |cfg| {
                if let Some(name) = cfg.profiles.keys().nth(i as usize).cloned() {
                    cfg.active_profile = name;
                }
            },
            sync_all,
        );
    });
    let w = ui.as_weak();
    ui.on_profile_new(move || {
        let ui = w.unwrap();
        edit(
            &ui,
            |cfg| {
                let mut n = cfg.profiles.len() + 1;
                while cfg.profiles.contains_key(&format!("Profile {n}")) {
                    n += 1;
                }
                let name = format!("Profile {n}");
                cfg.profiles.insert(name.clone(), cfg.profile().clone());
                cfg.active_profile = name;
            },
            sync_all,
        );
    });
    let w = ui.as_weak();
    ui.on_profile_delete(move || {
        let ui = w.unwrap();
        edit(
            &ui,
            |cfg| {
                if cfg.profiles.len() > 1 {
                    let name = cfg.active_profile.clone();
                    cfg.profiles.remove(&name);
                    cfg.active_profile = cfg.profiles.keys().next().unwrap().clone();
                }
            },
            sync_all,
        );
    });
    let w = ui.as_weak();
    ui.on_profile_rename(move |name| {
        let ui = w.unwrap();
        let name = name.trim().to_string();
        edit(
            &ui,
            |cfg| {
                if name.is_empty() || cfg.profiles.contains_key(&name) {
                    return;
                }
                let old = cfg.active_profile.clone();
                let p = cfg.profiles.remove(&old).unwrap();
                cfg.profiles.insert(name.clone(), p);
                cfg.active_profile = name;
            },
            sync_all,
        );
    });
    ui.on_start_daemon(start_daemon);

    // -- cooling
    let w = ui.as_weak();
    ui.on_channel_selected(move |_| sync_cooling(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_fan_mode_changed(move |i| {
        let ui = w.unwrap();
        edit_channel(&ui, |c, pump| match i {
            4 => c.mode = FanMode::Fixed,
            5 => {
                // Start the custom curve from whatever the channel is doing now.
                let (lo, hi) = temp_range(c.sensor);
                let current = display_points(c, pump, lo, hi);
                if c.mode != FanMode::Custom && current.len() >= 2 {
                    c.points = current;
                }
                c.mode = FanMode::Custom;
            }
            _ => {
                c.mode = FanMode::Preset;
                c.preset = [Preset::Quiet, Preset::Balanced, Preset::Performance, Preset::Max][i.clamp(0, 3) as usize];
            }
        });
    });
    let w = ui.as_weak();
    ui.on_fan_sensor_changed(move |i| {
        let ui = w.unwrap();
        edit_channel(&ui, |c, _| {
            let new = sensor_from(i);
            if new != c.sensor && c.mode == FanMode::Custom {
                // Re-map the curve onto the new sensor's range so it stays sensible.
                let (lo0, hi0) = temp_range(c.sensor);
                let (lo1, hi1) = temp_range(new);
                for p in &mut c.points {
                    p.0 = lo1 + (p.0 - lo0) / (hi0 - lo0) * (hi1 - lo1);
                }
            }
            c.sensor = new;
        });
    });
    let w = ui.as_weak();
    ui.on_fixed_duty_changed(move |v| {
        let ui = w.unwrap();
        edit_channel(&ui, |c, pump| c.fixed = (v.round() as u8).clamp(if pump { 50 } else { 0 }, 100));
    });
    let w = ui.as_weak();
    ui.on_point_moved(move |i, t, d| {
        let ui = w.unwrap();
        let (lo, hi) = (ui.get_t_min(), ui.get_t_max());
        edit_channel(&ui, |c, _| {
            let i = i as usize;
            c.points.sort_by(|a, b| a.0.total_cmp(&b.0));
            if i >= c.points.len() {
                return;
            }
            let min_t = if i == 0 { lo } else { c.points[i - 1].0 + 1.0 };
            let max_t = if i + 1 == c.points.len() { hi } else { c.points[i + 1].0 - 1.0 };
            c.points[i] = (((t * 2.0).round() / 2.0).clamp(min_t, max_t.max(min_t)), d.round().clamp(0.0, 100.0));
        });
    });
    let w = ui.as_weak();
    ui.on_curve_resized(move |aspect| {
        if aspect.is_finite() && aspect > 0.1 {
            with_app(|app| app.curve_aspect = aspect);
            sync_cooling(&w.unwrap());
        }
    });
    let w = ui.as_weak();
    ui.on_point_add(move || {
        let ui = w.unwrap();
        edit_channel(&ui, |c, _| {
            c.points.sort_by(|a, b| a.0.total_cmp(&b.0));
            if c.points.len() >= 8 {
                return;
            }
            let gap =
                c.points.windows(2).enumerate().max_by(|a, b| (a.1[1].0 - a.1[0].0).total_cmp(&(b.1[1].0 - b.1[0].0)));
            if let Some((i, w)) = gap {
                c.points.insert(i + 1, ((w[0].0 + w[1].0) / 2.0, (w[0].1 + w[1].1) / 2.0));
            }
        });
    });
    let w = ui.as_weak();
    ui.on_point_remove(move || {
        let ui = w.unwrap();
        edit_channel(&ui, |c, _| {
            if c.points.len() > 2 {
                c.points.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mid = c.points.len() / 2;
                c.points.remove(mid);
            }
        });
    });
    let w = ui.as_weak();
    ui.on_apply_to_all_fans(move || {
        let ui = w.unwrap();
        let ch = selected_channel(&ui);
        edit(
            &ui,
            |cfg| {
                let p = cfg.profile_mut();
                let src = p.cooling_for(ch);
                for fan in &CHANNELS[1..] {
                    p.cooling.insert(fan.to_string(), src.clone());
                }
            },
            sync_cooling,
        );
    });

    // -- lighting
    let w = ui.as_weak();
    ui.on_zone_selected(move |_| sync_lighting(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_zone_custom_toggled(move |on| {
        let ui = w.unwrap();
        let zone = zone_id(&ui);
        edit(
            &ui,
            |cfg| {
                let p = cfg.profile_mut();
                if on {
                    let all = p.lighting_for("*");
                    p.lighting.insert(zone, all);
                } else {
                    p.lighting.remove(&zone);
                }
            },
            sync_lighting,
        );
    });
    let w = ui.as_weak();
    ui.on_lighting_changed(move || {
        let ui = w.unwrap();
        let zone = zone_id(&ui);
        let l = lighting_from_ui(&ui);
        edit(
            &ui,
            |cfg| {
                cfg.profile_mut().lighting.insert(zone, l);
            },
            sync_lighting,
        );
    });

    // -- lcd
    let w = ui.as_weak();
    ui.on_lcd_changed(move || {
        let ui = w.unwrap();
        let mode = LcdMode::ALL[(ui.get_lcd_mode().max(0) as usize).min(LcdMode::ALL.len() - 1)];
        let (brightness, rotation, cover) = (ui.get_lcd_brightness(), ui.get_lcd_rotation(), ui.get_lcd_cover());
        let (accent, text) = (from_color(ui.get_lcd_accent()), ui.get_lcd_text().to_string());
        edit(
            &ui,
            |cfg| {
                let l = &mut cfg.profile_mut().lcd;
                l.mode = mode;
                l.brightness = brightness.round().clamp(0.0, 100.0) as u8;
                l.rotation = [0, 90, 180, 270][rotation.clamp(0, 3) as usize];
                l.cover = cover;
                l.accent = accent;
                l.text = text;
            },
            sync_lcd,
        );
    });
    let w = ui.as_weak();
    ui.on_lcd_pick_image(move || {
        let ui = w.unwrap();
        let Some(src) = rfd::FileDialog::new()
            .set_title("Choose an image or GIF for the pump LCD")
            .add_filter("Images", &["gif", "png", "jpg", "jpeg", "webp", "bmp"])
            .pick_file()
        else {
            return;
        };
        // Keep a private copy so the screen keeps working if the original moves.
        let dir = media_dir();
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join(src.file_name().unwrap_or_default());
        let path = match std::fs::copy(&src, &dest) {
            Ok(_) => dest,
            Err(e) => {
                log::warn!("cannot copy {} into {}: {e}; using it in place", src.display(), dir.display());
                src
            }
        };
        edit(
            &ui,
            |cfg| {
                let l = &mut cfg.profile_mut().lcd;
                l.image = path.display().to_string();
                l.mode = LcdMode::Image;
            },
            sync_lcd,
        );
    });

    // -- nexus
    let w = ui.as_weak();
    ui.on_nexus_changed(move || {
        let ui = w.unwrap();
        let modes = [NexusMode::Buttons, NexusMode::Sysinfo, NexusMode::Clock, NexusMode::Media, NexusMode::Gaming];
        let mode = modes[ui.get_nexus_mode().clamp(0, modes.len() as i32 - 1) as usize];
        let brightness = ui.get_nexus_brightness().round().clamp(0.0, 100.0) as u8;
        edit(
            &ui,
            |cfg| {
                cfg.nexus.mode = mode;
                cfg.nexus.brightness = brightness;
            },
            sync_nexus,
        );
    });
    let w = ui.as_weak();
    ui.on_game_metric_toggled(move |i, on| {
        let ui = w.unwrap();
        let Some(&metric) = GameMetric::ALL.get(i.max(0) as usize) else { return };
        edit(
            &ui,
            |cfg| {
                let chosen = &mut cfg.nexus.game_metrics;
                if !on {
                    chosen.retain(|m| *m != metric);
                } else if !chosen.contains(&metric) && chosen.len() < GameMetric::MAX_SHOWN {
                    chosen.push(metric);
                    // Cells follow the list order in the app.
                    chosen.sort_by_key(|m| GameMetric::ALL.iter().position(|a| a == m));
                }
            },
            // Re-sync either way, so a 7th tick is undone.
            sync_nexus,
        );
    });
    let w = ui.as_weak();
    ui.on_mangohud_setup(move || {
        let ui = w.unwrap();
        match gamestats::setup_mangohud() {
            Ok(path) => log::info!("added OpenCUE's logging block to {}", path.display()),
            Err(e) => log::error!("cannot set up MangoHud: {e}"),
        }
        sync_nexus(&ui);
    });
    let w = ui.as_weak();
    ui.on_nexus_page_selected(move |_| {
        let ui = w.unwrap();
        ui.set_nexus_button(0);
        sync_nexus(&ui);
    });
    let w = ui.as_weak();
    ui.on_nexus_page_add(move || {
        let ui = w.unwrap();
        let n = with_app(|a| a.cfg.nexus.pages.len());
        edit(
            &ui,
            |cfg| {
                cfg.nexus.pages.push(NexusPage {
                    name: format!("Page {}", n + 1),
                    buttons: vec![
                        NexusButton::default(),
                        NexusButton {
                            label: "›".into(),
                            text_color: "#00c8ff".into(),
                            color: "#2a2a3a".into(),
                            action: Action::Page("next".into()),
                        },
                    ],
                });
            },
            |_| {},
        );
        ui.set_nexus_page(n as i32);
        ui.set_nexus_button(0);
        sync_nexus(&ui);
    });
    let w = ui.as_weak();
    ui.on_nexus_page_remove(move || {
        let ui = w.unwrap();
        let page = ui.get_nexus_page().max(0) as usize;
        edit(
            &ui,
            |cfg| {
                if cfg.nexus.pages.len() > 1 && page < cfg.nexus.pages.len() {
                    cfg.nexus.pages.remove(page);
                }
            },
            sync_nexus,
        );
    });
    let w = ui.as_weak();
    ui.on_nexus_page_rename(move |name| {
        let ui = w.unwrap();
        let page = ui.get_nexus_page().max(0) as usize;
        edit(
            &ui,
            |cfg| {
                if let Some(p) = cfg.nexus.pages.get_mut(page) {
                    p.name = name.trim().to_string();
                }
            },
            sync_nexus,
        );
    });
    let w = ui.as_weak();
    ui.on_nexus_button_selected(move |_| sync_nexus(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_nexus_button_add(move || {
        let ui = w.unwrap();
        let page = ui.get_nexus_page().max(0) as usize;
        let mut new_index = 0;
        edit(
            &ui,
            |cfg| {
                if let Some(p) = cfg.nexus.pages.get_mut(page) {
                    // Insert before a trailing "next page" button so navigation stays at the end.
                    let at = if p.buttons.last().is_some_and(|b| matches!(b.action, Action::Page(_))) {
                        p.buttons.len() - 1
                    } else {
                        p.buttons.len()
                    };
                    p.buttons.insert(at, NexusButton::default());
                    new_index = at;
                }
            },
            |_| {},
        );
        ui.set_nexus_button(new_index as i32);
        sync_nexus(&ui);
    });
    let w = ui.as_weak();
    ui.on_nexus_button_remove(move || {
        let ui = w.unwrap();
        let (page, bi) = (ui.get_nexus_page().max(0) as usize, ui.get_nexus_button().max(0) as usize);
        edit(
            &ui,
            |cfg| {
                if let Some(p) = cfg.nexus.pages.get_mut(page)
                    && p.buttons.len() > 1
                    && bi < p.buttons.len()
                {
                    p.buttons.remove(bi);
                }
            },
            sync_nexus,
        );
    });
    let w = ui.as_weak();
    ui.on_nexus_button_move(move |dir| {
        let ui = w.unwrap();
        let (page, bi) = (ui.get_nexus_page().max(0) as usize, ui.get_nexus_button().max(0) as usize);
        let target = bi as i32 + dir;
        let mut moved = false;
        edit(
            &ui,
            |cfg| {
                if let Some(p) = cfg.nexus.pages.get_mut(page)
                    && target >= 0
                    && (target as usize) < p.buttons.len()
                    && bi < p.buttons.len()
                {
                    p.buttons.swap(bi, target as usize);
                    moved = true;
                }
            },
            |_| {},
        );
        if moved {
            ui.set_nexus_button(target);
        }
        sync_nexus(&ui);
    });
    let w = ui.as_weak();
    ui.on_button_changed(move || {
        let ui = w.unwrap();
        let (label, color, text_color) =
            (ui.get_btn_label().to_string(), from_color(ui.get_btn_color()), from_color(ui.get_btn_text_color()));
        let ai = ui.get_btn_action();
        let value = if ai == 2 {
            MEDIA[(ui.get_btn_media_index().max(0) as usize).min(MEDIA.len() - 1)].0.to_string()
        } else {
            ui.get_btn_value().to_string()
        };
        edit_button(&ui, |b| {
            b.label = label;
            b.color = color;
            b.text_color = text_color;
            b.action = make_action(ai, value);
        });
    });
    let w = ui.as_weak();
    ui.on_btn_action_changed(move |ai| {
        let ui = w.unwrap();
        let default = match ai {
            2 => "play_pause".to_string(),
            7 => "next".to_string(),
            6 => with_app(|a| a.cfg.active_profile.clone()),
            _ => String::new(),
        };
        edit_button(&ui, |b| b.action = make_action(ai, default));
    });
    let w = ui.as_weak();
    ui.on_nexus_clicked(move |frac| {
        let ui = w.unwrap();
        let x = (frac.clamp(0.0, 1.0) * 639.0) as u16;
        if ui.get_nexus_mode() == 3 {
            // Same as touching the strip: control the player.
            let cmd = render::nexus::media_control_at(x).map_or("switch_player", |i| render::nexus::MEDIA_CONTROLS[i]);
            with_app(|app| {
                let _ = app.tx.send(Request::RunAction(Action::Media(cmd.into())));
            });
            return;
        }
        if ui.get_nexus_mode() != 0 {
            return;
        }
        let n = ui.get_nexus_buttons().row_count();
        if let Some(i) = render::nexus::button_at(x, n) {
            ui.set_nexus_button(i as i32);
            sync_nexus(&ui);
        }
    });
    let w = ui.as_weak();
    ui.on_key_recorded(move |text, ctrl, alt, shift, meta| {
        let ui = w.unwrap();
        let Some(key) = key_name(&text) else { return };
        let mut parts = Vec::new();
        if ctrl {
            parts.push("ctrl".to_string());
        }
        if alt {
            parts.push("alt".to_string());
        }
        if shift {
            parts.push("shift".to_string());
        }
        if meta {
            parts.push("super".to_string());
        }
        parts.push(key);
        let combo = parts.join("+");
        edit_button(&ui, |b| b.action = Action::Keys(combo));
    });
    let w = ui.as_weak();
    ui.on_btn_test(move || {
        let ui = w.unwrap();
        let (page, bi) = (ui.get_nexus_page().max(0) as usize, ui.get_nexus_button().max(0) as usize);
        with_app(|app| {
            if let Some(b) = app.cfg.nexus.pages.get(page).and_then(|p| p.buttons.get(bi)) {
                let _ = app.tx.send(Request::RunAction(b.action.clone()));
            }
        });
    });

    // -- settings
    let w = ui.as_weak();
    ui.on_settings_changed(move || {
        let ui = w.unwrap();
        let (f, r, m, l, c) =
            (ui.get_fahrenheit(), ui.get_restore_hw(), ui.get_min_fan(), ui.get_crit_liquid(), ui.get_crit_cpu());
        edit(
            &ui,
            |cfg| {
                let s = &mut cfg.settings;
                s.fahrenheit = f;
                s.restore_hardware_on_exit = r;
                s.min_fan_duty = m.round().clamp(0.0, 100.0) as u8;
                s.critical_liquid_temp = l.round();
                s.critical_cpu_temp = c.round();
            },
            sync_settings,
        );
    });
    ui.on_autostart_toggled(|on| {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", if on { "enable" } else { "disable" }, "opencue.service"])
            .status();
    });

    // -- timers: LED preview at 30 fps, screen previews once a second, GIF at its own pace
    let w = ui.as_weak();
    let led_timer = Timer::default();
    led_timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
        let Some(ui) = w.upgrade() else { return };
        if ui.get_page() != 2 {
            return;
        }
        let zone = zone_id(&ui);
        with_app(|app| {
            let p = app.cfg.profile();
            let l = p.lighting_for(&zone);
            let n = app.tel.zones.iter().find(|z| z.id == zone).map_or(24, |z| z.leds).clamp(1, 34);
            let temp = match l.sensor {
                Sensor::Liquid => app.tel.liquid_temp,
                Sensor::Cpu => app.tel.system.cpu_temp,
                Sensor::Gpu => app.tel.system.gpu_temp,
            };
            let frame = lighting::render(&l, app.epoch.elapsed().as_secs_f64(), n, 0.0, temp);
            update_model(&app.leds, frame.into_iter().map(|[r, g, b]| Color::from_rgb_u8(r, g, b)).collect());
        });
    });
    let w = ui.as_weak();
    let screen_timer = Timer::default();
    screen_timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(ui) = w.upgrade() else { return };
        match ui.get_page() {
            3 if with_app(|a| a.lcd_anim.is_none()) => refresh_lcd_preview(&ui),
            4 => refresh_nexus_preview(&ui),
            _ => {}
        }
    });
    schedule_gif(ui.as_weak());

    // `opencue-gui --page lighting` opens on a given page.
    let pages = ["dashboard", "cooling", "lighting", "lcd", "nexus", "settings"];
    let wanted = screenshot.as_ref().map(|s| s.0.clone()).or_else(|| arg("--page", 1));
    if let Some(i) = wanted.and_then(|p| pages.iter().position(|n| *n == p)) {
        ui.set_page(i as i32);
    }

    sync_all(&ui);
    if let (Some((_, out)), Some(window)) = (screenshot, offscreen) {
        // Fill in live data synchronously, since there's no event loop off-screen.
        if let Ok(mut c) = Client::connect() {
            let tel = match c.request(&Request::Telemetry) {
                Ok(Response::Telemetry(t)) => Some(*t),
                _ => None,
            };
            let cfg = match c.request(&Request::GetConfig) {
                Ok(Response::Config(c)) => Some(*c),
                _ => None,
            };
            on_poll(&ui, tel.is_some(), tel, cfg);
            sync_all(&ui);
        }
        return screenshot::capture(&ui, &window, &out);
    }

    spawn_poller(ui.as_weak());
    ui.run()?;
    Ok(())
}

fn make_action(index: i32, value: String) -> Action {
    match index {
        1 => Action::Keys(value),
        2 => Action::Media(value),
        3 => Action::Command(value),
        4 => Action::Url(value),
        5 => Action::Text(value),
        6 => Action::Profile(value),
        7 => Action::Page(value),
        8 => Action::CycleLcd,
        _ => Action::None,
    }
}
