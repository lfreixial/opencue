//! Hardware probe: `cargo run --example probe -- cc|strimer`. Reads status and restores hardware mode.

use std::time::Duration;

use opencue_core::devices::{
    Kind,
    commander_core::CommanderCore,
    discover,
    strimer::{Cable, Strimer},
};

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
        "strimer" => {
            let f = found.iter().find(|f| f.kind == Kind::Strimer).expect("no Strimer controller");
            let s = Strimer::open(&f.info).expect("open");
            let solid = |c: [u8; 3], n: usize| vec![c; n];
            let (atx, gpu) = (Cable::Atx.leds(), Cable::Gpu.leds());
            println!("24-pin red, GPU blue");
            s.write_colors(&solid([255, 0, 0], atx), &solid([0, 0, 255], gpu)).expect("write");
            std::thread::sleep(Duration::from_secs(3));
            println!("24-pin green, GPU white");
            s.write_colors(&solid([0, 255, 0], atx), &solid([255, 255, 255], gpu)).expect("write");
            std::thread::sleep(Duration::from_secs(3));
            println!("one white LED running from LED 0 to the end, twice");
            for _ in 0..2 {
                for i in 0..gpu {
                    let dot = |n: usize| {
                        (0..n).map(|j| if j == i.min(n - 1) { [255; 3] } else { [0; 3] }).collect::<Vec<_>>()
                    };
                    s.write_colors(&dot(atx), &dot(gpu)).expect("write");
                    std::thread::sleep(Duration::from_millis(120));
                }
            }
            println!("timing 120 frames as fast as possible (rainbow)");
            let t = std::time::Instant::now();
            for k in 0..120 {
                let frame = |n: usize| {
                    (0..n)
                        .map(|j| opencue_core::lighting::hsv(j as f32 / n as f32 + k as f32 / 60.0, 1.0, 1.0))
                        .collect::<Vec<_>>()
                };
                s.write_colors(&frame(atx), &frame(gpu)).expect("write");
            }
            println!("{:.1} frames/s", 120.0 / t.elapsed().as_secs_f64());
            s.restore_hardware_mode();
            println!("restored hardware rainbow");
        }
        other => eprintln!("unknown probe {other}"),
    }
}
