//! Render every LCD and Nexus screen to PNG files: `cargo run --example preview -- <out_dir>`.

use opencue_core::config::{LcdConfig, LcdMode, NexusConfig, NexusMode};
use opencue_core::render::{lcd, nexus};
use opencue_core::telemetry::{ChannelState, Telemetry};

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out).unwrap();
    let mut t = Telemetry { liquid_temp: Some(31.4), ..Default::default() };
    t.system.cpu_temp = Some(54.2);
    t.system.gpu_temp = Some(47.0);
    t.system.cpu_load = Some(12.0);
    t.system.gpu_load = Some(3.0);
    t.channels.insert("pump".into(), ChannelState { name: "Pump".into(), rpm: Some(2280), duty: Some(75) });
    for mode in LcdMode::ALL {
        let cfg = LcdConfig { mode, ..Default::default() };
        lcd::render(&cfg, &t, false).save(out.join(format!("lcd_{mode:?}.png"))).unwrap();
    }
    for (i, mode) in [NexusMode::Buttons, NexusMode::Sysinfo, NexusMode::Clock].into_iter().enumerate() {
        let cfg = NexusConfig { mode, ..Default::default() };
        nexus::render(&cfg, 0, Some(1), &t, false).save(out.join(format!("nexus_{i}_{mode:?}.png"))).unwrap();
    }
    println!("wrote previews to {}", out.display());
}
