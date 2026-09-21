// SPDX-License-Identifier: GPL-3.0-only
//! Metric collection for the COSMIC sysmon applet.
//!
//! Everything is read from `/proc` and `/sys`. No external helper process and
//! no elevated privileges are required, except for the RAPL energy counter
//! which is root-only on Fedora until a udev rule relaxes it.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Number of samples kept: one hour at 1 Hz.
pub const HISTORY_LEN: usize = 3600;

const GIB: f32 = 1024.0 * 1024.0 * 1024.0;

/// One snapshot of every monitored value.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Total CPU utilization in percent.
    pub cpu_load: f32,
    /// Used memory in GiB (total minus available).
    pub mem_used: f32,
    /// Installed memory in GiB.
    pub mem_total: f32,
    /// Used VRAM of the primary GPU in GiB.
    pub vram_used: f32,
    /// Total VRAM of the primary GPU in GiB.
    pub vram_total: f32,
    /// Graphics engine utilization of the primary GPU in percent.
    pub gpu_load: f32,
    /// CPU package power in watts, `0.0` when unavailable.
    pub cpu_watts: f32,
    /// Whether `cpu_watts` holds a measured value.
    pub cpu_watts_available: bool,
    /// Sum of all AMD GPU board powers in watts.
    pub gpu_watts: f32,
}

impl Sample {
    pub fn mem_pct(&self) -> f32 {
        percent(self.mem_used, self.mem_total)
    }

    pub fn vram_pct(&self) -> f32 {
        percent(self.vram_used, self.vram_total)
    }

    /// Measured sum of CPU package and GPU power.
    pub fn total_watts(&self) -> f32 {
        self.cpu_watts + self.gpu_watts
    }
}

