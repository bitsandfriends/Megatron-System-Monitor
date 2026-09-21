// SPDX-License-Identifier: GPL-3.0-only
//! Megatron sysmon agent.
//!
//! Small Rust service for the DGX Spark nodes: it samples `/proc`, keeps one
//! resident `nvidia-smi` query, discovers local LLM servers and their models,
//! scrapes the metrics of the primary engine and pushes one JSON document per
//! interval to every connected WebSocket client (the COSMIC panel applet).
//!
//! Resource posture: a static binary, two dependencies, no async runtime, one
//! small thread per task, and work proportional to the push interval. Nothing is
//! polled faster than it is needed, and nothing runs when no client is attached.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Reported to the applet, which compares it with the version it ships.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const LLM_PORTS: [u16; 8] = [8000, 8001, 8002, 8080, 8081, 9000, 30000, 11434];
/// Clients that fetch one snapshot and disconnect (the applet does) must still
/// keep the sampling alive: demand holds for this long after the last contact.
const DEMAND_GRACE_MS: u64 = 10_000;
const VLLM_METRICS: [&str; 11] = [
    "vllm:num_requests_running",
    "vllm:num_requests_waiting",
    "vllm:kv_cache_usage_perc",
    "vllm:gpu_cache_usage_perc",
    "vllm:prompt_tokens_total",
    "vllm:generation_tokens_total",
    "vllm:time_to_first_token_seconds_sum",
    "vllm:time_to_first_token_seconds_count",
    "vllm:num_preemptions_total",
    "vllm:spec_decode_num_draft_tokens_total",
    "vllm:spec_decode_num_accepted_tokens_total",
];

// --------------------------------------------------------------------------- arguments

#[derive(Clone)]
struct Args {
    bind: String,
    port: u16,
    push_interval: f64,
    gpu_interval: f64,
    llm_interval: f64,
    discover: bool,
    discovery_interval: f64,
    metrics_url: String,
    token: String,
    once: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_owned(),
            port: 8787,
            push_interval: 2.0,
            gpu_interval: 3.0,
            llm_interval: 4.0,
            discover: true,
            discovery_interval: 60.0,
            metrics_url: "http://127.0.0.1:8000/metrics".to_owned(),
            token: String::new(),
            once: false,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut items = std::env::args().skip(1);
    while let Some(item) = items.next() {
        let mut value = || items.next().ok_or_else(|| format!("{item} needs a value"));
        match item.as_str() {
            "--bind" => args.bind = value()?,
            "--port" => args.port = value()?.parse().map_err(|_| "--port expects a number")?,
            "--push-interval" => args.push_interval = value()?.parse().map_err(|_| "--push-interval expects seconds")?,
            "--gpu-interval" => args.gpu_interval = value()?.parse().map_err(|_| "--gpu-interval expects seconds")?,
            "--vllm-interval" => args.llm_interval = value()?.parse().map_err(|_| "--vllm-interval expects seconds")?,
            "--vllm-url" => args.metrics_url = value()?,
            "--discovery-interval" => {
                args.discovery_interval = value()?.parse().map_err(|_| "--discovery-interval expects seconds")?
            }
            "--discover" => args.discover = true,
            "--no-discover" => args.discover = false,
            "--token" => args.token = value()?,
            "--once" => args.once = true,
            "--version" => {
                println!("megatron-sysmon-agent {VERSION}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                print!(
                    "megatron-sysmon-agent {VERSION}\n\n\
                     --bind ADDR (0.0.0.0)      --port N (8787)\n\
                     --push-interval S (2)      --gpu-interval S (3)\n\
                     --vllm-interval S (4)      --vllm-url URL\n\
                     --discover / --no-discover --discovery-interval S (60)\n\
                     --token VALUE              --once\n"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

/// Diagnostics go to stderr so stdout stays machine readable (`--once`).
fn log(message: &str) {
    eprintln!("[megatron-sysmon] {message}");
}

// --------------------------------------------------------------------------- small helpers

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

/// True while somebody is watching: an attached client or the grace period.
fn under_demand(last_client_seen: &AtomicU64) -> bool {
    now_millis().saturating_sub(last_client_seen.load(Ordering::Relaxed)) < DEMAND_GRACE_MS
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

/// Readable processor name.
///
/// x86 exposes a marketing name in `/proc/cpuinfo`; arm64 usually does not, so
/// the ARM part numbers are mapped to core names (values from the kernel's
/// `ARM_CPU_PART_*` defines) and the board name is used as a last resort.
fn cpu_model_name() -> String {
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("model name") {
                if let Some((_, name)) = value.split_once(':') {
                    let name = name.trim();
                    if !name.is_empty() {
                        return name.to_owned();
                    }
                }
            }
        }
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("Hardware") {
                if let Some((_, name)) = value.split_once(':') {
                    let name = name.trim();
                    if !name.is_empty() {
                        return name.to_owned();
                    }
                }
            }
        }
        let mut cores: Vec<String> = Vec::new();
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("CPU part") {
                if let Some((_, part)) = value.split_once(':') {
                    let name = arm_core_name(part.trim());
                    if let Some(name) = name {
                        if !cores.iter().any(|item| item == &name) {
                            cores.push(name);
                        }
                    }
                }
            }
        }
        if !cores.is_empty() {
            return format!("ARM {}", cores.join(" + "));
        }
    }
    for path in [
        "/sys/firmware/devicetree/base/model",
        "/sys/devices/virtual/dmi/id/product_name",
    ] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let name = value.trim_end_matches('\0').trim();
            if !name.is_empty() {
                return name.to_owned();
            }
        }
    }
    "CPU".to_owned()
}

/// ARM `CPU part` number to core name (`ARM_CPU_PART_*` in the kernel).
fn arm_core_name(part: &str) -> Option<String> {
    let value = u32::from_str_radix(part.trim_start_matches("0x"), 16).ok()?;
    let name = match value {
        0xD03 => "Cortex-A53",
        0xD05 => "Cortex-A55",
        0xD07 => "Cortex-A57",
        0xD08 => "Cortex-A72",
        0xD09 => "Cortex-A73",
        0xD0A => "Cortex-A75",
        0xD0B => "Cortex-A76",
        0xD0D => "Cortex-A77",
        0xD41 => "Cortex-A78",
        0xD44 => "Cortex-X1",
        0xD46 => "Cortex-A510",
        0xD47 => "Cortex-A710",
        0xD48 => "Cortex-X2",
        0xD4D => "Cortex-A715",
        0xD4E => "Cortex-X3",
        0xD80 => "Cortex-A520",
        0xD81 => "Cortex-A720",
        0xD82 => "Cortex-X4",
        0xD85 => "Cortex-X925",
        0xD87 => "Cortex-A725",
        _ => return None,
    };
    Some(name.to_owned())
}

fn read_first_line(path: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    Some(line)
}

/// Minimal HTTP GET for loopback endpoints: no TLS, no redirects, bounded body.
fn http_get(url: &str, timeout: Duration, limit: usize) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let stream = TcpStream::connect(authority).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let mut writer = stream.try_clone().ok()?;
    write!(
        writer,
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
    .ok()?;

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).ok()?;
    if !status.contains(" 200") {
        return None;
    }
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = Vec::new();
    let take = if length > 0 { length.min(limit) } else { limit };
    let mut limited = reader.take(take as u64);
    limited.read_to_end(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

fn http_get_json(url: &str, timeout: Duration) -> Option<Value> {
    let body = http_get(url, timeout, 512 * 1024)?;
    serde_json::from_str(&body).ok()
}

fn port_open(host: &str, port: u16, timeout: Duration) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    addresses
        .into_iter()
        .any(|address| TcpStream::connect_timeout(&address, timeout).is_ok())
}

// --------------------------------------------------------------------------- collectors

#[derive(Default, Clone)]
struct GpuState {
    available: bool,
    name: String,
    util_pct: f64,
    mem_util_pct: f64,
    temp_c: f64,
    power_w: f64,
    sm_mhz: f64,
    vram_used_mib: f64,
    vram_total_mib: f64,
    /// `nvidia`, `amd`, `intel` or empty when nothing was found.
    vendor: String,
}

