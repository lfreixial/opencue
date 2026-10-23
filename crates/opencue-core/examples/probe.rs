//! Hardware probe: `cargo run --example probe -- cc|lcd|nexus|mm700`. Reads status and restores hardware mode.

use std::time::Duration;

use opencue_core::devices::{Kind, commander_core::CommanderCore, discover};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let what = std::env::args().nth(1).unwrap_or_else(|| "cc".into());
    let found = discover();
    let has_lcd = found.iter().any(|f| f.kind == Kind::LcdCap);
    match what.as_str() {
        "cc" => {
            let f = found.iter().find(|f| f.kind == Kind::CommanderCore).expect("no Commander Core");
            let cc = CommanderCore::open(&f.info, has_lcd).expect("open");
            println!("model {}  firmware {}", cc.model, cc.firmware);
            println!("fan channels {:?}", cc.fan_channels);
            println!("led counts {:?}", cc.led_counts);
            for _ in 0..3 {
                let st = cc.status().expect("status");
                println!("liquid {:?}  rpm {:?}  probes {:?}", st.liquid_temp, st.rpm, st.probes);
                std::thread::sleep(Duration::from_millis(700));
            }
            cc.restore_hardware_mode();
            println!("restored hardware mode");
        }
        other => eprintln!("unknown probe {other}"),
    }
}
