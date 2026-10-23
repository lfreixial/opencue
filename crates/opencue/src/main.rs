//! `opencue` - daemon and command-line interface.

mod actions;
mod daemon;

use anyhow::{Context, Result, bail};
use opencue_core::devices::discover;
use opencue_core::hidraw::{HidDevice, enumerate};
use opencue_core::ipc::{Client, Request, Response};
use opencue_core::render::fmt_temp;

const HELP: &str = "\
opencue - open-source Corsair iCUE replacement

USAGE:
    opencue daemon            run the background service (normally started by systemd)
    opencue gui               open the control app
    opencue status            show temperatures, fan speeds and devices
    opencue devices           list Corsair USB devices and whether they are accessible
    opencue profile [NAME]    list profiles, or switch to NAME
    opencue rescan            look for newly connected devices now
";

fn client() -> Result<Client> {
    Client::connect().context("the opencue daemon is not running (start it with: systemctl --user start opencue)")
}

fn status() -> Result<()> {
    let mut c = client()?;
    let Response::Telemetry(t) = c.request(&Request::Telemetry)? else { bail!("unexpected reply") };
    let Response::Config(cfg) = c.request(&Request::GetConfig)? else { bail!("unexpected reply") };
    let f = cfg.settings.fahrenheit;
    println!("Profile:  {}", cfg.active_profile);
    println!("Liquid:   {}", fmt_temp(t.liquid_temp, f));
    println!(
        "CPU:      {}  {}",
        fmt_temp(t.system.cpu_temp, f),
        t.system.cpu_load.map(|l| format!("{l:.0}% load")).unwrap_or_default()
    );
    println!(
        "GPU:      {}  {}",
        fmt_temp(t.system.gpu_temp, f),
        t.system.gpu_load.map(|l| format!("{l:.0}% load")).unwrap_or_default()
    );
    if t.emergency {
        println!("!! critical temperature: all fans forced to 100%");
    }
    for ch in t.channels.values() {
        println!(
            "{:<9} {:>5} RPM  {:>3}%",
            format!("{}:", ch.name),
            ch.rpm.map_or("-".into(), |r| r.to_string()),
            ch.duty.map_or("-".into(), |d| d.to_string())
        );
    }
    println!("\nDevices:");
    for d in &t.devices {
        let state = d.error.as_deref().map_or("ok".to_string(), |e| format!("ERROR: {e}"));
        println!("  {:<24} fw {:<10} {state}", d.name, d.firmware);
    }
    Ok(())
}

fn devices() {
    let supported: Vec<_> = discover().into_iter().map(|f| (f.info.path.clone(), f.kind)).collect();
    for info in enumerate(Some(0x1b1c)) {
        let kind = supported.iter().find(|(p, _)| *p == info.path).map(|(_, k)| format!("{k:?}"));
        let access = match HidDevice::open(&info) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.to_string(),
        };
        println!(
            "{:<14} {:04x}:{:04x} if{} {:<45} {:<14} {}",
            info.path.display(),
            info.vid,
            info.pid,
            info.interface,
            info.name,
            kind.unwrap_or_else(|| "-".into()),
            access
        );
    }
}

fn profile(name: Option<String>) -> Result<()> {
    let mut c = client()?;
    match name {
        Some(n) => match c.request(&Request::SetProfile(n.clone()))? {
            Response::Ok => println!("switched to {n}"),
            Response::Error(e) => bail!(e),
            _ => bail!("unexpected reply"),
        },
        None => {
            let Response::Config(cfg) = c.request(&Request::GetConfig)? else { bail!("unexpected reply") };
            for p in cfg.profiles.keys() {
                println!("{} {p}", if *p == cfg.active_profile { "*" } else { " " });
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_timestamp_secs().init();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("daemon") => daemon::run(),
        Some("status") => status(),
        Some("devices") => {
            devices();
            Ok(())
        }
        Some("profile") => profile(args.next()),
        Some("rescan") => {
            client()?.request(&Request::Rescan)?;
            Ok(())
        }
        Some("gui") => {
            let exe = std::env::current_exe()?.with_file_name("opencue-gui");
            let err = std::os::unix::process::CommandExt::exec(&mut std::process::Command::new(exe));
            Err(err).context("cannot start opencue-gui")
        }
        Some("-h" | "--help" | "help") | None => {
            print!("{HELP}");
            Ok(())
        }
        Some(other) => bail!("unknown command '{other}'\n\n{HELP}"),
    }
}