/// One long-running `nvidia-smi` instead of a process per sample.
struct GpuCollector {
    state: Arc<Mutex<GpuState>>,
    stop: Arc<AtomicBool>,
}

impl GpuCollector {
    /// `demand` is false while no client is attached: the collector then parks
    /// and stops sampling, so an unwatched node costs nothing.
    fn start(interval_s: f64, last_client_seen: Arc<AtomicU64>) -> Self {
        let collector = Self {
            state: Arc::new(Mutex::new(GpuState::default())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let state = Arc::clone(&collector.state);
        let stop = Arc::clone(&collector.stop);
        thread::Builder::new()
            .name("gpu".to_owned())
            .stack_size(192 * 1024)
            .spawn(move || {
                let interval_ms = ((interval_s * 1000.0) as u64).max(500);
                // NVIDIA first: one long-running nvidia-smi instead of a process
                // per sample. If it is unavailable, AMD/Intel GPUs are read from
                // sysfs (utilization, VRAM, power, temperature).
                match nvidia_fields() {
                    Some(fields) => {
                        log(&format!("gpu: nvidia-smi with {fields}"));
                        nvidia_loop(&state, &stop, &last_client_seen, &fields, interval_ms);
                    }
                    None => {
                        let gpus = sysfs_gpus();
                        if gpus.is_empty() {
                            log("gpu: neither nvidia-smi nor a supported GPU in sysfs");
                            return;
                        }
                        log(&format!("gpu: {} sysfs GPU(s) without nvidia-smi", gpus.len()));
                        while !stop.load(Ordering::Relaxed) {
                            if !under_demand(&last_client_seen) {
                                thread::sleep(Duration::from_millis(500));
                                continue;
                            }
                            if let Ok(mut guard) = state.lock() {
                                *guard = sample_sysfs(&gpus);
                            }
                            sleep_interruptible(&stop, Duration::from_secs_f64(interval_s));
                        }
                    }
                }
            })
            .ok();
        collector
    }

    fn sample(&self) -> GpuState {
        self.state.lock().map(|guard| guard.clone()).unwrap_or_default()
    }
}

/// Field list for `nvidia-smi --query-gpu`, reduced until the driver accepts it.
fn nvidia_fields() -> Option<String> {
    const CANDIDATES: [&str; 3] = [
        "name,utilization.gpu,utilization.memory,temperature.gpu,power.draw,clocks.current.sm,memory.used,memory.total",
        "name,utilization.gpu,temperature.gpu,memory.used,memory.total",
        "name,memory.used,memory.total",
    ];
    for fields in CANDIDATES {
        let output = Command::new("nvidia-smi")
            .args([
                &format!("--query-gpu={fields}"),
                "--format=csv,noheader,nounits",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        if let Ok(output) = output {
            if output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
                return Some(fields.to_owned());
            }
        }
    }
    None
}

fn nvidia_loop(
    state: &Arc<Mutex<GpuState>>,
    stop: &Arc<AtomicBool>,
    last_client_seen: &Arc<AtomicU64>,
    fields: &str,
    interval_ms: u64,
) {
    while !stop.load(Ordering::Relaxed) {
        if !under_demand(last_client_seen) {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let child = Command::new("nvidia-smi")
            .args([
                &format!("--query-gpu={fields}"),
                "--format=csv,noheader,nounits",
                &format!("--loop-ms={interval_ms}"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            thread::sleep(Duration::from_secs(5));
            continue;
        };
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if stop.load(Ordering::Relaxed) || !under_demand(last_client_seen) {
                    break;
                }
                if let Some(sample) = parse_nvidia_line(&line, fields) {
                    if let Ok(mut guard) = state.lock() {
                        *guard = sample;
                    }
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        thread::sleep(Duration::from_secs(5));
    }
}

/// `nounits` keeps the values numeric, so the position decides the meaning.
fn parse_nvidia_line(line: &str, fields: &str) -> Option<GpuState> {
    let values: Vec<&str> = line.split(',').map(str::trim).collect();
    let names: Vec<&str> = fields.split(',').collect();
    if values.len() < names.len() {
        return None;
    }
    let mut sample = GpuState {
        available: true,
        vendor: "nvidia".to_owned(),
        ..GpuState::default()
    };
    for (name, value) in names.iter().zip(values.iter()) {
        match *name {
            "name" => sample.name = (*value).to_owned(),
            "utilization.gpu" => sample.util_pct = number(value),
            "utilization.memory" => sample.mem_util_pct = number(value),
            "temperature.gpu" => sample.temp_c = number(value),
            "power.draw" => sample.power_w = number(value),
            "clocks.current.sm" => sample.sm_mhz = number(value),
            "memory.used" => sample.vram_used_mib = number(value),
            "memory.total" => sample.vram_total_mib = number(value),
            _ => {}
        }
    }
    Some(sample)
}

/// One AMD/Intel GPU found in sysfs.
struct SysfsGpu {
    name: String,
    vendor: String,
    busy: PathBuf,
    vram_used: PathBuf,
    vram_total: PathBuf,
    power: PathBuf,
    temp: PathBuf,
}

fn sysfs_gpus() -> Vec<SysfsGpu> {
    let mut gpus = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return gpus;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only "cardN", never "cardN-DP-1" or "renderD128".
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device = entry.path().join("device");
        let vendor_id = match std::fs::read_to_string(device.join("vendor")) {
            Ok(value) => value.trim().to_ascii_lowercase(),
            Err(_) => continue,
        };
        let vendor = match vendor_id.as_str() {
            "0x1002" => "amd",
            "0x8086" => "intel",
            _ => continue,
        };
        let device_id = std::fs::read_to_string(device.join("device"))
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let mut power = PathBuf::new();
        let mut temp = PathBuf::new();
        if let Ok(hwmons) = std::fs::read_dir(device.join("hwmon")) {
            for hwmon in hwmons.flatten() {
                let path = hwmon.path();
                if path.join("power1_average").exists() || path.join("power1_input").exists() {
                    power = path.clone();
                }
                if path.join("temp1_input").exists() {
                    temp = path.clone();
                }
            }
        }
        gpus.push(SysfsGpu {
            name: pci_name(&device, &device_id, vendor),
            vendor: vendor.to_owned(),
            busy: device.join("gpu_busy_percent"),
            vram_used: device.join("mem_info_vram_used"),
            vram_total: device.join("mem_info_vram_total"),
            power,
            temp,
        });
    }
    gpus
}

/// Marketing name from `lspci` when available, else a readable fallback.
fn pci_name(device: &std::path::Path, device_id: &str, vendor: &str) -> String {
    let slot = std::fs::canonicalize(device)
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if !slot.is_empty() {
        if let Ok(output) = Command::new("lspci")
            .args(["-s", &slot, "-mm"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                // Fields are quoted: vendor "AMD/ATI" device "Strix Halo ..."
                let parts: Vec<String> = text
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect();
                // lspci -mm: slot "class" "vendor" "device" ...
                if parts.len() >= 3 {
                    let device = parts[2].trim();
                    if device.is_empty() || device.starts_with("Device") {
                        return format!("{} {}", vendor.to_uppercase(), device).trim().to_owned();
                    }
                    return device.to_owned();
                }
            }
        }
    }
    let vendor_name = if vendor == "amd" { "AMD" } else { "Intel" };
    format!("{vendor_name} GPU {device_id}")
}

/// Aggregates all sysfs GPUs: highest utilization, summed VRAM and power.
fn sample_sysfs(gpus: &[SysfsGpu]) -> GpuState {
    let mut sample = GpuState {
        available: true,
        vendor: gpus.first().map(|gpu| gpu.vendor.clone()).unwrap_or_default(),
        ..GpuState::default()
    };
    let mut names = Vec::new();
    for gpu in gpus {
        names.push(gpu.name.clone());
        sample.util_pct = sample.util_pct.max(read_number(&gpu.busy));
        sample.vram_used_mib += read_number(&gpu.vram_used) / 1024.0 / 1024.0;
        sample.vram_total_mib += read_number(&gpu.vram_total) / 1024.0 / 1024.0;
        let watts = if gpu.power.join("power1_average").exists() {
            read_number(&gpu.power.join("power1_average")) / 1_000_000.0
        } else {
            read_number(&gpu.power.join("power1_input")) / 1_000_000.0
        };
        sample.power_w += watts;
        sample.temp_c = sample.temp_c.max(read_number(&gpu.temp.join("temp1_input")) / 1000.0);
    }
    sample.name = if names.len() > 1 {
        format!("{} (+{})", names[0], names.len() - 1)
    } else {
        names.first().cloned().unwrap_or_default()
    };
    if sample.vram_total_mib > 0.0 {
        sample.mem_util_pct = sample.vram_used_mib / sample.vram_total_mib * 100.0;
    }
    sample
}

fn read_number(path: &std::path::Path) -> f64 {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| number(&value))
        .unwrap_or(0.0)
}

impl Drop for GpuCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Reads the leading number of values such as `11.56 W` or `[N/A]`.
fn number(text: &str) -> f64 {
    let mut cleaned = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() || matches!(character, '+' | '-' | '.' | 'e' | 'E') {
            cleaned.push(character);
        } else if !cleaned.is_empty() {
            break;
        }
    }
    cleaned.parse().unwrap_or(0.0)
}

struct NodeCollector {
    hostname: String,
    cpu_model: String,
    cpu_count: usize,
    previous: Option<(f64, f64)>,
    cpu_pct: f64,
}

impl NodeCollector {
    fn new() -> Self {
        Self {
            hostname: read_first_line("/proc/sys/kernel/hostname")
                .map(|line| line.trim().to_owned())
                .unwrap_or_default(),
            cpu_model: cpu_model_name(),
            cpu_count: std::thread::available_parallelism().map(|value| value.get()).unwrap_or(0),
            previous: None,
            cpu_pct: 0.0,
        }
    }

    fn sample(&mut self) -> Value {
        let mut load = [0.0_f64; 3];
        if let Some(line) = read_first_line("/proc/loadavg") {
            for (index, part) in line.split_whitespace().take(3).enumerate() {
                load[index] = part.parse().unwrap_or(0.0);
            }
        }

        let (mut total_kib, mut available_kib, mut swap_total_kib, mut swap_free_kib) = (0.0, 0.0, 0.0, 0.0);
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            for line in text.lines() {
                let value = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|item| item.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if line.starts_with("MemTotal:") {
                    total_kib = value;
                } else if line.starts_with("MemAvailable:") {
                    available_kib = value;
                } else if line.starts_with("SwapTotal:") {
                    swap_total_kib = value;
                } else if line.starts_with("SwapFree:") {
                    swap_free_kib = value;
                }
            }
        }

        let total_gib = total_kib / (1024.0 * 1024.0);
        let used_gib = (total_gib - available_kib / (1024.0 * 1024.0)).max(0.0);
        let uptime = read_first_line("/proc/uptime")
            .and_then(|line| line.split_whitespace().next().map(|item| item.to_owned()))
            .and_then(|item| item.parse::<f64>().ok())
            .unwrap_or(0.0);

        json!({
            "cpu_pct": (self.cpu_load() * 10.0).round() / 10.0,
            "cpu_count": self.cpu_count,
            "cpu_model": self.cpu_model,
            "load1": load[0], "load5": load[1], "load15": load[2],
            "mem_total_gib": (total_gib * 100.0).round() / 100.0,
            "mem_used_gib": (used_gib * 100.0).round() / 100.0,
            "mem_avail_gib": ((available_kib / (1024.0 * 1024.0)) * 100.0).round() / 100.0,
            "mem_pct": if total_gib > 0.0 { (used_gib / total_gib * 1000.0).round() / 10.0 } else { 0.0 },
            "swap_total_gib": (swap_total_kib / (1024.0 * 1024.0) * 100.0).round() / 100.0,
            "swap_used_gib": ((swap_total_kib - swap_free_kib) / (1024.0 * 1024.0) * 100.0).round() / 100.0,
            "uptime_s": (uptime * 10.0).round() / 10.0,
        })
    }

    fn cpu_load(&mut self) -> f64 {
        let Some(line) = read_first_line("/proc/stat") else {
            return self.cpu_pct;
        };
        let values: Vec<f64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|item| item.parse::<f64>().ok())
            .collect();
        if values.len() < 5 {
            return self.cpu_pct;
        }
        let idle = values[3] + values.get(4).copied().unwrap_or(0.0);
        let total: f64 = values.iter().take(8).sum();
        if let Some((previous_total, previous_idle)) = self.previous {
            let total_delta = total - previous_total;
            let idle_delta = idle - previous_idle;
            if total_delta > 0.0 {
                self.cpu_pct = ((total_delta - idle_delta) / total_delta * 100.0).clamp(0.0, 100.0);
            }
        }
        self.previous = Some((total, idle));
        self.cpu_pct
    }
}

#[derive(Default, Clone)]
struct LlmState {
    available: bool,
    error: String,
    reason: String,
    model: String,
    running: u64,
    waiting: u64,
    kv_cache_pct: f64,
    gen_tokens_total: f64,
    prompt_tokens_total: f64,
    preemptions_total: u64,
    ttft_ms: f64,
    gen_tok_s: f64,
    prompt_tok_s: f64,
    spec_accept_pct: Option<f64>,
}

struct LlmCollector {
    state: Arc<Mutex<LlmState>>,
    url: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
}

impl LlmCollector {
    fn start(url: String, interval_s: f64, last_client_seen: Arc<AtomicU64>) -> Self {
        let collector = Self {
            state: Arc::new(Mutex::new(LlmState {
                reason: "pending".to_owned(),
                error: "noch nicht gelesen".to_owned(),
                ..LlmState::default()
            })),
            url: Arc::new(Mutex::new(url)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let state = Arc::clone(&collector.state);
        let url = Arc::clone(&collector.url);
        let stop = Arc::clone(&collector.stop);
        thread::Builder::new()
            .name("llm".to_owned())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut previous: Option<(f64, HashMap<String, f64>)> = None;
                while !stop.load(Ordering::Relaxed) {
                    // Nothing to scrape while nobody is watching.
                    if !under_demand(&last_client_seen) {
                        previous = None;
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    let current = url.lock().map(|guard| guard.clone()).unwrap_or_default();
                    if current.is_empty() {
                        if let Ok(mut guard) = state.lock() {
                            *guard = LlmState {
                                reason: "pending".to_owned(),
                                error: "kein lokaler LLM-Endpunkt gefunden".to_owned(),
                                ..LlmState::default()
                            };
                        }
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    match http_get(&current, Duration::from_secs(5), 4 * 1024 * 1024) {
                        Some(body) => {
                            let (values, model) = parse_vllm_metrics(&body);
                            let now = now_seconds();
                            let snapshot = derive_llm(&values, &model, previous.as_ref(), now);
                            previous = Some((now, values));
                            if let Ok(mut guard) = state.lock() {
                                *guard = snapshot;
                            }
                        }
                        None => {
                            if let Ok(mut guard) = state.lock() {
                                *guard = LlmState {
                                    reason: "no-endpoint".to_owned(),
                                    error: "kein lokaler vLLM-Endpunkt (headless Rank?)".to_owned(),
                                    ..LlmState::default()
                                };
                            }
                            previous = None;
                        }
                    }
                    sleep_interruptible(&stop, Duration::from_secs_f64(interval_s));
                }
            })
            .ok();
        collector
    }

    fn current_url(&self) -> String {
        self.url.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    fn sample(&self) -> LlmState {
        self.state.lock().map(|guard| guard.clone()).unwrap_or_default()
    }
}

impl Drop for LlmCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn parse_vllm_metrics(body: &str) -> (HashMap<String, f64>, String) {
    let mut values = HashMap::new();
    let mut model = String::new();
    for line in body.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line.split(['{', ' ']).next().unwrap_or_default();
        if !VLLM_METRICS.contains(&name) {
            continue;
        }
        if let Some(value) = line.rsplit(' ').next().and_then(|item| item.parse::<f64>().ok()) {
            values.insert(name.to_owned(), value);
        }
        if model.is_empty() {
            if let Some(start) = line.find("model_name=\"") {
                let rest = &line[start + 12..];
                if let Some(end) = rest.find('"') {
                    model = rest[..end].to_owned();
                }
            }
        }
    }
    (values, model)
}

fn derive_llm(
    values: &HashMap<String, f64>,
    model: &str,
    previous: Option<&(f64, HashMap<String, f64>)>,
    now: f64,
) -> LlmState {
    let get = |name: &str| values.get(name).copied().unwrap_or(0.0);
    let kv_key = if values.contains_key("vllm:kv_cache_usage_perc") {
        "vllm:kv_cache_usage_perc"
    } else {
        "vllm:gpu_cache_usage_perc"
    };
    let samples = get("vllm:time_to_first_token_seconds_count");
    let drafts = get("vllm:spec_decode_num_draft_tokens_total");
    let accepted = get("vllm:spec_decode_num_accepted_tokens_total");

    let mut state = LlmState {
        available: true,
        error: String::new(),
        reason: String::new(),
        model: model.to_owned(),
        running: get("vllm:num_requests_running") as u64,
        waiting: get("vllm:num_requests_waiting") as u64,
        // The gauge is a fraction: 1.0 means 100 %.
        kv_cache_pct: ((get(kv_key) * 100.0) * 10.0).round() / 10.0,
        gen_tokens_total: get("vllm:generation_tokens_total"),
        prompt_tokens_total: get("vllm:prompt_tokens_total"),
        preemptions_total: get("vllm:num_preemptions_total") as u64,
        ttft_ms: if samples > 0.0 {
            (get("vllm:time_to_first_token_seconds_sum") / samples * 1000.0 * 10.0).round() / 10.0
        } else {
            0.0
        },
        spec_accept_pct: if drafts > 0.0 {
            Some((accepted / drafts * 1000.0).round() / 10.0)
        } else {
            None
        },
        ..LlmState::default()
    };

    if let Some((previous_at, previous_values)) = previous {
        let seconds = now - previous_at;
        if seconds > 0.5 {
            let delta = |name: &str| {
                (get(name) - previous_values.get(name).copied().unwrap_or(0.0)).max(0.0)
            };
            state.gen_tok_s = (delta("vllm:generation_tokens_total") / seconds * 10.0).round() / 10.0;
            state.prompt_tok_s = (delta("vllm:prompt_tokens_total") / seconds * 10.0).round() / 10.0;
            let draft_delta = delta("vllm:spec_decode_num_draft_tokens_total");
            if draft_delta > 0.0 {
                state.spec_accept_pct =
                    Some((delta("vllm:spec_decode_num_accepted_tokens_total") / draft_delta * 1000.0).round() / 10.0);
            }
        }
    }
    state
}

#[derive(Clone)]
struct Engine {
    port: u16,
    kind: String,
    endpoint: String,
    metrics: String,
    /// All models the endpoint offers.
    models: Vec<String>,
    /// Models currently loaded into memory (Ollama: /api/ps).
    loaded: Vec<String>,
}

struct Discovery {
    engines: Arc<Mutex<Vec<Engine>>>,
    stop: Arc<AtomicBool>,
}

impl Discovery {
    fn start(interval_s: f64, enabled: bool, fallback_metrics: String, on_primary: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        let discovery = Self {
            engines: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let engines = Arc::clone(&discovery.engines);
        let stop = Arc::clone(&discovery.stop);
        thread::Builder::new()
            .name("discover".to_owned())
            .stack_size(256 * 1024)
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut found = if enabled {
                        let open_ports: Vec<u16> = LLM_PORTS
                            .iter()
                            .copied()
                            .filter(|port| port_open("127.0.0.1", *port, Duration::from_millis(300)))
                            .collect();
                        let engines = scan_llm_ports();
                        log(&format!(
                            "discovery: open ports {open_ports:?} -> engines {:?}",
                            engines.iter().map(|engine| engine.port).collect::<Vec<u16>>()
                        ));
                        engines
                    } else {
                        Vec::new()
                    };
                    if found.is_empty() {
                        if let Some(port) = port_of_url(&fallback_metrics) {
                            if port_open("127.0.0.1", port, Duration::from_millis(300)) {
                                found.push(Engine {
                                    port,
                                    kind: "vllm".to_owned(),
                                    endpoint: fallback_metrics.trim_end_matches("/metrics").to_owned(),
                                    metrics: fallback_metrics.clone(),
                                    models: Vec::new(),
                                    loaded: Vec::new(),
                                });
                            }
                        }
                    }
                    if let Some(primary) = found.iter().find(|engine| !engine.metrics.is_empty()) {
                        on_primary(&primary.metrics);
                    }
                    if let Ok(mut guard) = engines.lock() {
                        *guard = found;
                    }
                    sleep_interruptible(&stop, Duration::from_secs_f64(interval_s));
                }
            })
            .ok();
        discovery
    }

    /// Shared handle for command handlers.
    fn handle(&self) -> Arc<Mutex<Vec<Engine>>> {
        Arc::clone(&self.engines)
    }

    fn list(&self) -> Vec<Engine> {
        self.engines.lock().map(|guard| guard.clone()).unwrap_or_default()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn port_of_url(url: &str) -> Option<u16> {
    let authority = url.strip_prefix("http://")?.split('/').next()?;
    authority.rsplit(':').next()?.parse().ok()
}

/// Probes the usual LLM ports: a quick TCP check first, then the API listing.
fn scan_llm_ports() -> Vec<Engine> {
    let mut engines = Vec::new();
    for port in LLM_PORTS {
        if !port_open("127.0.0.1", port, Duration::from_millis(300)) {
            continue;
        }
        let endpoint = format!("http://127.0.0.1:{port}");

        // Ollama exposes an OpenAI-compatible listing as well; label it correctly.
        if let Some(tags) = http_get_json(&format!("{endpoint}/api/tags"), Duration::from_secs(1)) {
            if let Some(models) = tags.get("models").and_then(Value::as_array) {
                engines.push(Engine {
                    port,
                    kind: "ollama".to_owned(),
                    endpoint: endpoint.clone(),
                    metrics: String::new(),
                    models: models
                        .iter()
                        .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_owned))
                        .collect(),
                    // `/api/ps` lists what is loaded right now.
                    loaded: ollama_loaded_models(&endpoint),
                });
                continue;
            }
        }

        if let Some(listing) = http_get_json(&format!("{endpoint}/v1/models"), Duration::from_secs(2)) {
            let models: Vec<String> = listing
                .get("data")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let is_vllm = http_get(&format!("{endpoint}/metrics"), Duration::from_secs(2), 65536)
                .map(|body| body.contains("vllm:"))
                .unwrap_or(false);
            engines.push(Engine {
                port,
                kind: if is_vllm { "vllm".to_owned() } else { "openai".to_owned() },
                endpoint: endpoint.clone(),
                metrics: if is_vllm { format!("{endpoint}/metrics") } else { String::new() },
                loaded: models.clone(),
                models,
            });
            continue;
        }
        if let Some(tags) = http_get_json(&format!("{endpoint}/api/tags"), Duration::from_secs(2)) {
            let models = tags
                .get("models")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            engines.push(Engine {
                port,
                kind: "ollama".to_owned(),
                endpoint: endpoint.clone(),
                metrics: String::new(),
                loaded: ollama_loaded_models(&endpoint),
                models,
            });
        }
    }
    engines
}

/// Sleeps in coarse steps: a long wait must not cause busy wakeups, and a
/// shutdown still reacts within a second.
/// Established connections to a local port, grouped by remote address.
///
/// Read from `/proc/net/tcp` and `/proc/net/tcp6`, so no process is spawned.
/// This answers "who is connected to the engine" without needing per-request
/// attribution, which the OpenAI API does not provide.
fn client_connections(port: u16) -> Vec<(String, u32)> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // The file decides the address family; the string length does not.
        let is_ipv6 = path.ends_with("tcp6");
        for (address, count) in parse_tcp_table(&text, port, is_ipv6) {
            *counts.entry(address).or_insert(0) += count;
        }
    }
    let mut rows: Vec<(String, u32)> = counts.into_iter().collect();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    rows
}

