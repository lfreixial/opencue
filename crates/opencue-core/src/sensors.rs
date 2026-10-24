//! System sensors from hwmon and /proc: CPU/GPU temperature and load.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemSensors {
    pub cpu_temp: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub cpu_load: Option<f32>,
    pub gpu_load: Option<f32>,
    pub cpu_name: String,
    pub gpu_name: String,
}

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
        let gpu_busy = gpu_hwmon.map(|h| h.join("device/gpu_busy_percent")).filter(|p| p.exists());
        let cpu_name = fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
            })
            .unwrap_or_else(|| "CPU".into());
        let gpu_name = gpu_name().unwrap_or_else(|| "GPU".into());
        Self { cpu_temp, gpu_temp, gpu_busy, last_cpu: None, cpu_name, gpu_name }
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
