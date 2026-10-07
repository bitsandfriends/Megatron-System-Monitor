// SPDX-License-Identifier: GPL-3.0-only
//! Metric collection for the COSMIC sysmon applet.
//!
//! Everything is read from `/proc` and `/sys`. No external helper process and
//! no elevated privileges are required, except for the RAPL energy counter
//! which is root-only on Fedora until a udev rule relaxes it.
//!
//! The GPU detection follows btop's Linux collector (`src/linux/btop_collect.cpp`,
//! namespace `Gpu::Asysfs`): every `/sys/class/drm/cardN` backed by the amdgpu
//! driver is one GPU, a card counts as soon as *any* metric is readable, board
//! power prefers the averaged `power1_average` node over `power1_input`, and the
//! display name comes from the PCI IDs database (btop falls back to
//! `AMD GPU (vendor:device)` when no name is available).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// Number of samples kept: one hour at 1 Hz.
pub const HISTORY_LEN: usize = 3600;

const GIB: f32 = 1024.0 * 1024.0 * 1024.0;

/// One GPU as it is exposed to the rest of the applet.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuInfo {
    /// PCI address, e.g. `0000:03:00.0`.
    pub slot: String,
    /// Human readable model name, e.g. `AMD Radeon AI PRO R9700`.
    pub name: String,
    /// Short label for narrow rows, e.g. `R9700`.
    pub short: String,
    /// Vendor and device ID, e.g. `1002:7551`; empty when unreadable.
    pub pci_id: String,
    /// PCI device ID in lowercase hex without prefix, e.g. `7551`.
    pub device_id: String,
    /// Utilisation of the graphics engine in percent.
    pub load: f32,
    /// Temperature in degrees Celsius.
    pub temp_c: f32,
    /// Board power in watts (`0.0` while the node reports nothing usable).
    pub watts: f32,
    /// Whether `watts` holds a measured value.
    pub watts_available: bool,
    /// Measured VRAM in GiB.
    pub vram_used: f32,
    /// Installed VRAM in GiB.
    pub vram_total: f32,
}