// --------------------------------------------------------------------------- processes

/// Maximum number of process rows one reply may carry.
const PROC_LIMIT_MAX: usize = 1000;
/// Default row count when the client does not ask for a specific number.
const PROC_LIMIT_DEFAULT: usize = 200;
/// Longest command line sent to the applet; the popup only shows one line.
const PROC_CMD_MAX: usize = 160;

/// One row of the process table.
struct ProcRow {
    pid: u32,
    ppid: u32,
    name: String,
    user: String,
    cpu: f64,
    mem_kb: u64,
    state: String,
    cmd: String,
}

/// Reads the process table on demand and derives CPU shares between two calls.
///
/// The sampler is only touched when a client sends the `processes` action, so an
/// idle agent never scans `/proc`. It keeps the jiffy counters of the previous
/// scan because a percentage can only be computed from a delta.
struct ProcSampler {
    /// `utime + stime` per pid from the previous scan.
    previous: HashMap<u32, f64>,
    /// Total CPU jiffies of the previous scan.
    previous_total: f64,
    /// uid to user name, read once from `/etc/passwd`.
    users: HashMap<u32, String>,
}

impl ProcSampler {
    fn new() -> Self {
        Self {
            previous: HashMap::new(),
            previous_total: 0.0,
            users: user_names(),
        }
    }