fn percent(used: f32, total: f32) -> f32 {
    if total > 0.0 {
        (used / total * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_f32(path: &Path) -> Option<f32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// One AMD GPU exposed through the amdgpu kernel driver.
#[derive(Debug, Clone)]
struct Amdgpu {
    vram_used: PathBuf,
    vram_total: PathBuf,
    busy: PathBuf,
    power: Option<PathBuf>,
    vram_total_gib: f32,
}

/// Discovers every `cardN` backed by the amdgpu driver.
fn find_amdgpus() -> Vec<Amdgpu> {
    let mut cards: Vec<PathBuf> = match fs::read_dir("/sys/class/drm") {
        Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
        Err(_) => return Vec::new(),
    };
    cards.sort();

    let mut gpus = Vec::new();
    for card in cards {
        let Some(name) = card.file_name().and_then(|name| name.to_str()).map(str::to_owned) else {
            continue;
        };
        let Some(index) = name.strip_prefix("card") else {
            continue;
        };
        if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let device = card.join("device");
        let uevent = fs::read_to_string(device.join("uevent")).unwrap_or_default();
        if !uevent.contains("DRIVER=amdgpu") {
            continue;
        }

        let vram_total = device.join("mem_info_vram_total");
        if !vram_total.exists() {
            continue;
        }

        gpus.push(Amdgpu {
            vram_used: device.join("mem_info_vram_used"),
            vram_total_gib: read_u64(&vram_total).unwrap_or(0) as f32 / GIB,
            vram_total,
            busy: device.join("gpu_busy_percent"),
            power: amdgpu_power_path(&device),
        });
    }

    gpus
}

/// Maps a PCI device to the amdgpu hwmon node that reports its board power.
fn amdgpu_power_path(device: &Path) -> Option<PathBuf> {
    let device = fs::canonicalize(device).ok()?;
    let entries = fs::read_dir("/sys/class/hwmon").ok()?;

    for entry in entries.flatten() {
        let hwmon = entry.path();
        let is_amdgpu = fs::read_to_string(hwmon.join("name"))
            .map(|name| name.trim() == "amdgpu")
            .unwrap_or(false);
        if !is_amdgpu {
            continue;
        }
        if fs::canonicalize(hwmon.join("device")).ok().as_deref() != Some(device.as_path()) {
            continue;
        }
        for candidate in ["power1_average", "power1_input"] {
            let path = hwmon.join(candidate);
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
}

/// Finds the RAPL package domain, e.g. `intel-rapl:0`.
fn find_rapl_package() -> Option<PathBuf> {
    let entries = fs::read_dir("/sys/class/powercap").ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // `intel-rapl` is the container, `intel-rapl:0` a package domain and
        // `intel-rapl:0:0` a nested subdomain.
        if name.matches(':').count() != 1 {
            continue;
        }
        let label = fs::read_to_string(path.join("name")).unwrap_or_default();
        if label.trim().starts_with("package") {
            return Some(path);
        }
    }

    None
}

fn read_cpu_times() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse().ok())
        .collect();
    if values.len() < 5 {
        return None;
    }
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    let total: u64 = values.iter().take(8).sum();
    Some((total, idle))
}

fn read_memory() -> (f32, f32) {
    let text = match fs::read_to_string("/proc/meminfo") {
        Ok(text) => text,
        Err(_) => return (0.0, 0.0),
    };

    let mut total_kib = 0u64;
    let mut available_kib = 0u64;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("MemTotal:") {
            total_kib = parse_first_number(value);
        } else if let Some(value) = line.strip_prefix("MemAvailable:") {
            available_kib = parse_first_number(value);
        }
    }

    let total = total_kib as f32 / (1024.0 * 1024.0);
    let used = total_kib.saturating_sub(available_kib) as f32 / (1024.0 * 1024.0);
    (used, total)
}

fn parse_first_number(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Keeps the previous counter readings and the one-hour history.
pub struct Monitor {
    previous_cpu: Option<(u64, u64)>,
    previous_energy: Option<(u64, Instant)>,
    rapl_package: Option<PathBuf>,
    rapl_range_uj: u64,
    last_cpu_watts: f32,
    gpus: Vec<Amdgpu>,
    primary_gpu: usize,
    /// Samples of the last hour, oldest first.
    pub history: VecDeque<Sample>,
    /// Most recent sample.
    pub last: Sample,
}

impl Monitor {
    pub fn new() -> Self {
        let gpus = find_amdgpus();
        let primary_gpu = gpus
            .iter()
            .enumerate()
            .max_by(|left, right| {
                left.1
                    .vram_total_gib
                    .partial_cmp(&right.1.vram_total_gib)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index)
            .unwrap_or(0);

        let rapl_package = find_rapl_package();
        let rapl_range_uj = rapl_package
            .as_ref()
            .and_then(|path| read_u64(&path.join("max_energy_range_uj")))
            .unwrap_or(0);

        Self {
            previous_cpu: None,
            previous_energy: None,
            rapl_package,
            rapl_range_uj,
            last_cpu_watts: 0.0,
            gpus,
            primary_gpu,
            history: VecDeque::with_capacity(HISTORY_LEN),
            last: Sample::default(),
        }
    }

    /// Reads every value and appends the result to the history.
    pub fn tick(&mut self) -> Sample {
        let sample = self.read();
        self.last = sample;
        if self.history.len() >= HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(sample);
        sample
    }

    fn read(&mut self) -> Sample {
        let cpu_load = self.cpu_load();
        let (mem_used, mem_total) = read_memory();
        let (vram_used, vram_total, gpu_load, gpu_watts) = self.gpu_values();
        let cpu_watts = self.cpu_watts();

        Sample {
            cpu_load,
            mem_used,
            mem_total,
            vram_used,
            vram_total,
            gpu_load,
            cpu_watts: cpu_watts.unwrap_or(0.0),
            cpu_watts_available: cpu_watts.is_some(),
            gpu_watts,
        }
    }

    fn cpu_load(&mut self) -> f32 {
        let Some((total, idle)) = read_cpu_times() else {
            return 0.0;
        };

        let load = match self.previous_cpu {
            Some((previous_total, previous_idle)) => {
                let total_delta = total.saturating_sub(previous_total);
                let idle_delta = idle.saturating_sub(previous_idle);
                if total_delta == 0 {
                    0.0
                } else {
                    ((total_delta - idle_delta) as f32 / total_delta as f32 * 100.0)
                        .clamp(0.0, 100.0)
                }
            }
            None => 0.0,
        };

        self.previous_cpu = Some((total, idle));
        load
    }

    /// CPU package power from the RAPL energy counter.
    ///
    /// Returns `None` while the counter is not readable, and retries the
    /// discovery on every call so a permission fix is picked up immediately.
    fn cpu_watts(&mut self) -> Option<f32> {
        if self.rapl_package.is_none() {
            self.rapl_package = find_rapl_package();
        }
        let energy_path = self.rapl_package.as_ref()?.join("energy_uj");
        let energy = read_u64(&energy_path)?;

        let now = Instant::now();
        let watts = match self.previous_energy {
            Some((previous, previous_at)) => {
                let seconds = now.duration_since(previous_at).as_secs_f32();
                if seconds <= 0.0 {
                    self.last_cpu_watts
                } else {
                    let delta = if energy >= previous {
                        energy - previous
                    } else {
                        // Counter wrapped around.
                        self.rapl_range_uj
                            .saturating_sub(previous)
                            .saturating_add(energy)
                    };
                    delta as f32 / 1_000_000.0 / seconds
                }
            }
            None => 0.0,
        };

        if !watts.is_finite() || watts < 0.0 {
            return Some(self.last_cpu_watts);
        }

        self.previous_energy = Some((energy, now));
        self.last_cpu_watts = watts;
        Some(watts)
    }

    /// VRAM, GPU load and total GPU board power for the primary GPU.
    fn gpu_values(&self) -> (f32, f32, f32, f32) {
        let mut watts = 0.0;
        for gpu in &self.gpus {
            if let Some(path) = &gpu.power {
                if let Some(microwatts) = read_f32(path) {
                    watts += microwatts / 1_000_000.0;
                }
            }
        }

        match self.gpus.get(self.primary_gpu) {
            Some(gpu) => {
                let used = read_u64(&gpu.vram_used).unwrap_or(0) as f32 / GIB;
                let total = read_u64(&gpu.vram_total).unwrap_or(0) as f32 / GIB;
                let load = read_f32(&gpu.busy).unwrap_or(0.0);
                (used, total, load, watts)
            }
            None => (0.0, 0.0, 0.0, watts),
        }
    }
}
