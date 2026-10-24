//! Persistent configuration: profiles (cooling, lighting, LCD) plus Nexus pages and settings.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("opencue")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// Where images picked in the GUI are copied, so the config never points at a moved file.
pub fn media_dir() -> PathBuf {
    config_dir().join("media")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Sensor {
    #[default]
    Liquid,
    Cpu,
    Gpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    #[default]
    Preset,
    Fixed,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Quiet,
    #[default]
    Balanced,
    Performance,
    Max,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelCooling {
    pub mode: FanMode,
    pub preset: Preset,
    pub sensor: Sensor,
    pub fixed: u8,
    /// (temperature °C, duty %) for custom curves.
    pub points: Vec<(f32, f32)>,
}

impl Default for ChannelCooling {
    fn default() -> Self {
        Self {
            mode: FanMode::Preset,
            preset: Preset::Balanced,
            sensor: Sensor::Liquid,
            fixed: 50,
            points: vec![(25.0, 30.0), (30.0, 40.0), (35.0, 55.0), (40.0, 75.0), (45.0, 100.0)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Off,
    Static,
    Gradient,
    Breathing,
    ColorShift,
    Rainbow,
    #[default]
    RainbowWave,
    ColorWave,
    Spinner,
    Marquee,
    Twinkle,
    Temperature,
}

impl Effect {
    pub const ALL: [Effect; 12] = [
        Effect::Off,
        Effect::Static,
        Effect::Gradient,
        Effect::Breathing,
        Effect::ColorShift,
        Effect::Rainbow,
        Effect::RainbowWave,
        Effect::ColorWave,
        Effect::Spinner,
        Effect::Marquee,
        Effect::Twinkle,
        Effect::Temperature,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Effect::Off => "Off",
            Effect::Static => "Static",
            Effect::Gradient => "Gradient",
            Effect::Breathing => "Breathing",
            Effect::ColorShift => "Color shift",
            Effect::Rainbow => "Rainbow",
            Effect::RainbowWave => "Rainbow wave",
            Effect::ColorWave => "Color wave",
            Effect::Spinner => "Spinner",
            Effect::Marquee => "Marquee",
            Effect::Twinkle => "Twinkle",
            Effect::Temperature => "Temperature",
        }
    }

    pub fn is_animated(self) -> bool {
        !matches!(self, Effect::Off | Effect::Static | Effect::Gradient)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lighting {
    pub effect: Effect,
    pub colors: Vec<String>,
    /// 1..=100
    pub speed: u8,
    /// 0..=100
    pub brightness: u8,
    pub sensor: Sensor,
    pub reverse: bool,
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            effect: Effect::RainbowWave,
            colors: vec!["#ff0000".into(), "#0000ff".into()],
            speed: 50,
            brightness: 100,
            sensor: Sensor::Liquid,
            reverse: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LcdMode {
    Image,
    #[default]
    Liquid,
    Cpu,
    Gpu,
    Pump,
    Dashboard,
    Clock,
    Text,
}

impl LcdMode {
    pub const ALL: [LcdMode; 8] = [
        LcdMode::Image,
        LcdMode::Liquid,
        LcdMode::Cpu,
        LcdMode::Gpu,
        LcdMode::Pump,
        LcdMode::Dashboard,
        LcdMode::Clock,
        LcdMode::Text,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LcdMode::Image => "Image / GIF",
            LcdMode::Liquid => "Liquid temperature",
            LcdMode::Cpu => "CPU temperature",
            LcdMode::Gpu => "GPU temperature",
            LcdMode::Pump => "Pump speed",
            LcdMode::Dashboard => "Dashboard",
            LcdMode::Clock => "Clock",
            LcdMode::Text => "Custom text",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LcdConfig {
    pub mode: LcdMode,
    pub image: String,
    /// true = crop to fill the circle, false = fit whole image.
    pub cover: bool,
    pub brightness: u8,
    pub rotation: u16,
    pub accent: String,
    pub background: String,
    pub text: String,
}

impl Default for LcdConfig {
    fn default() -> Self {
        Self {
            mode: LcdMode::Liquid,
            image: String::new(),
            cover: true,
            brightness: 100,
            rotation: 0,
            accent: "#00c8ff".into(),
            background: "#000000".into(),
            text: "OpenCUE".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Keys are "pump", "fan1".."fan6".
    pub cooling: BTreeMap<String, ChannelCooling>,
    /// Keys are zone ids ("cc:pump", "cc:fan1", "mm700"); "*" applies to zones without their own entry.
    pub lighting: BTreeMap<String, Lighting>,
    pub lcd: LcdConfig,
}

impl Default for Profile {
    fn default() -> Self {
        let mut cooling = BTreeMap::new();
        cooling.insert("pump".into(), ChannelCooling { fixed: 75, ..Default::default() });
        for i in 1..=6 {
            cooling.insert(format!("fan{i}"), ChannelCooling::default());
        }
        Self { cooling, lighting: BTreeMap::from([("*".into(), Lighting::default())]), lcd: LcdConfig::default() }
    }
}

impl Profile {
    pub fn lighting_for(&self, zone: &str) -> Lighting {
        self.lighting.get(zone).or_else(|| self.lighting.get("*")).cloned().unwrap_or_default()
    }

    pub fn cooling_for(&self, channel: &str) -> ChannelCooling {
        self.cooling.get(channel).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Action {
    None,
    /// Key combination, e.g. "ctrl+alt+t".
    Keys(String),
    /// play_pause | next | previous | stop | volume_up | volume_down | mute
    Media(String),
    /// Shell command, run detached.
    Command(String),
    Url(String),
    /// Type text.
    Text(String),
    /// Switch to the named profile.
    Profile(String),
    /// "next" | "prev" | page number.
    Page(String),
    /// Cycle through LCD modes.
    CycleLcd,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NexusButton {
    pub label: String,
    pub color: String,
    pub text_color: String,
    pub action: Action,
}

impl Default for NexusButton {
    fn default() -> Self {
        Self { label: "Button".into(), color: "#1e1e28".into(), text_color: "#ffffff".into(), action: Action::None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NexusPage {
    pub name: String,
    pub buttons: Vec<NexusButton>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NexusMode {
    #[default]
    Buttons,
    Sysinfo,
    Clock,
    /// Now playing from the active media player, with transport controls.
    Media,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NexusConfig {
    pub mode: NexusMode,
    pub brightness: u8,
    pub pages: Vec<NexusPage>,
}

fn button(label: &str, action: Action) -> NexusButton {
    NexusButton { label: label.into(), action, ..Default::default() }
}

impl Default for NexusConfig {
    fn default() -> Self {
        let next = NexusButton {
            label: "›".into(),
            color: "#2a2a3a".into(),
            text_color: "#00c8ff".into(),
            action: Action::Page("next".into()),
        };
        Self {
            mode: NexusMode::Buttons,
            brightness: 100,
            pages: vec![
                NexusPage {
                    name: "Media".into(),
                    buttons: vec![
                        button("Prev", Action::Media("previous".into())),
                        button("Play", Action::Media("play_pause".into())),
                        button("Next", Action::Media("next".into())),
                        button("Vol -", Action::Media("volume_down".into())),
                        button("Vol +", Action::Media("volume_up".into())),
                        next.clone(),
                    ],
                },
                NexusPage {
                    name: "Apps".into(),
                    buttons: vec![
                        button("Terminal", Action::Command("konsole".into())),
                        button("Files", Action::Command("dolphin".into())),
                        button("Browser", Action::Url("https://github.com".into())),
                        button("Screenshot", Action::Keys("print".into())),
                        next,
                    ],
                },
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub fahrenheit: bool,
    /// Lowest duty for fans (the pump has its own hard floor of 50%).
    pub min_fan_duty: u8,
    /// Above this liquid temperature every fan and the pump go to 100%.
    pub critical_liquid_temp: f32,
    pub critical_cpu_temp: f32,
    /// On exit, hand control back to the devices' built-in (hardware) mode.
    pub restore_hardware_on_exit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fahrenheit: false,
            min_fan_duty: 20,
            critical_liquid_temp: 60.0,
            critical_cpu_temp: 95.0,
            restore_hardware_on_exit: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub active_profile: String,
    pub profiles: BTreeMap<String, Profile>,
    pub nexus: NexusConfig,
    pub settings: Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            active_profile: "Default".into(),
            profiles: BTreeMap::from([("Default".into(), Profile::default())]),
            nexus: NexusConfig::default(),
            settings: Settings::default(),
        }
    }
}

impl Config {
    /// Repair anything that would leave the rest of the program without a valid choice.
    pub fn normalize(mut self) -> Self {
        if self.profiles.is_empty() {
            self.profiles.insert("Default".into(), Profile::default());
        }
        if !self.profiles.contains_key(&self.active_profile) {
            self.active_profile = self.profiles.keys().next().unwrap().clone();
        }
        if self.nexus.pages.is_empty() {
            self.nexus.pages = NexusConfig::default().pages;
        }
        self.settings.min_fan_duty = self.settings.min_fan_duty.min(100);
        self
    }

    pub fn profile(&self) -> &Profile {
        &self.profiles[&self.active_profile]
    }

    pub fn profile_mut(&mut self) -> &mut Profile {
        self.profiles.get_mut(&self.active_profile).expect("normalized config")
    }

    pub fn load() -> Self {
        let path = config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => cfg.normalize(),
                Err(e) => {
                    let backup = path.with_extension("broken.json");
                    let _ = std::fs::rename(&path, &backup);
                    log::error!("config unreadable ({e}); moved to {} and using defaults", backup.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    /// Atomic save (write temp file, then rename).
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_file();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }
}

/// Parse "#rrggbb" (or "rrggbb"). Falls back to white.
pub fn parse_color(s: &str) -> [u8; 3] {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6
        && let Ok(v) = u32::from_str_radix(h, 16)
    {
        return [(v >> 16) as u8, (v >> 8) as u8, v as u8];
    }
    [255, 255, 255]
}

pub fn format_color(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}