    /// Total CPU jiffies across all cores, from the `cpu ` line of `/proc/stat`.
    fn cpu_total() -> Option<f64> {
        let line = read_first_line("/proc/stat")?;
        let mut parts = line.split_whitespace();
        if parts.next() != Some("cpu") {
            return None;
        }
        Some(parts.filter_map(|item| item.parse::<f64>().ok()).take(8).sum())
    }

    /// Builds the reply for the `processes` action, sorted by CPU descending.
    fn sample(&mut self, limit: usize) -> Result<Value, String> {
        let total = Self::cpu_total().ok_or_else(|| "/proc/stat nicht lesbar".to_owned())?;
        let total_delta = total - self.previous_total;
        let cores = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);

        let entries = std::fs::read_dir("/proc")
            .map_err(|error| format!("/proc nicht lesbar: {error}"))?;
        let mut rows: Vec<ProcRow> = Vec::new();
        let mut current: HashMap<u32, f64> = HashMap::new();

        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(line) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some((pid, name, state, ppid, utime, stime)) = parse_stat_line(&line) else {
                continue;
            };
            let jiffies = utime + stime;
            let (uid, mem_kb) = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .map(|text| parse_status(&text))
                .unwrap_or((None, 0));
            let user = match uid {
                Some(value) => self
                    .users
                    .get(&value)
                    .cloned()
                    .unwrap_or_else(|| value.to_string()),
                None => "-".to_owned(),
            };
            // A process without a previous sample (just started, or the very
            // first scan) reports 0.0 instead of a made-up value.
            let cpu = match self.previous.get(&pid) {
                Some(before) if total_delta > 0.0 => {
                    ((jiffies - before) / total_delta * cores as f64 * 100.0).max(0.0)
                }
                _ => 0.0,
            };
            rows.push(ProcRow {
                pid,
                ppid,
                name: name.clone(),
                user,
                cpu: (cpu * 10.0).round() / 10.0,
                mem_kb,
                state,
                cmd: read_cmdline(pid, &name),
            });
            current.insert(pid, jiffies);
        }

        self.previous = current;
        self.previous_total = total;

        rows.sort_by(|left, right| {
            right
                .cpu
                .partial_cmp(&left.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.pid.cmp(&right.pid))
        });
        let found = rows.len();
        rows.truncate(limit.clamp(1, PROC_LIMIT_MAX));

        let processes: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "pid": row.pid,
                    "ppid": row.ppid,
                    "name": row.name,
                    "user": row.user,
                    "cpu": row.cpu,
                    "mem_kb": row.mem_kb,
                    "state": row.state,
                    "cmd": row.cmd,
                })
            })
            .collect();

        Ok(json!({
            "ok": true,
            "action": "processes",
            "ts": (now_seconds() * 1000.0).round() / 1000.0,
            "total": found,
            "cores": cores,
            "processes": processes,
        }))
    }
}