/// One snapshot of every monitored value.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Total CPU utilization in percent.
    pub cpu_load: f32,
    /// Used memory in GiB (total minus available).
    pub mem_used: f32,
    /// Installed memory in GiB.
    pub mem_total: f32,
    /// Used VRAM of all GPUs in GiB.
    pub vram_used: f32,
    /// Installed VRAM of all GPUs in GiB.
    pub vram_total: f32,
    /// CPU package power in watts, `0.0` when unavailable.
    pub cpu_watts: f32,
    /// Whether `cpu_watts` holds a measured value.
    pub cpu_watts_available: bool,
    /// Sum of all GPU board powers in watts.
    pub gpu_watts: f32,
    /// Every detected GPU, sorted by PCI address.
    pub gpus: Vec<GpuInfo>,
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

    /// Number of detected GPUs.
    pub fn gpu_count(&self) -> usize {
        self.gpus.len()
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

fn read_text(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// One amdgpu card found in `/sys/class/drm`.
#[derive(Debug, Clone)]
struct Amdgpu {
    slot: String,
    pci_id: String,
    device_id: String,
    vendor_id: String,
    vram_used: Option<PathBuf>,
    vram_total: Option<PathBuf>,
    busy: Option<PathBuf>,
    power: Option<PathBuf>,
    temp: Option<PathBuf>,
}

/// Returns the `N` of a plain `cardN` entry, ignoring connectors such as
/// `card1-DP-1` and render nodes such as `renderD128`.
fn card_index(name: &str) -> Option<u32> {
    let index = name.strip_prefix("card")?;
    if index.is_empty() || !index.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

/// Discovers every `cardN` backed by the amdgpu driver.
///
/// A card is kept as soon as one of its metrics is readable; btop uses the same
/// rule so that a card without `mem_info_vram_*` is still shown.
fn find_amdgpus() -> Vec<Amdgpu> {
    let mut cards: Vec<(u32, PathBuf)> = match fs::read_dir("/sys/class/drm") {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let index = card_index(name.to_str()?)?;
                Some((index, entry.path()))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    cards.sort_by_key(|(index, _)| *index);

    let mut gpus = Vec::new();
    for (_, card) in cards {
        let device = card.join("device");
        let uevent = fs::read_to_string(device.join("uevent")).unwrap_or_default();
        if !uevent.contains("DRIVER=amdgpu") {
            continue;
        }

        let vendor_id = read_text(&device.join("vendor"))
            .map(|value| normalize_pci_id(&value))
            .unwrap_or_default();
        let device_id = read_text(&device.join("device"))
            .map(|value| normalize_pci_id(&value))
            .unwrap_or_default();

        let vram_total = existing(&device.join("mem_info_vram_total"));
        let vram_used = existing(&device.join("mem_info_vram_used"));
        let busy = existing(&device.join("gpu_busy_percent"));

        let hwmon = amdgpu_hwmon(&device);
        let power = hwmon.as_ref().and_then(|path| amdgpu_power_path(path));
        let temp = hwmon
            .as_ref()
            .map(|path| path.join("temp1_input"))
            .filter(|path| path.exists());

        // btop skips cards that expose nothing at all, so the applet does not
        // render an empty row for a virtual or freshly bound device.
        if vram_total.is_none() && busy.is_none() && power.is_none() && temp.is_none() {
            continue;
        }

        gpus.push(Amdgpu {
            slot: pci_slot(&uevent).unwrap_or_default(),
            pci_id: match (vendor_id.is_empty(), device_id.is_empty()) {
                (false, false) => format!("{vendor_id}:{device_id}"),
                _ => String::new(),
            },
            device_id,
            vendor_id,
            vram_used,
            vram_total,
            busy,
            power,
            temp,
        });
    }

    gpus
}

fn existing(path: &Path) -> Option<PathBuf> {
    path.exists().then(|| path.to_path_buf())
}

/// `PCI_SLOT_NAME=0000:03:00.0` out of a device `uevent` file.
fn pci_slot(uevent: &str) -> Option<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
        .map(|value| value.trim().to_owned())
}

/// `0x1002` becomes `1002`, an already short value stays as it is.
fn normalize_pci_id(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    let value = value.strip_prefix("0x").unwrap_or(&value);
    value.trim_start_matches('0').to_owned()
}

/// The `hwmon` node that belongs to one PCI device.
fn amdgpu_hwmon(device: &Path) -> Option<PathBuf> {
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
        if fs::canonicalize(hwmon.join("device")).ok().as_deref() == Some(device.as_path()) {
            return Some(hwmon);
        }
    }

    None
}

/// Board power node of one hwmon directory.
///
/// btop prefers `power1_average` (filtered, smoothed) over `power1_input`
/// (instantaneous). Some cards leave the averaged value empty, so a node is only
/// accepted when it currently holds a readable number — otherwise the row would
/// show 0 W although the card draws power.
fn amdgpu_power_path(hwmon: &Path) -> Option<PathBuf> {
    for candidate in ["power1_average", "power1_input"] {
        let path = hwmon.join(candidate);
        if read_f32(&path).is_some() {
            return Some(path);
        }
    }
    None
}

/// A human readable model name plus the short label derived from it.
fn gpu_names(vendor_id: &str, device_id: &str) -> (String, String) {
    let fallback = || {
        let label = if vendor_id.is_empty() || device_id.is_empty() {
            "GPU".to_owned()
        } else {
            format!("GPU ({vendor_id}:{device_id})")
        };
        (label.clone(), label)
    };

    if vendor_id.is_empty() || device_id.is_empty() {
        return fallback();
    }
    let Some(vendor) = pci_vendor_name(vendor_id) else {
        return fallback();
    };
    let Some(model) = pci_device_name(vendor_id, device_id) else {
        return fallback();
    };

    // `Navi 48 [Radeon AI PRO R9700]` -> `Radeon AI PRO R9700`; the code name in
    // front of the bracket is what the PCI database uses for sorting.
    let model = match (model.find('['), model.rfind(']')) {
        (Some(start), Some(end)) if end > start + 1 => model[start + 1..end].to_owned(),
        _ => model,
    };

    (format!("{} {model}", short_vendor(&vendor)), model)
}

/// Shortens a PCI vendor to the name people use in a panel row.
///
/// The database says `Advanced Micro Devices, Inc. [AMD/ATI]`; the bracket holds
/// the short form and is preferred. `NVIDIA Corporation` loses its legal suffix.
fn short_vendor(full: &str) -> String {
    if let (Some(start), Some(end)) = (full.find('['), full.rfind(']')) {
        if end > start + 1 {
            return full[start + 1..end].split('/').next().unwrap_or("").trim().to_owned();
        }
    }
    for suffix in [" Corporation", ", Inc.", " Inc.", " GmbH", " Ltd."] {
        if let Some(stripped) = full.strip_suffix(suffix) {
            return stripped.trim().to_owned();
        }
    }
    full.to_owned()
}

/// The PCI IDs database, parsed once per process.
fn pci_ids() -> &'static PciIds {
    static IDS: OnceLock<PciIds> = OnceLock::new();
    IDS.get_or_init(|| {
        let text = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"]
            .iter()
            .find_map(|path| fs::read_to_string(path).ok())
            .unwrap_or_default();
        PciIds::parse(&text)
    })
}

fn pci_vendor_name(vendor_id: &str) -> Option<String> {
    pci_ids().vendor(vendor_id)
}

fn pci_device_name(vendor_id: &str, device_id: &str) -> Option<String> {
    pci_ids().device(vendor_id, device_id)
}

/// Minimal reader for `/usr/share/hwdata/pci.ids`.
///
/// Only the vendor block and the plain device lines inside it are indexed; the
/// `\t\t` subsystem lines (`subvendor subdevice  name`) are skipped on purpose.
#[derive(Debug, Default)]
struct PciIds {
    vendors: std::collections::HashMap<String, String>,
    devices: std::collections::HashMap<(String, String), String>,
}

impl PciIds {
    fn parse(text: &str) -> Self {
        let mut ids = Self::default();
        let mut vendor = String::new();

        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !line.starts_with('\t') {
                // `1002  Advanced Micro Devices, Inc. [AMD/ATI]`
                if let Some((id, name)) = line.split_once(char::is_whitespace) {
                    if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()) {
                        vendor = normalize_pci_id(id);
                        ids.vendors.insert(vendor.clone(), name.trim().to_owned());
                    }
                }
                continue;
            }
            if line.starts_with("\t\t") || vendor.is_empty() {
                continue;
            }
            // `\t7551  Navi 48 [Radeon AI PRO R9700]`
            if let Some((id, name)) = line.trim_start().split_once(char::is_whitespace) {
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()) {
                    ids.devices
                        .insert((vendor.clone(), normalize_pci_id(id)), name.trim().to_owned());
                }
            }
        }

        ids
    }

    fn vendor(&self, vendor_id: &str) -> Option<String> {
        // The raw database line is kept, including the `[AMD/ATI]` short form:
        // `short_vendor` needs it.
        self.vendors.get(&normalize_pci_id(vendor_id)).cloned()
    }

    fn device(&self, vendor_id: &str, device_id: &str) -> Option<String> {
        self.devices
            .get(&(normalize_pci_id(vendor_id), normalize_pci_id(device_id)))
            .cloned()
    }
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
    /// Samples of the last hour, oldest first.
    pub history: VecDeque<Sample>,
    /// Most recent sample.
    pub last: Sample,
}

