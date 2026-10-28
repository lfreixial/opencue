//! System sensors from hwmon and /proc: CPU/GPU temperature and load.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemSensors {
    pub cpu_temp: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub cpu_load: Option<f32>,
    pub gpu_load: Option<f32>,
    pub cpu_name: String,
    pub gpu_name: String,
    /// Junction (hotspot) temperature, where the GPU reports one.
    pub gpu_hotspot: Option<f32>,
    pub gpu_mem_temp: Option<f32>,
    /// Watts.
    pub gpu_power: Option<f32>,
    /// MHz.
    pub gpu_clock: Option<f32>,
    /// GiB.
    pub vram_used: Option<f32>,
    pub vram_total: Option<f32>,
}

const GIB: f32 = 1024.0 * 1024.0 * 1024.0;

fn read_f32(p: &Path) -> Option<f32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn hwmon_by_name(names: &[&str]) -> Option<PathBuf> {
    let mut found: Vec<(usize, PathBuf)> = fs::read_dir("/sys/class/hwmon")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = fs::read_to_string(e.path().join("name")).ok()?;
            names.iter().position(|n| *n == name.trim()).map(|rank| (rank, e.path()))
        })
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().next().map(|(_, p)| p)
}

/// The `<kind>N_input` file labelled exactly `label`, if any.
fn labelled_input(hwmon: &Path, kind: &str, label: &str) -> Option<PathBuf> {
    (1..16).find_map(|i| {
        let l = fs::read_to_string(hwmon.join(format!("{kind}{i}_label"))).ok()?;
        (l.trim() == label).then(|| hwmon.join(format!("{kind}{i}_input")))
    })
}

/// Find the temp input whose label matches one of `labels`, else temp1.
fn temp_input(hwmon: &Path, labels: &[&str]) -> PathBuf {
    for label in labels {
        for i in 1..16 {
            let l = fs::read_to_string(hwmon.join(format!("temp{i}_label"))).unwrap_or_default();
            if l.trim() == *label {
                return hwmon.join(format!("temp{i}_input"));
            }
        }
    }
    hwmon.join("temp1_input")
}

pub struct SensorReader {
    cpu_temp: Option<PathBuf>,
    gpu_temp: Option<PathBuf>,
    gpu_busy: Option<PathBuf>,
    gpu_hotspot: Option<PathBuf>,
    gpu_mem_temp: Option<PathBuf>,
    gpu_power: Option<PathBuf>,
    gpu_clock: Option<PathBuf>,
    vram_used: Option<PathBuf>,
    vram_total: Option<PathBuf>,
    last_cpu: Option<(u64, u64)>,
    cpu_name: String,
    gpu_name: String,
}

impl Default for SensorReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorReader {
    pub fn new() -> Self {
        let cpu_hwmon = hwmon_by_name(&["k10temp", "zenpower", "coretemp"]);
        let cpu_temp = cpu_hwmon.map(|h| temp_input(&h, &["Tctl", "Tdie", "Package id 0"]));
        let gpu_hwmon = hwmon_by_name(&["amdgpu", "nouveau", "i915", "xe"]);
        let gpu_temp = gpu_hwmon.as_ref().map(|h| temp_input(h, &["edge", "junction"]));
        let gpu_busy = gpu_hwmon.as_ref().map(|h| h.join("device/gpu_busy_percent")).filter(|p| p.exists());
        let exists = |p: PathBuf| p.exists().then_some(p);
        let gpu_hotspot = gpu_hwmon.as_ref().and_then(|h| labelled_input(h, "temp", "junction"));
        let gpu_mem_temp = gpu_hwmon.as_ref().and_then(|h| labelled_input(h, "temp", "mem"));
        let gpu_power = gpu_hwmon
            .as_ref()
            .and_then(|h| exists(h.join("power1_average")).or_else(|| labelled_input(h, "power", "PPT")));
        let gpu_clock = gpu_hwmon.as_ref().and_then(|h| labelled_input(h, "freq", "sclk"));
        let vram_used = gpu_hwmon.as_ref().and_then(|h| exists(h.join("device/mem_info_vram_used")));
        let vram_total = gpu_hwmon.as_ref().and_then(|h| exists(h.join("device/mem_info_vram_total")));
        let cpu_name = fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
            })
            .unwrap_or_else(|| "CPU".into());
        let gpu_name = gpu_name().unwrap_or_else(|| "GPU".into());
        Self {
            cpu_temp,
            gpu_temp,
            gpu_busy,
            gpu_hotspot,
            gpu_mem_temp,
            gpu_power,
            gpu_clock,
            vram_used,
            vram_total,
            last_cpu: None,
            cpu_name,
            gpu_name,
        }
    }

    fn cpu_load(&mut self) -> Option<f32> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let vals: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
        let idle = vals.get(3)? + vals.get(4).unwrap_or(&0);
        let total: u64 = vals.iter().take(8).sum();
        let prev = self.last_cpu.replace((idle, total));
        let (pi, pt) = prev?;
        let dt = total.saturating_sub(pt);
        (dt > 0).then(|| 100.0 * (1.0 - idle.saturating_sub(pi) as f32 / dt as f32))
    }

    pub fn read(&mut self) -> SystemSensors {
        SystemSensors {
            cpu_temp: self.cpu_temp.as_deref().and_then(read_f32).map(|v| v / 1000.0),
            gpu_temp: self.gpu_temp.as_deref().and_then(read_f32).map(|v| v / 1000.0),
            cpu_load: self.cpu_load(),
            gpu_load: self.gpu_busy.as_deref().and_then(read_f32),
            cpu_name: self.cpu_name.clone(),
            gpu_name: self.gpu_name.clone(),
            gpu_hotspot: self.gpu_hotspot.as_deref().and_then(read_f32).map(|v| v / 1000.0),
            gpu_mem_temp: self.gpu_mem_temp.as_deref().and_then(read_f32).map(|v| v / 1000.0),
            gpu_power: self.gpu_power.as_deref().and_then(read_f32).map(|v| v / 1_000_000.0),
            gpu_clock: self.gpu_clock.as_deref().and_then(read_f32).map(|v| v / 1_000_000.0),
            vram_used: self.vram_used.as_deref().and_then(read_f32).map(|v| v / GIB),
            vram_total: self.vram_total.as_deref().and_then(read_f32).map(|v| v / GIB),
        }
    }
}

fn gpu_name() -> Option<String> {
    let out = std::process::Command::new("lspci").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("VGA compatible controller") || l.contains("3D controller"))?;
    let name = line.split(": ").nth(1)?;
    // "Advanced Micro Devices, Inc. [AMD/ATI] Navi 48 [Radeon RX 9070/9070 XT/9070 GRE] (rev c0)"
    let bracketed: Vec<&str> = name.split('[').filter_map(|s| s.split(']').next()).collect();
    Some(bracketed.last().map(|s| s.to_string()).unwrap_or_else(|| name.to_string()))
}