/// Signals one process. Only the two signals the applet offers are accepted so
/// that a confirmation dialog in the UI always describes the real effect.
fn kill_process(pid: i64, signal: i64) -> Value {
    if signal != libc::SIGTERM as i64 && signal != libc::SIGKILL as i64 {
        return json!({
            "ok": false, "action": "kill", "pid": pid, "signal": signal,
            "error": "nur SIGTERM (15) und SIGKILL (9) sind erlaubt"
        });
    }
    if pid <= 1 {
        return json!({
            "ok": false, "action": "kill", "pid": pid, "signal": signal,
            "error": "PID 1 kann nicht beendet werden"
        });
    }
    if pid == i64::from(std::process::id()) {
        return json!({
            "ok": false, "action": "kill", "pid": pid, "signal": signal,
            "error": "der Agent kann sich nicht selbst beenden"
        });
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, signal as libc::c_int) };
    if result == 0 {
        return json!({"ok": true, "action": "kill", "pid": pid, "signal": signal});
    }

    let error = std::io::Error::last_os_error();
    let reason = match error.raw_os_error() {
        Some(libc::EPERM) => "keine Berechtigung (EPERM)".to_owned(),
        Some(libc::ESRCH) => "Prozess existiert nicht mehr".to_owned(),
        Some(libc::EINVAL) => "ungültiges Signal".to_owned(),
        Some(code) => format!("{error} (errno {code})"),
        None => error.to_string(),
    };
    json!({"ok": false, "action": "kill", "pid": pid, "signal": signal, "error": reason})
}

/// Cached uid to user name map, filled from `/etc/passwd`.
fn user_names() -> HashMap<u32, String> {
    let mut users = HashMap::new();
    if let Ok(text) = std::fs::read_to_string("/etc/passwd") {
        for line in text.lines() {
            let mut parts = line.split(':');
            let Some(name) = parts.next() else { continue };
            if parts.next().is_none() {
                continue;
            }
            let Some(uid) = parts.next().and_then(|item| item.parse::<u32>().ok()) else {
                continue;
            };
            users.insert(uid, name.to_owned());
        }
    }
    users
}

/// Splits one `/proc/<pid>/stat` line into the fields the agent needs.
///
/// The command name sits in parentheses and may itself contain spaces and
/// parentheses, so the numeric fields are parsed after the *last* `)`.
fn parse_stat_line(line: &str) -> Option<(u32, String, String, u32, f64, f64)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid = line[..open].trim().parse::<u32>().ok()?;
    let name = line[open + 1..close].to_owned();
    let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
    // state, ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt,
    // majflt, cmajflt, utime, stime
    if rest.len() < 13 {
        return None;
    }
    let state = rest[0].to_owned();
    let ppid = rest[1].parse::<u32>().unwrap_or(0);
    let utime = rest[11].parse::<f64>().unwrap_or(0.0);
    let stime = rest[12].parse::<f64>().unwrap_or(0.0);
    Some((pid, name, state, ppid, utime, stime))
}