impl Monitor {
    pub fn new() -> Self {
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
            gpus: find_amdgpus(),
            history: VecDeque::with_capacity(HISTORY_LEN),
            last: Sample::default(),
        }
    }

    /// Reads every value and appends the result to the history.
    pub fn tick(&mut self) -> Sample {
        let sample = self.read();
        self.last = sample.clone();
        if self.history.len() >= HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(sample.clone());
        sample
    }

    fn read(&mut self) -> Sample {
        let cpu_load = self.cpu_load();
        let (mem_used, mem_total) = read_memory();
        let gpus = self.gpu_values();
        let cpu_watts = self.cpu_watts();

        let vram_used: f32 = gpus.iter().map(|gpu| gpu.vram_used).sum();
        let vram_total: f32 = gpus.iter().map(|gpu| gpu.vram_total).sum();
        let gpu_watts: f32 = gpus.iter().map(|gpu| gpu.watts).sum();

        Sample {
            cpu_load,
            mem_used,
            mem_total,
            vram_used,
            vram_total,
            cpu_watts: cpu_watts.unwrap_or(0.0),
            cpu_watts_available: cpu_watts.is_some(),
            gpu_watts,
            gpus,
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

    /// Utilization, VRAM, temperature and board power of every GPU.
    fn gpu_values(&self) -> Vec<GpuInfo> {
        self.gpus
            .iter()
            .map(|gpu| {
                let (name, short) = gpu_names(&gpu.vendor_id, &gpu.device_id);
                let watts = gpu
                    .power
                    .as_ref()
                    .and_then(|path| read_f32(path))
                    .map(|microwatts| microwatts / 1_000_000.0)
                    .filter(|watts| *watts > 0.0);

                GpuInfo {
                    slot: gpu.slot.clone(),
                    name,
                    short,
                    pci_id: gpu.pci_id.clone(),
                    device_id: gpu.device_id.clone(),
                    load: gpu
                        .busy
                        .as_ref()
                        .and_then(|path| read_f32(path))
                        .unwrap_or(0.0),
                    temp_c: gpu
                        .temp
                        .as_ref()
                        .and_then(|path| read_f32(path))
                        .map(|millidegrees| millidegrees / 1000.0)
                        .unwrap_or(0.0),
                    watts: watts.unwrap_or(0.0),
                    watts_available: watts.is_some(),
                    vram_used: gpu
                        .vram_used
                        .as_ref()
                        .and_then(|path| read_u64(path))
                        .unwrap_or(0) as f32
                        / GIB,
                    vram_total: gpu
                        .vram_total
                        .as_ref()
                        .and_then(|path| read_u64(path))
                        .unwrap_or(0) as f32
                        / GIB,
                }
            })
            .collect()
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCI_IDS_SAMPLE: &str = "\
# comment line
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t13c0  Granite Ridge [Radeon 610M]
\t7551  Navi 48 [Radeon AI PRO R9700]
\t\t1002 7551  Subsystem entry that must be ignored
1000  Broadcom / LSI
\t005e  SAS1064ET PCI-Express Fusion-MPT SAS
";

    #[test]
    fn card_names_ignore_connectors() {
        assert_eq!(card_index("card1"), Some(1));
        assert_eq!(card_index("card12"), Some(12));
        assert_eq!(card_index("card1-DP-1"), None);
        assert_eq!(card_index("renderD128"), None);
        assert_eq!(card_index("card"), None);
    }

    #[test]
    fn pci_ids_lookup_reads_device_names() {
        let ids = PciIds::parse(PCI_IDS_SAMPLE);
        assert_eq!(
            ids.vendor("1002").as_deref(),
            Some("Advanced Micro Devices, Inc. [AMD/ATI]")
        );
        assert_eq!(
            ids.vendor("0x1002").as_deref(),
            Some("Advanced Micro Devices, Inc. [AMD/ATI]")
        );
        assert_eq!(short_vendor(&ids.vendor("1002").unwrap()), "AMD");
        assert_eq!(
            ids.device("1002", "7551").as_deref(),
            Some("Navi 48 [Radeon AI PRO R9700]")
        );
        // The subsystem line with two IDs must not shadow the device line.
        assert_eq!(ids.device("1002", "13c0").as_deref(), Some("Granite Ridge [Radeon 610M]"));
        assert_eq!(ids.device("1002", "ffff"), None);
    }

    #[test]
    fn bracket_names_become_short_labels() {
        let (name, short) = {
            let ids = PciIds::parse(PCI_IDS_SAMPLE);
            let model = ids.device("1002", "7551").unwrap();
            let model = match (model.find('['), model.rfind(']')) {
                (Some(start), Some(end)) if end > start + 1 => model[start + 1..end].to_owned(),
                _ => model,
            };
            (
                format!("{} {}", short_vendor(&ids.vendor("1002").unwrap()), model),
                model,
            )
        };
        assert_eq!(name, "AMD Radeon AI PRO R9700");
        assert_eq!(short, "Radeon AI PRO R9700");
    }

    #[test]
    fn vendor_names_are_shortened() {
        assert_eq!(short_vendor("Advanced Micro Devices, Inc. [AMD/ATI]"), "AMD");
        assert_eq!(short_vendor("NVIDIA Corporation"), "NVIDIA");
        assert_eq!(short_vendor("Intel Corporation"), "Intel");
        assert_eq!(short_vendor("Broadcom / LSI"), "Broadcom / LSI");
    }

    #[test]
    fn pci_slot_is_read_from_uevent() {
        let uevent = "DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:7551\nPCI_SLOT_NAME=0000:03:00.0\n";
        assert_eq!(pci_slot(uevent).as_deref(), Some("0000:03:00.0"));
        assert_eq!(pci_slot("DRIVER=amdgpu\n"), None);
    }

    #[test]
    fn pci_ids_are_normalised() {
        assert_eq!(normalize_pci_id("0x1002"), "1002");
        assert_eq!(normalize_pci_id("0x13C0"), "13c0");
        assert_eq!(normalize_pci_id(" 7551 "), "7551");
    }

    /// Live check against the running machine: run with
    /// `cargo test --release detected_gpus -- --nocapture` to print the GPUs and
    /// compare them with `btop` or `nvtop` on the same host.
    #[test]
    fn detected_gpus_are_readable_on_this_host() {
        let mut monitor = Monitor::new();
        let sample = monitor.tick();

        for (index, gpu) in sample.gpus.iter().enumerate() {
            println!(
                "GPU {} · {} [{}] {} % · {:.0} °C · {}{} · {:.1}/{:.1} GiB",
                index + 1,
                gpu.name,
                if gpu.slot.is_empty() { "?" } else { &gpu.slot },
                gpu.load,
                gpu.temp_c,
                if gpu.watts_available {
                    format!("{:.1} W", gpu.watts)
                } else {
                    "kein Leistungswert".to_owned()
                },
                if gpu.pci_id.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", gpu.pci_id)
                },
                gpu.vram_used,
                gpu.vram_total,
            );
        }
        println!(
            "Summe: {:.1}/{:.1} GiB VRAM · {:.1} W",
            sample.vram_used, sample.vram_total, sample.gpu_watts
        );

        // A machine without any AMD card stays valid, so only the consistency of
        // what was found is asserted here, not a fixed card count.
        for gpu in &sample.gpus {
            assert!(!gpu.name.is_empty(), "jede Karte braucht einen Namen");
            assert!(gpu.load <= 100.0, "Auslastung liegt zwischen 0 und 100 %");
            assert!(gpu.vram_used <= gpu.vram_total, "belegter VRAM <= VRAM gesamt");
        }
    }

    #[test]
    fn sample_aggregates_match_the_single_card_values() {        let gpu = |load: f32, used: f32, total: f32, watts: f32, temp: f32| GpuInfo {
            slot: String::new(),
            name: "GPU".to_owned(),
            short: "GPU".to_owned(),
            pci_id: String::new(),
            device_id: String::new(),
            load,
            temp_c: temp,
            watts,
            watts_available: watts > 0.0,
            vram_used: used,
            vram_total: total,
        };

        let sample = Sample {
            vram_used: 20.0,
            vram_total: 64.0,
            gpu_watts: 30.0,
            gpus: vec![
                gpu(3.0, 18.0, 32.0, 15.0, 27.0),
                gpu(7.0, 2.0, 32.0, 15.0, 30.0),
            ],
            ..Sample::default()
        };

        assert_eq!(sample.vram_pct(), 31.25);
    }
}