/// Reads the uid and the resident memory from `/proc/<pid>/status`.
fn parse_status(text: &str) -> (Option<u32>, u64) {
    let mut uid = None;
    let mut rss_kb = 0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Uid:") {
            uid = value
                .split_whitespace()
                .next()
                .and_then(|item| item.parse::<u32>().ok());
        } else if let Some(value) = line.strip_prefix("VmRSS:") {
            rss_kb = value
                .split_whitespace()
                .next()
                .and_then(|item| item.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    (uid, rss_kb)
}

/// `/proc/<pid>/cmdline` is NUL separated; kernel threads have none at all.
fn read_cmdline(pid: u32, name: &str) -> String {
    let mut cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    if cmd.is_empty() {
        cmd = format!("[{name}]");
    }
    if cmd.chars().count() > PROC_CMD_MAX {
        let cut = cmd
            .char_indices()
            .nth(PROC_CMD_MAX)
            .map(|(index, _)| index)
            .unwrap_or(cmd.len());
        cmd.truncate(cut);
        cmd.push('…');
    }
    cmd
}

/// Executes a command sent by a client over the WebSocket connection.
///
/// Only endpoints the agent discovered itself are accepted, and only Ollama
/// models can be loaded; everything else is rejected with a message.
fn handle_command(
    payload: &[u8],
    engines: &Arc<Mutex<Vec<Engine>>>,
    processes: &Arc<Mutex<ProcSampler>>,
) -> Value {
    let Ok(command) = serde_json::from_slice::<Value>(payload) else {
        return json!({"ok": false, "error": "ungültiges JSON"});
    };
    match command.get("action").and_then(Value::as_str) {
        Some("ping") => json!({"ok": true, "action": "ping"}),
        Some("processes") => {
            let limit = command
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(PROC_LIMIT_DEFAULT);
            match processes.lock() {
                Ok(mut sampler) => match sampler.sample(limit) {
                    Ok(reply) => reply,
                    Err(error) => {
                        json!({"ok": false, "action": "processes", "error": error})
                    }
                },
                Err(_) => json!({
                    "ok": false, "action": "processes", "error": "Prozessliste ist belegt"
                }),
            }
        }
        Some("kill") => {
            let pid = command.get("pid").and_then(Value::as_i64).unwrap_or(0);
            let signal = command.get("signal").and_then(Value::as_i64).unwrap_or(0);
            kill_process(pid, signal)
        }
        Some("load_model") => {
            let port = command.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
            let model = command
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if model.is_empty() {
                return json!({"ok": false, "action": "load_model", "error": "kein Modell angegeben"});
            }
            let known = engines
                .lock()
                .map(|guard| {
                    guard
                        .iter()
                        .any(|engine| engine.port == port && engine.kind == "ollama")
                })
                .unwrap_or(false);
            if !known {
                return json!({
                    "ok": false, "action": "load_model",
                    "error": format!("Port {port} ist kein erkannter Ollama-Endpunkt")
                });
            }
            let body = json!({"model": model, "keep_alive": "10m", "prompt": ""}).to_string();
            let url = format!("http://127.0.0.1:{port}/api/generate");
            // Loading can take a while: the model has to be read from disk.
            match http_post(&url, &body, Duration::from_secs(180), 65536) {
                Some(response) => {
                    let loaded = ollama_loaded_models(&format!("http://127.0.0.1:{port}"));
                    let error = response.get("error").and_then(Value::as_str);
                    if let Some(error) = error {
                        return json!({
                            "ok": false, "action": "load_model", "model": model, "error": error
                        });
                    }
                    json!({
                        "ok": true, "action": "load_model", "model": model, "loaded": loaded
                    })
                }
                None => json!({
                    "ok": false, "action": "load_model", "model": model,
                    "error": "keine Antwort vom Ollama-Endpunkt"
                }),
            }
        }
        other => json!({
            "ok": false,
            "error": format!("unbekannte Aktion: {}", other.unwrap_or("-"))
        }),
    }
}

/// Simple POST for loopback requests; returns the parsed JSON body.
fn http_post(url: &str, body: &str, timeout: Duration, limit: usize) -> Option<Value> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(&authority).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let request = format!(
        "POST {path} HTTP/1.0\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).ok()?;
    if !status.contains(" 200") {
        return None;
    }
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let take = if length > 0 { length.min(limit) } else { limit };
    let mut body_bytes = vec![0u8; take];
    let mut read = 0usize;
    while read < take {
        match reader.read(&mut body_bytes[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(_) => break,
        }
    }
    body_bytes.truncate(read);
    serde_json::from_slice(&body_bytes).ok()
}

/// Models Ollama has in memory right now.
fn ollama_loaded_models(endpoint: &str) -> Vec<String> {
    http_get_json(&format!("{endpoint}/api/ps"), Duration::from_secs(2))
        .and_then(|value| {
            value.get("models").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Parses one `/proc/net/tcp*` table: established sockets on `port`, grouped by
/// remote address. Loopback and unspecified addresses are ignored.
fn parse_tcp_table(text: &str, port: u16, is_ipv6: bool) -> Vec<(String, u32)> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 || fields[3] != "01" {
            continue;
        }
        let Some((_, local_port)) = fields[1].rsplit_once(':') else {
            continue;
        };
        if u16::from_str_radix(local_port, 16).ok() != Some(port) {
            continue;
        }
        let Some((remote, _)) = fields[2].rsplit_once(':') else {
            continue;
        };
        // v4-mapped loopback (::ffff:127.0.0.1) ends with this word.
        if is_ipv6 && (remote.ends_with("0100007F") || remote == "00000000000000000000000000000000") {
            continue;
        }
        let Some(address) = decode_address(remote, is_ipv6) else {
            continue;
        };
        if address == "0.0.0.0"
            || address == "::"
            || address.starts_with("127.")
            || address == "::1"
            || address == "0:0:0:0:0:0:0:1"
        {
            continue;
        }
        *counts.entry(address).or_insert(0) += 1;
    }
    let mut rows: Vec<(String, u32)> = counts.into_iter().collect();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    rows
}

/// `/proc/net/tcp*` stores addresses as little-endian hex words.
fn decode_address(text: &str, is_ipv6: bool) -> Option<String> {
    if !is_ipv6 {
        let value = u32::from_str_radix(text, 16).ok()?;
        let bytes = value.to_be_bytes();
        return Some(format!("{}.{}.{}.{}", bytes[3], bytes[2], bytes[1], bytes[0]));
    }
    if text.len() != 32 {
        return None;
    }
    let mut groups = Vec::new();
    for chunk in text.as_bytes().chunks(8) {
        let part = std::str::from_utf8(chunk).ok()?;
        let word = u32::from_str_radix(part, 16).ok()?.swap_bytes();
        // One 32-bit word carries two 16-bit groups.
        groups.push(format!("{:x}", word >> 16));
        groups.push(format!("{:x}", word & 0xffff));
    }
    Some(groups.join(":"))
}

/// Binds the listening socket for both families when possible.
///
/// A dual-stack `::` socket also accepts IPv4 clients (bindv6only=0 is the Linux
/// default), so one listener serves hosts that only resolve to IPv6 and hosts
/// that use IPv4. If IPv6 is unavailable the agent falls back to IPv4 only.
fn bind_both_families(bind: &str, port: u16) -> std::io::Result<(TcpListener, String)> {
    let wants_ipv6 = matches!(bind, "::" | "0.0.0.0" | "");
    if wants_ipv6 {
        if let Ok(listener) = TcpListener::bind(("::", port)) {
            let address = listener
                .local_addr()
                .map(|value| value.to_string())
                .unwrap_or_else(|_| format!("[::]:{port}"));
            return Ok((listener, address));
        }
        if bind == "::" {
            log("IPv6 bind failed; falling back to IPv4");
        }
    }
    let fallback = if bind.is_empty() || bind == "::" { "0.0.0.0" } else { bind };
    let listener = TcpListener::bind((fallback, port))?;
    let address = listener
        .local_addr()
        .map(|value| value.to_string())
        .unwrap_or_else(|_| format!("{fallback}:{port}"));
    Ok((listener, address))
}

fn sleep_interruptible(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_secs(1).min(remaining));
    }
}

// --------------------------------------------------------------------------- snapshot

fn build_snapshot(node: &mut NodeCollector, gpu: &GpuCollector, llm: &LlmCollector, discovery: &Discovery, args: &Args) -> Value {
    let gpu_state = gpu.sample();
    let llm_state = llm.sample();
    let engines: Vec<Value> = discovery
        .list()
        .iter()
        .enumerate()
        .map(|(index, engine)| {
            json!({
                "port": engine.port,
                "kind": engine.kind,
                "endpoint": engine.endpoint,
                "metrics": engine.metrics,
                "models": engine.models,
                "model_count": engine.models.len(),
                "loaded": engine.loaded,
                "primary": index == 0 && !engine.metrics.is_empty(),
            })
        })
        .collect();

    let node_values = node.sample();
    let llm_block = if args.metrics_url.is_empty() && !args.discover {
        json!({"available": false, "reason": "disabled",
               "error": "LLM-Monitoring auf diesem Knoten deaktiviert"})
    } else if llm_state.available {
        json!({
            "available": true, "error": Value::Null, "reason": "",
            "model": llm_state.model,
            "running": llm_state.running, "waiting": llm_state.waiting,
            "kv_cache_pct": llm_state.kv_cache_pct,
            "gen_tokens_total": llm_state.gen_tokens_total,
            "prompt_tokens_total": llm_state.prompt_tokens_total,
            "preemptions_total": llm_state.preemptions_total,
            "ttft_ms": llm_state.ttft_ms,
            "gen_tok_s": llm_state.gen_tok_s,
            "prompt_tok_s": llm_state.prompt_tok_s,
            "spec_accept_pct": llm_state.spec_accept_pct,
        })
    } else {
        json!({"available": false, "reason": llm_state.reason, "error": llm_state.error})
    };

    json!({
        "agent": "megatron-sysmon",
        "version": VERSION,
        "host": node.hostname,
        "ts": (now_seconds() * 1000.0).round() / 1000.0,
        "uptime_s": node_values.get("uptime_s").cloned().unwrap_or(json!(0.0)),
        "node": node_values,
        "gpu": {
            "available": gpu_state.available,
            "name": gpu_state.name,
            "util_pct": gpu_state.util_pct,
            "mem_util_pct": gpu_state.mem_util_pct,
            "temp_c": gpu_state.temp_c,
            "power_w": gpu_state.power_w,
            "sm_mhz": gpu_state.sm_mhz,
            "vram_used_mib": gpu_state.vram_used_mib,
            "vram_total_mib": gpu_state.vram_total_mib,
            "vendor": gpu_state.vendor,
        },
        "llm": {
            "autodiscover": args.discover,
            "endpoint": llm.current_url(),
            "engines": engines,
        },
        "clients": {
            "port": discovery.list().iter().find(|engine| engine.port > 0).map(|engine| engine.port),
            "connections": discovery
                .list()
                .iter()
                .find(|engine| engine.port > 0)
                .map(|engine| {
                    client_connections(engine.port)
                        .into_iter()
                        .map(|(address, count)| json!({"ip": address, "count": count}))
                        .collect::<Vec<Value>>()
                })
                .unwrap_or_default(),
        },
        "vllm": llm_block,
    })
}

// --------------------------------------------------------------------------- websocket server

struct Client {
    stream: TcpStream,
}

fn encode_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x81);
    let length = payload.len();
    if length < 126 {
        frame.push(length as u8);
    } else if length < 65536 {
        frame.push(126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

fn websocket_handshake(stream: &mut TcpStream, token: &str) -> bool {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return false,
    });
    let mut request = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return false,
            Ok(_) => {
                if line.trim().is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            Err(_) => return false,
        }
    }

    let mut key = String::new();
    for line in request.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("sec-websocket-key:") {
            let _ = value;
            key = line.splitn(2, ':').nth(1).unwrap_or_default().trim().to_owned();
        }
    }
    if key.is_empty() {
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        return false;
    }

    if !token.is_empty() {
        let lowered = request.to_ascii_lowercase();
        let supplied = if let Some(index) = lowered.find("x-sysmon-token:") {
            request[index + 16..].lines().next().unwrap_or_default().trim().to_owned()
        } else if let Some(index) = request.find("token=") {
            request[index + 6..].split(['&', ' ']).next().unwrap_or_default().to_owned()
        } else {
            String::new()
        };
        if supplied != token {
            let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
            return false;
        }
    }

    let accept = websocket_accept(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).is_ok() && stream.flush().is_ok()
}

/// base64(sha1(key + GUID)) implemented locally: one dependency less.
fn websocket_accept(key: &str) -> String {
    let mut input = String::with_capacity(key.len() + WS_GUID.len());
    input.push_str(key);
    input.push_str(WS_GUID);
    base64(&sha1(input.as_bytes()))
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 80];
        for index in 0..16 {
            w[index] = u32::from_be_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (index, value) in h.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let triple = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(triple >> 18) as usize & 0x3F] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 { TABLE[(triple >> 6) as usize & 0x3F] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[triple as usize & 0x3F] as char } else { '=' });
    }
    out
}

fn read_client_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).ok()?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7f) as usize;
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended).ok()?;
        length = u16::from_be_bytes(extended) as usize;
    } else if length == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended).ok()?;
        length = u64::from_be_bytes(extended) as usize;
    }
    if length > 1_048_576 {
        return None;
    }
    let mut mask = [0u8; 4];
    if masked {
        stream.read_exact(&mut mask).ok()?;
    }
    let mut payload = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut payload).ok()?;
    }
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Some((opcode, payload))
}

fn serve(args: Args) -> std::io::Result<()> {
    let last_client_seen = Arc::new(AtomicU64::new(0));
    let gpu = GpuCollector::start(args.gpu_interval, Arc::clone(&last_client_seen));
    let llm = LlmCollector::start(
        args.metrics_url.clone(),
        args.llm_interval,
        Arc::clone(&last_client_seen),
    );
    let callback: Arc<dyn Fn(&str) + Send + Sync> = {
        let url_handle = Arc::clone(&llm.url);
        let state_handle = Arc::clone(&llm.state);
        Arc::new(move |url: &str| {
            if url.is_empty() {
                return;
            }
            if let Ok(mut guard) = url_handle.lock() {
                if *guard != url {
                    log(&format!("LLM metrics endpoint is now {url}"));
                    *guard = url.to_owned();
                }
            }
            if let Ok(mut guard) = state_handle.lock() {
                guard.reason.clear();
                guard.error.clear();
            }
        })
    };
    let discovery = Discovery::start(
        args.discovery_interval,
        args.discover,
        args.metrics_url.clone(),
        callback,
    );
    // Handle for the client threads; the discovery itself moves into the push
    // thread below.
    let discovery_engines = discovery.handle();
    let mut node = NodeCollector::new();
    // The process table is answered per client request: nothing is scanned while
    // no client asks for it.
    let procs: Arc<Mutex<ProcSampler>> = Arc::new(Mutex::new(ProcSampler::new()));

    if args.once {
        // A one-shot snapshot must always sample, even without a client.
        last_client_seen.store(now_millis(), Ordering::Relaxed);
        thread::sleep(Duration::from_millis(2000));
        let snapshot = build_snapshot(&mut node, &gpu, &llm, &discovery, &args);
        println!("{}", serde_json::to_string_pretty(&snapshot).unwrap_or_default());
        return Ok(());
    }

    let (listener, bound) = bind_both_families(&args.bind, args.port)?;
    log(&format!(
        "listening on {bound} (push {}s, discovery {}, vLLM {})",
        args.push_interval,
        if args.discover { "on" } else { "off" },
        if args.metrics_url.is_empty() { "off" } else { "on" }
    ));

    let clients: Arc<Mutex<Vec<Arc<Client>>>> = Arc::new(Mutex::new(Vec::new()));

    // Broadcaster: one snapshot per interval for every client.
    {
        let clients = Arc::clone(&clients);
        let last_client_seen = Arc::clone(&last_client_seen);
        let args = args.clone();
        thread::Builder::new()
            .name("push".to_owned())
            .stack_size(512 * 1024)
            .spawn(move || {
                let mut node = NodeCollector::new();
                let gpu = gpu;
                let llm = llm;
                let discovery = discovery;
                loop {
                    let attached = clients.lock().map(|guard| !guard.is_empty()).unwrap_or(false);
                    if attached {
                        last_client_seen.store(now_millis(), Ordering::Relaxed);
                    }
                    if !attached {
                        // No client: neither /proc nor the engine are sampled.
                        thread::sleep(Duration::from_secs_f64(args.push_interval));
                        continue;
                    }
                    let payload = build_snapshot(&mut node, &gpu, &llm, &discovery, &args).to_string();
                    let frame = encode_text_frame(payload.as_bytes());
                    let mut alive = Vec::new();
                    if let Ok(mut guard) = clients.lock() {
                        for client in guard.drain(..) {
                            let mut stream = &client.stream;
                            if stream.write_all(&frame).is_ok() && stream.flush().is_ok() {
                                alive.push(client);
                            }
                        }
                        if !alive.is_empty() {
                            last_client_seen.store(now_millis(), Ordering::Relaxed);
                        }
                        *guard = alive;
                    }
                    thread::sleep(Duration::from_secs_f64(args.push_interval));
                }
            })
            .ok();
    }

    for incoming in listener.incoming() {
        let Ok(mut stream) = incoming else { continue };
        let clients = Arc::clone(&clients);
        let last_client_seen = Arc::clone(&last_client_seen);
        let engines = Arc::clone(&discovery_engines);
        let procs = Arc::clone(&procs);
        let token = args.token.clone();
        thread::Builder::new()
            .name("client".to_owned())
            .stack_size(256 * 1024)
            .spawn(move || {
                let _ = stream.set_nodelay(true);
                if !websocket_handshake(&mut stream, &token) {
                    return;
                }
                // A fetch-and-close client still counts as demand.
                last_client_seen.store(now_millis(), Ordering::Relaxed);
                let Ok(write_half) = stream.try_clone() else { return };
                let client = Arc::new(Client { stream: write_half });
                if let Ok(mut guard) = clients.lock() {
                    guard.push(Arc::clone(&client));
                }
                log("client connected");
                // Read loop: answers snapshot requests and notices disconnects.
                loop {
                    match read_client_frame(&mut stream) {
                        Some((0x8, _)) | None => break,
                        Some((0x9, payload)) => {
                            let mut frame = vec![0x8a];
                            frame.push(payload.len() as u8);
                            frame.extend_from_slice(&payload);
                            let mut writer = &client.stream;
                            let _ = writer.write_all(&frame);
                        }
                        Some((0x1, payload)) => {
                            // Command from the applet (for example "load model").
                            let reply = handle_command(&payload, &engines, &procs);
                            let mut writer = &client.stream;
                            let _ = writer.write_all(&encode_text_frame(
                                reply.to_string().as_bytes(),
                            ));
                            let _ = writer.flush();
                        }
                        Some(_) => {}
                    }
                }
                if let Ok(mut guard) = clients.lock() {
                    guard.retain(|other| !Arc::ptr_eq(other, &client));
                }
                let _ = stream.shutdown(Shutdown::Both);
                log("client disconnected");
            })
            .ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n \
  0: 00000000:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0000000000000000 100 0 0 10 0\n \
  1: 0100007F:1F40 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0000000000000000 100 0 0 10 0\n \
  2: 00000000:1F40 9EB2A8C0:A1B2 01 00000000:00000000 00:00000000 00000000  1000        0 3 1 0000000000000000 100 0 0 10 0\n \
  3: 00000000:1F90 9EB2A8C0:A1B3 01 00000000:00000000 00:00000000 00000000  1000        0 4 1 0000000000000000 100 0 0 10 0\n";

    #[test]
    fn parses_established_clients_by_remote_address() {
        let rows = parse_tcp_table(TABLE, 8000, false);
        assert_eq!(rows, vec![("192.0.2.10".to_owned(), 1)]);
    }

    #[test]
    fn other_ports_and_states_are_ignored() {
        assert!(parse_tcp_table(TABLE, 9000, false).is_empty());
        let closed = TABLE.replace(" 01 ", " 06 ");
        assert!(parse_tcp_table(&closed, 8000, false).is_empty());
    }

    #[test]
    fn ipv6_addresses_are_decoded() {
        // Loopback in both representations must be filtered out. The kernel
        // stores each 32-bit word little-endian (verified against /proc/net/tcp6).
        let loopback = "  sl  local_address                         remote_address                        st\n \
  0: 00000000000000000000000001000000:1F40 0000000000000000FFFF00000100007F:1F40 01 00000000:00000000 00:00000000 00000000  1000        0 5 1 0000000000000000 100 0 0 10 0\n \
  1: 00000000000000000000000001000000:1F40 00000000000000000000000001000000:1F40 01 00000000:00000000 00:00000000 00000000  1000        0 6 1 0000000000000000 100 0 0 10 0\n";
        assert!(parse_tcp_table(loopback, 8000, true).is_empty());

        // fd00::1 arrives as 000000FD...01000000.
        let client = "  sl  local_address                         remote_address                        st\n \
  0: 00000000000000000000000001000000:1F40 000000FD000000000000000001000000:1F40 01 00000000:00000000 00:00000000 00000000  1000        0 7 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(
            parse_tcp_table(client, 8000, true),
            vec![("fd00:0:0:0:0:0:0:1".to_owned(), 1)]
        );
    }

    #[test]
    fn parses_stat_lines_with_tricky_command_names() {
        // Firefox-style names contain spaces, kernel threads parentheses.
        let line = "1234 (Web Content) S 1 1234 1234 0 -1 4194560 1 0 0 0 12 34 0 0 20 0 1 0";
        let (pid, name, state, ppid, utime, stime) = parse_stat_line(line).expect("stat line");
        assert_eq!(pid, 1234);
        assert_eq!(name, "Web Content");
        assert_eq!(state, "S");
        assert_eq!(ppid, 1);
        assert_eq!(utime, 12.0);
        assert_eq!(stime, 34.0);

        assert!(parse_stat_line("ohne klammern").is_none());
        assert!(parse_stat_line("1 (kurz) S 1").is_none());
    }

    #[test]
    fn parses_uid_and_rss_from_status() {
        let text = "Name:\tbash\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t  20480 kB\n";
        assert_eq!(parse_status(text), (Some(1000), 20480));
        assert_eq!(parse_status("Name:\tinit\n"), (None, 0));
    }

    #[test]
    fn kill_refusals_name_the_reason() {
        assert_eq!(
            kill_process(1, libc::SIGTERM as i64)["error"],
            json!("PID 1 kann nicht beendet werden")
        );
        assert_eq!(
            kill_process(i64::from(std::process::id()), libc::SIGTERM as i64)["error"],
            json!("der Agent kann sich nicht selbst beenden")
        );
        assert_eq!(
            kill_process(4242, libc::SIGUSR1 as i64)["error"],
            json!("nur SIGTERM (15) und SIGKILL (9) sind erlaubt")
        );
    }

    #[test]
    fn sends_sigterm_to_a_real_process() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .expect("sleep startet");
        let pid = i64::from(child.id());
        let reply = kill_process(pid, libc::SIGTERM as i64);
        assert_eq!(reply["ok"], json!(true), "{reply}");
        let status = child.wait().expect("sleep endet");
        assert!(!status.success(), "sleep muss durch das Signal enden");
    }

    #[test]
    fn samples_the_real_process_table_with_a_second_delta() {
        let mut sampler = ProcSampler::new();
        let first = sampler.sample(20).expect("erste Liste");
        assert_eq!(first["ok"], json!(true));
        assert!(first["total"].as_u64().unwrap_or(0) >= 1);
        let rows = first["processes"].as_array().expect("rows");
        assert!(!rows.is_empty() && rows.len() <= 20);
        // Without a previous scan every share is zero rather than invented.
        assert!(rows.iter().all(|row| row["cpu"].as_f64() == Some(0.0)));

        thread::sleep(Duration::from_millis(400));
        let second = sampler.sample(1000).expect("zweite Liste");
        let rows = second["processes"].as_array().expect("rows");
        let own = u64::from(std::process::id());
        let own_row = rows
            .iter()
            .find(|row| row["pid"].as_u64() == Some(own))
            .expect("eigener Prozess fehlt");
        assert!(own_row["name"].as_str().unwrap_or("").len() > 1);
        assert!(own_row["cmd"].as_str().unwrap_or("").len() > 1);
        assert!(own_row["user"].as_str().unwrap_or("").len() > 0);
        assert!(rows.iter().any(|row| row["cpu"].as_f64().unwrap_or(0.0) > 0.0));
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = serve(args) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
