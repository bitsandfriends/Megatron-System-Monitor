// SPDX-License-Identifier: GPL-3.0-only
//! Remote monitoring of the DGX Spark vLLM pair.
//!
//! Every Spark node runs the small WebSocket agent from `agent/` (see the
//! project README). This module keeps one connection per node in a background
//! thread, parses the pushed JSON documents and hands a plain snapshot to the
//! UI. The applet thread never performs I/O, so a slow or dead node cannot
//! stall the panel.
//!
//! All data in this module is owned, plain data: no socket, agent or service
//! object ever reaches the view code.

use std::collections::VecDeque;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

/// One hour of history at the agent's two-second push interval.
pub const SPARK_HISTORY_LEN: usize = 1800;

/// Default TCP port of the node agent.
pub const DEFAULT_AGENT_PORT: u16 = 8787;

const DEFAULT_TARGETS: [&str; 2] = ["node-01", "node-02"];
const READ_TIMEOUT: Duration = Duration::from_secs(8);
/// One refresh cycle: the agents push every two seconds.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// A cached snapshot younger than this is reused instead of fetching again.
const CACHE_TTL_S: f64 = 1.5;
/// A refresh lock older than this is considered abandoned.
const LOCK_STALE_S: u64 = 10;
/// Upper bound for one agent fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);
/// An agent that has not pushed within this window counts as offline.
const OFFLINE_AFTER_S: f32 = 12.0;
/// GPU temperature above which the popup shows a warning.
pub const GPU_TEMP_WARN_C: f32 = 80.0;

/// Where the applet looks for the Spark agents.
#[derive(Debug, Clone)]
pub struct SparkConfig {
    /// Configured targets, either `host` or `host:port`.
    pub targets: Vec<String>,
    /// Optional shared token for the agent handshake.
    pub token: Option<String>,
    /// `false` disables all remote monitoring.
    pub enabled: bool,
    /// Use the engines the agents discover on their own ports.
    pub use_discovered: bool,
    /// Share one snapshot between several panel instances through a cache file.
    pub shared_cache: bool,
    /// Draw the local CPU/RAM/GPU history charts in the popup.
    pub show_local_charts: bool,
    /// Friendly names for client addresses, `ip -> name`.
    pub client_names: Vec<(String, String)>,
    /// Whether the Spark section starts expanded in the popup.
    pub spark_section_open: bool,
}

impl Default for SparkConfig {
    fn default() -> Self {
        Self {
            targets: DEFAULT_TARGETS.iter().map(|host| (*host).to_owned()).collect(),
            token: None,
            enabled: true,
            use_discovered: true,
            shared_cache: true,
            show_local_charts: true,
            client_names: Vec::new(),
            spark_section_open: true,
        }
    }
}

impl SparkConfig {
    /// Reads the configuration from the environment or the config file.
    ///
    /// Order: `SYSMON_SPARK_AGENTS` (comma separated, `off` disables) wins,
    /// then `~/.config/cosmic-applet-sysmon/spark.json`, then the default
    /// `node-01, node-02`. The token is never taken from the environment.
    pub fn load() -> Self {
        let mut config = Self::default();

        if let Some(path) = config_path() {
            if let Ok(text) = fs::read_to_string(&path) {
                match serde_json::from_str::<Value>(&text) {
                    Ok(value) => config.apply_file(&value),
                    Err(error) => tracing::warn!("{path:?}: {error}"),
                }
            }
        }

        if let Ok(value) = std::env::var("SYSMON_SPARK_AGENTS") {
            let trimmed = value.trim();
            if trimmed.eq_ignore_ascii_case("off")
                || trimmed.eq_ignore_ascii_case("none")
                || trimmed.is_empty()
            {
                config.enabled = false;
            } else {
                config.targets = trimmed
                    .split(',')
                    .map(|item| item.trim().to_owned())
                    .filter(|item| !item.is_empty())
                    .collect();
                config.enabled = !config.targets.is_empty();
            }
        }

        config
    }

    fn apply_file(&mut self, value: &Value) {
        if let Some(enabled) = value.get("enabled").and_then(Value::as_bool) {
            self.enabled = enabled;
        }
        if let Some(token) = value.get("token").and_then(Value::as_str) {
            let token = token.trim();
            self.token = if token.is_empty() {
                None
            } else {
                Some(token.to_owned())
            };
        }
        if let Some(use_discovered) = value.get("use_discovered").and_then(Value::as_bool) {
            self.use_discovered = use_discovered;
        }
        if let Some(shared_cache) = value.get("shared_cache").and_then(Value::as_bool) {
            self.shared_cache = shared_cache;
        }
        if let Some(show_local_charts) = value.get("show_local_charts").and_then(Value::as_bool) {
            self.show_local_charts = show_local_charts;
        }
        if let Some(open) = value.get("spark_section_open").and_then(Value::as_bool) {
            self.spark_section_open = open;
        }
        if let Some(names) = value.get("client_names").and_then(Value::as_object) {
            self.client_names = names
                .iter()
                .filter_map(|(ip, name)| name.as_str().map(|name| (ip.clone(), name.to_owned())))
                .collect();
        }
        if let Some(agents) = value.get("agents").and_then(Value::as_array) {
            let targets: Vec<String> = agents
                .iter()
                .filter_map(Value::as_str)
                .map(|item| item.trim().to_owned())
                .filter(|item| !item.is_empty())
                .collect();
            if !targets.is_empty() {
                self.targets = targets;
            }
        }
    }
}

impl SparkConfig {
    /// Writes the configuration back, so the settings view survives a restart.
    pub fn save(&self) -> std::io::Result<PathBuf> {
        let Some(path) = config_path() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no config path"));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let document = serde_json::json!({
            "enabled": self.enabled,
            "use_discovered": self.use_discovered,
            "shared_cache": self.shared_cache,
            "show_local_charts": self.show_local_charts,
            "spark_section_open": self.spark_section_open,
            "client_names": self
                .client_names
                .iter()
                .map(|(ip, name)| (ip.clone(), Value::String(name.clone())))
                .collect::<serde_json::Map<String, Value>>(),
            "agents": self.targets,
            "token": self.token.clone().unwrap_or_default(),
        });
        fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&document)?))?;
        Ok(path)
    }
}

/// `$XDG_CONFIG_HOME/cosmic-applet-sysmon/spark.json`, else `~/.config/...`.
fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("cosmic-applet-sysmon").join("spark.json"))
}

/// Node values as pushed by one agent.
#[derive(Debug, Clone, Default)]
pub struct NodeMetrics {
    pub cpu_pct: f32,
    pub cpu_count: u32,
    pub load1: f32,
    pub mem_used_gib: f32,
    pub mem_total_gib: f32,
    pub mem_pct: f32,
    pub swap_used_gib: f32,
    pub swap_total_gib: f32,
    pub gpu_name: String,
    pub gpu_available: bool,
    pub gpu_util_pct: f32,
    pub gpu_power_w: f32,
    pub gpu_temp_c: f32,
}

/// vLLM engine values; the agent that hosts the metrics endpoint reports them.
#[derive(Debug, Clone, Default)]
pub struct EngineMetrics {
    pub available: bool,
    pub error: Option<String>,
    /// Machine readable reason when unavailable: `pending`, `disabled`,
    /// `no-endpoint`, `timeout` or `error`.
    pub reason: String,
    pub model: String,
    pub running: u32,
    pub waiting: u32,
    pub kv_cache_pct: f32,
    pub gen_tok_s: f32,
    pub prompt_tok_s: f32,
    pub gen_tokens_total: u64,
    pub prompt_tokens_total: u64,
    pub ttft_ms: f32,
    pub spec_accept_pct: Option<f32>,
    pub preemptions_total: u64,
}

/// One LLM server the node agent found on its own ports.
#[derive(Debug, Clone, Default)]
pub struct EngineInfo {
    pub port: u16,
    /// `vllm`, `openai` or `ollama`.
    pub kind: String,
    pub endpoint: String,
    /// Models the engine currently serves.
    pub models: Vec<String>,
    /// Number of models the engine offers.
    pub model_count: usize,
    /// Models currently loaded in memory (Ollama: `/api/ps`).
    pub loaded: Vec<String>,
    /// Whether the engine publishes metrics the agent can scrape.
    pub has_metrics: bool,
    pub primary: bool,
}

impl EngineInfo {
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind, self.port)
    }
}

/// One client address connected to the node's engine.
#[derive(Debug, Clone, Default)]
pub struct ClientConnection {
    pub ip: String,
    pub count: u32,
}

/// One monitored node as seen by the UI.
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    /// Configured target, e.g. `node-01:8787`.
    pub label: String,
    /// Hostname reported by the agent.
    pub host: String,
    pub online: bool,
    /// Seconds since the last pushed document.
    pub age_s: f32,
    /// Reason for the last connection failure, empty while online.
    pub status: String,
    pub uptime_s: f32,
    pub node: NodeMetrics,
    pub engine: EngineMetrics,
    /// Agent version the node reported.
    pub version: String,
    /// Processor name of the node.
    pub cpu_model: String,
    /// GPU name and VRAM values of the node.
    pub gpu_name: String,
    pub gpu_vram_used_mib: f64,
    pub gpu_vram_total_mib: f64,
    /// Engines discovered on this node, most useful first.
    pub engines: Vec<EngineInfo>,
    /// Established connections to the engine, grouped by remote address.
    pub clients: Vec<ClientConnection>,
    /// Port the client connections belong to.
    pub client_port: u16,
}

/// Everything the popup needs; cheap to clone once per second.
#[derive(Debug, Clone, Default)]
pub struct SparkSnapshot {
    pub enabled: bool,
    pub agents: Vec<AgentState>,
    /// Decode token rate in tokens per second.
    pub gen_tok_s: Vec<f32>,
    /// Prefill token rate in tokens per second.
    pub prompt_tok_s: Vec<f32>,
    /// KV-cache usage in percent.
    pub kv_pct: Vec<f32>,
    /// Observed push interval of the engine samples in seconds.
    #[allow(dead_code)]
    pub interval_s: f32,
}

impl AgentState {
    /// Hostname when the agent reported one, otherwise the configured target.
    pub fn display_name(&self) -> String {
        if !self.host.is_empty() {
            return self.host.clone();
        }
        // "desktop-01:8787" reads better as "desktop-01".
        match self.label.split_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name.to_owned(),
            _ => self.label.clone(),
        }
    }
}

impl SparkSnapshot {
    pub fn online_count(&self) -> usize {
        self.agents.iter().filter(|agent| agent.online).count()
    }

    /// The agent that currently publishes engine values.
    #[allow(dead_code)]
    pub fn engine_agent(&self) -> Option<&AgentState> {
        self.agents.iter().find(|agent| agent.engine.available)
    }

    /// First engine error, used when no agent publishes engine values.
    #[allow(dead_code)]
    pub fn engine_error(&self) -> Option<&str> {
        self.agents
            .iter()
            .filter_map(|agent| agent.engine.error.as_deref())
            .find(|error| !error.is_empty())
    }

    /// Measured GPU power of every reachable node in watts.
    pub fn total_gpu_watts(&self) -> f32 {
        self.agents
            .iter()
            .filter(|agent| agent.online && agent.node.gpu_available)
            .map(|agent| agent.node.gpu_power_w)
            .sum()
    }

    /// Every engine reported by a reachable node, primary engines first.
    pub fn engines(&self) -> Vec<(&AgentState, &EngineInfo)> {
        let mut rows: Vec<(&AgentState, &EngineInfo)> = self
            .agents
            .iter()
            .filter(|agent| agent.online)
            .flat_map(|agent| agent.engines.iter().map(move |engine| (agent, engine)))
            .collect();
        rows.sort_by_key(|(_, engine)| !engine.primary);
        rows
    }

    /// Number of engines discovered across all reachable nodes.
    #[allow(dead_code)]
    pub fn engine_count(&self) -> usize {
        self.engines().len()
    }

    /// Unique model names the discovered engines serve.
    #[allow(dead_code)]
    pub fn models(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .engines()
            .into_iter()
            .flat_map(|(_, engine)| engine.models.iter().cloned())
            .filter(|name| !name.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// One readable line per discovered engine.
    #[allow(dead_code)]
    pub fn engine_lines(&self) -> Vec<String> {
        self.engines()
            .into_iter()
            .map(|(agent, engine)| {
                let models = if engine.models.is_empty() {
                    "Modelle unbekannt".to_owned()
                } else if engine.models.len() == 1 {
                    engine.models[0].clone()
                } else {
                    format!("{} Modelle", engine.models.len())
                };
                format!(
                    "{}: {} ({}{})",
                    agent.display_name(),
                    engine.label(),
                    models,
                    if engine.has_metrics { ", Metriken" } else { "" }
                )
            })
            .collect()
    }

    /// Like `engine_lines`, but with the full endpoint for the settings page.
    pub fn engine_details(&self) -> Vec<String> {
        self.engines()
            .into_iter()
            .map(|(agent, engine)| {
                let models = if engine.models.is_empty() {
                    "Modelle unbekannt".to_owned()
                } else {
                    engine.models.join(", ")
                };
                format!(
                    "{}: {} · {} · {}{}",
                    agent.display_name(),
                    engine.endpoint,
                    engine.label(),
                    models,
                    if engine.has_metrics { " · Metriken" } else { "" }
                )
            })
            .collect()
    }

    /// Nodes whose agent version differs from the one this applet ships.
    ///
    /// Returns `(name, target, reported version, needs update)`; `needs_update`
    /// is false when the agent is newer than the applet.
    pub fn version_mismatches(&self) -> Vec<(String, String, String, bool)> {
        self.agents
            .iter()
            .filter(|agent| agent.online && !agent.version.trim().is_empty())
            .filter_map(|agent| {
                let state = crate::install::agent_version_state(&agent.version);
                match state {
                    crate::install::AgentVersionState::Outdated => Some((
                        agent.display_name(),
                        agent.label.clone(),
                        agent.version.clone(),
                        true,
                    )),
                    crate::install::AgentVersionState::AppletOutdated => Some((
                        agent.display_name(),
                        agent.label.clone(),
                        agent.version.clone(),
                        false,
                    )),
                    _ => None,
                }
            })
            .collect()
    }

    /// Nodes that should be updated, most recently seen first.
    pub fn outdated_agents(&self) -> Vec<(String, String, String)> {
        self.version_mismatches()
            .into_iter()
            .filter(|(_, _, _, needs_update)| *needs_update)
            .map(|(name, target, version, _)| (name, target, version))
            .collect()
    }

    /// Total number of established engine connections across all nodes.
    pub fn client_total(&self) -> u32 {
        self.agents
            .iter()
            .filter(|agent| agent.online)
            .flat_map(|agent| agent.clients.iter())
            .map(|client| client.count)
            .sum()
    }

    /// Threshold warnings for the popup, most severe first.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(agent) = self.engine_agent() {
            if agent.engine.preemptions_total > 0 {
                warnings.push(format!(
                    "{} verdrängte Anfragen (Präemptionen)",
                    agent.engine.preemptions_total
                ));
            }
            if agent.engine.waiting > 0 {
                warnings.push(format!("{} Anfrage(n) warten in der Schlange", agent.engine.waiting));
            }
        }
        for agent in &self.agents {
            if self.node_is_hot(agent) {
                warnings.push(format!(
                    "{}: GPU {:.0} °C",
                    agent.display_name(),
                    agent.node.gpu_temp_c
                ));
            }
        }
        warnings
    }

    /// Whether this node's GPU is above the warning threshold.
    pub fn node_is_hot(&self, agent: &AgentState) -> bool {
        agent.online && agent.node.gpu_available && agent.node.gpu_temp_c >= GPU_TEMP_WARN_C
    }

    /// GPU nodes that reported a value.
    pub fn gpu_node_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|agent| agent.online && agent.node.gpu_available)
            .count()
    }
}

/// One agent connection plus the time of its last document.
struct AgentSlot {
    state: AgentState,
    last_rx: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    agents: Vec<AgentSlot>,
    gen_hist: VecDeque<f32>,
    prompt_hist: VecDeque<f32>,
    kv_hist: VecDeque<f32>,
    interval_s: f32,
    last_engine_rx: Option<Instant>,
}

/// Owns the background threads that talk to the node agents.
pub struct SparkMonitor {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    enabled: bool,
}

impl SparkMonitor {
    /// Starts one connection thread per configured node.
    pub fn start(config: SparkConfig) -> Self {
        let agents = config
            .targets
            .iter()
            .map(|target| AgentSlot {
                state: AgentState {
                    label: target.clone(),
                    ..AgentState::default()
                },
                last_rx: None,
            })
            .collect();

        let inner = Arc::new(Mutex::new(Inner {
            agents,
            ..Inner::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));

        if config.enabled && !config.targets.is_empty() {
            let targets = config.targets.clone();
            let token = config.token.clone();
            let shared_cache = config.shared_cache;
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("sysmon-spark".to_owned())
                .spawn(move || refresh_loop(targets, token, shared_cache, inner, stop))
                .ok();
            tracing::info!(
                "spark monitor: watching {:?} (shared cache: {shared_cache})",
                config.targets
            );
        } else if config.enabled {
            tracing::info!("spark monitor: no nodes configured");
        } else {
            tracing::info!("spark monitor: disabled by configuration");
        }

        Self {
            inner,
            stop,
            enabled: config.enabled,
        }
    }

    /// Copies the current state for the view; never blocks on the network.
    pub fn snapshot(&self) -> SparkSnapshot {
        let Ok(inner) = self.inner.lock() else {
            return SparkSnapshot::default();
        };

        let agents: Vec<AgentState> = inner
            .agents
            .iter()
            .map(|slot| {
                let mut state = slot.state.clone();
                state.age_s = match slot.last_rx {
                    Some(at) => at.elapsed().as_secs_f32(),
                    None => f32::MAX,
                };
                state.online = state.age_s < OFFLINE_AFTER_S;
                state
            })
            .collect();

        SparkSnapshot {
            enabled: self.enabled,
            agents,
            gen_tok_s: inner.gen_hist.iter().copied().collect(),
            prompt_tok_s: inner.prompt_hist.iter().copied().collect(),
            kv_pct: inner.kv_hist.iter().copied().collect(),
            interval_s: inner.interval_s,
        }
    }
}

impl Drop for SparkMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Keeps the node documents up to date and stores them in the shared cache.
///
/// Every panel instance runs this loop. Without the shared cache each instance
/// fetches on its own; with it (the default) only the instance that wins the
/// refresh lock fetches, and the others read the cache file. That keeps two
/// panel outputs from doubling the work and gives both the same values.
fn refresh_loop(
    targets: Vec<String>,
    token: Option<String>,
    shared_cache: bool,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
) {
    let cache = cache_path();
    let lock_path = lock_path();
    let mut last_ts = vec![0.0_f64; targets.len()];

    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        let cached = read_cache(&cache);

        let documents = match cached {
            Some((timestamp, documents)) if shared_cache && epoch_seconds() - timestamp < CACHE_TTL_S => {
                Some(documents)
            }
            cached_documents => {
                let may_fetch = !shared_cache || acquire_lock(&lock_path);
                if may_fetch {
                    let fetched: Vec<Result<String, String>> = targets
                        .iter()
                        .map(|target| fetch_document(target, token.as_deref()))
                        .collect();
                    if shared_cache {
                        write_cache(&cache, &targets, &fetched);
                        release_lock(&lock_path);
                    }
                    Some(fetched)
                } else {
                    cached_documents.map(|(_, documents)| documents)
                }
            }
        };

        if let Some(documents) = documents {
            for (index, document) in documents.iter().enumerate() {
                match document {
                    Ok(text) => {
                        // Parse once: the timestamp check and the update share it.
                        let Ok(value) = serde_json::from_str::<Value>(text) else {
                            tracing::warn!("spark agent sent invalid JSON");
                            continue;
                        };
                        // The cache is read more often than it is refreshed;
                        // identical documents must not extend the history.
                        let stamp = value.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
                        if stamp != 0.0 && stamp == last_ts.get(index).copied().unwrap_or(0.0) {
                            continue;
                        }
                        apply_value(&inner, index, &value);
                        if let Some(slot) = last_ts.get_mut(index) {
                            *slot = stamp;
                        }
                    }
                    Err(error) => mark_unreachable(&inner, index, error),
                }
            }
        }

        sleep_in_steps(&stop, REFRESH_INTERVAL.saturating_sub(started.elapsed()));
    }
}

/// Opens a short connection, reads the document the agent pushes on connect and
/// closes again. Nothing is kept open between refreshes.
fn fetch_document(target: &str, token: Option<&str>) -> Result<String, String> {
    let url = agent_url(target, token);
    let (mut socket, _) = tungstenite::connect(url.as_str())
        .map_err(|error| short_error(&error.to_string()))?;
    set_read_timeout(&mut socket);
    let deadline = Instant::now() + FETCH_TIMEOUT;
    while Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let document = text.as_str().to_owned();
                let _ = socket.close(None);
                return Ok(document);
            }
            Ok(Message::Close(_)) => return Err("Verbindung geschlossen".to_owned()),
            Ok(_) => {}
            Err(error) => return Err(short_error(&error.to_string())),
        }
    }
    Err("Zeitüberschreitung".to_owned())
}

/// Asks an agent to load a model into its engine (Ollama).
///
/// The agent answers over the same connection; the pushed snapshots in between
/// are skipped until the reply with the matching action arrives.
pub fn load_model(target: &str, token: Option<&str>, port: u16, model: &str) -> Result<String, String> {
    let url = agent_url(target, token);
    let (mut socket, _) = tungstenite::connect(url.as_str())
        .map_err(|error| short_error(&error.to_string()))?;
    socket
        .send(Message::Text(
            serde_json::json!({"action": "load_model", "port": port, "model": model})
                .to_string()
                .into(),
        ))
        .map_err(|error| short_error(&error.to_string()))?;

    // Loading reads the model from disk; that can take a while.
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if value.get("action").and_then(Value::as_str) != Some("load_model") {
                    continue;
                }
                let _ = socket.close(None);
                if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    let loaded = value
                        .get("loaded")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str())
                                .collect::<Vec<&str>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    return Ok(if loaded.is_empty() {
                        format!("{model} geladen")
                    } else {
                        format!("{model} geladen (aktiv: {loaded})")
                    });
                }
                return Err(value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("der Agent hat das Laden abgelehnt")
                    .to_owned());
            }
            Ok(Message::Close(_)) => return Err("Verbindung geschlossen".to_owned()),
            Ok(_) => {}
            Err(error) => return Err(short_error(&error.to_string())),
        }
    }
    Err("Zeitüberschreitung beim Laden".to_owned())
}

/// Progress of one model load, read by the popup.
#[derive(Debug, Clone, Default)]
pub struct LoadState {
    pub running: bool,
    pub message: String,
    pub failed: bool,
}

/// Loads one model in a background thread.
pub struct ModelLoader {
    state: Arc<Mutex<LoadState>>,
}

impl ModelLoader {
    pub fn start(target: String, token: Option<String>, port: u16, model: String) -> Self {
        let state = Arc::new(Mutex::new(LoadState {
            running: true,
            message: format!("Lade {model} …"),
            failed: false,
        }));
        let thread_state = Arc::clone(&state);
        thread::Builder::new()
            .name("sysmon-load".to_owned())
            .spawn(move || {
                let result = load_model(&target, token.as_deref(), port, &model);
                if let Ok(mut guard) = thread_state.lock() {
                    guard.running = false;
                    match result {
                        Ok(message) => {
                            guard.message = message;
                            guard.failed = false;
                        }
                        Err(error) => {
                            guard.message = format!("Fehlgeschlagen: {error}");
                            guard.failed = true;
                        }
                    }
                }
            })
            .ok();
        Self { state }
    }

    pub fn snapshot(&self) -> LoadState {
        self.state.lock().map(|guard| guard.clone()).unwrap_or_default()
    }
}

fn mark_unreachable(inner: &Arc<Mutex<Inner>>, index: usize, error: &str) {
    let mut guard = lock(inner);
    if let Some(slot) = guard.agents.get_mut(index) {
        slot.state.status = error.to_owned();
        slot.state.engine.available = false;
    }
}

fn sleep_in_steps(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

/// `$XDG_RUNTIME_DIR/cosmic-applet-sysmon`, else a directory under `/tmp`.
pub fn runtime_directory() -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => std::env::temp_dir(),
    };
    base.join("cosmic-applet-sysmon")
}

fn cache_path() -> PathBuf {
    runtime_directory().join("spark-cache.json")
}

fn lock_path() -> PathBuf {
    runtime_directory().join("spark-refresh.lock")
}

fn read_cache(path: &std::path::Path) -> Option<(f64, Vec<Result<String, String>>)> {
    let text = fs::read_to_string(path).ok()?;
    let document: Value = serde_json::from_str(&text).ok()?;
    let timestamp = document.get("ts").and_then(Value::as_f64)?;
    let entries = document.get("documents").and_then(Value::as_array)?;
    let documents = entries
        .iter()
        .map(|entry| match entry.get("document").and_then(Value::as_str) {
            Some(document) => Ok(document.to_owned()),
            None => Err(entry
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("nicht erreichbar")
                .to_owned()),
        })
        .collect();
    Some((timestamp, documents))
}

fn write_cache(path: &std::path::Path, targets: &[String], documents: &[Result<String, String>]) {
    let entries: Vec<Value> = targets
        .iter()
        .enumerate()
        .map(|(index, target)| match documents.get(index) {
            Some(Ok(document)) => serde_json::json!({ "target": target, "document": document }),
            Some(Err(error)) => serde_json::json!({ "target": target, "error": error }),
            None => serde_json::json!({ "target": target }),
        })
        .collect();
    let payload = serde_json::json!({
        "ts": epoch_seconds(),
        "documents": entries,
    });
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, payload.to_string());
}

/// Takes the refresh lock; a lock older than `LOCK_STALE_S` is reclaimed.
fn acquire_lock(path: &std::path::Path) -> bool {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let _ = file.write_all(format!("{}", std::process::id()).as_bytes());
            true
        }
        Err(_) => {
            let stale = fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .map(|age| age.as_secs() > LOCK_STALE_S)
                .unwrap_or(false);
            if stale {
                let _ = fs::remove_file(path);
                return fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .is_ok();
            }
            false
        }
    }
}

fn release_lock(path: &std::path::Path) {
    let _ = fs::remove_file(path);
}

fn lock(inner: &Arc<Mutex<Inner>>) -> std::sync::MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Parses one pushed document and updates the shared state (test entry point).
#[cfg(test)]
fn apply_document(inner: &Arc<Mutex<Inner>>, index: usize, text: &str) {
    match serde_json::from_str::<Value>(text) {
        Ok(document) => apply_value(inner, index, &document),
        Err(_) => tracing::warn!("spark agent sent invalid JSON ({} bytes)", text.len()),
    }
}

/// Applies an already parsed document.
fn apply_value(inner: &Arc<Mutex<Inner>>, index: usize, document: &Value) {

    let now = Instant::now();
    let mut guard = lock(inner);

    let (engine_available, gen_rate, prompt_rate, kv_pct) = {
        let Some(slot) = guard.agents.get_mut(index) else {
            return;
        };

        slot.last_rx = Some(now);
        slot.state.host = text_at(&document, &["host"]).unwrap_or_default().to_owned();
        slot.state.uptime_s = number_at(&document, &["uptime_s"]);

        let node = &mut slot.state.node;
        node.cpu_pct = number_at(&document, &["node", "cpu_pct"]);
        node.cpu_count = number_at(&document, &["node", "cpu_count"]) as u32;
        node.load1 = number_at(&document, &["node", "load1"]);
        node.mem_used_gib = number_at(&document, &["node", "mem_used_gib"]);
        node.mem_total_gib = number_at(&document, &["node", "mem_total_gib"]);
        node.mem_pct = number_at(&document, &["node", "mem_pct"]);
        node.swap_used_gib = number_at(&document, &["node", "swap_used_gib"]);
        node.swap_total_gib = number_at(&document, &["node", "swap_total_gib"]);
        node.gpu_available = flag_at(&document, &["gpu", "available"]);
        node.gpu_name = text_at(&document, &["gpu", "name"]).unwrap_or_default().to_owned();
        node.gpu_util_pct = number_at(&document, &["gpu", "util_pct"]);
        node.gpu_power_w = number_at(&document, &["gpu", "power_w"]);
        node.gpu_temp_c = number_at(&document, &["gpu", "temp_c"]);

        let engine = &mut slot.state.engine;
        engine.available = flag_at(&document, &["vllm", "available"]);
        engine.error = text_at(&document, &["vllm", "error"])
            .filter(|error| !error.is_empty())
            .map(str::to_owned);
        engine.reason = text_at(&document, &["vllm", "reason"]).unwrap_or_default().to_owned();

        slot.state.cpu_model = value_at(&document, &["node", "cpu_model"])
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        slot.state.gpu_name = value_at(&document, &["gpu", "name"])
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        slot.state.gpu_vram_used_mib = value_at(&document, &["gpu", "vram_used_mib"])
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        slot.state.gpu_vram_total_mib = value_at(&document, &["gpu", "vram_total_mib"])
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        slot.state.version = value_at(&document, &["version"])
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        slot.state.client_port = value_at(&document, &["clients", "port"])
            .and_then(Value::as_u64)
            .unwrap_or(0) as u16;
        slot.state.clients = value_at(&document, &["clients", "connections"])
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let ip = item.get("ip").and_then(Value::as_str)?;
                        Some(ClientConnection {
                            ip: ip.to_owned(),
                            count: item.get("count").and_then(Value::as_u64).unwrap_or(0) as u32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        slot.state.engines = value_at(&document, &["llm", "engines"])
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| EngineInfo {
                        port: item.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
                        kind: item.get("kind").and_then(Value::as_str).unwrap_or("llm").to_owned(),
                        endpoint: item.get("endpoint").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        models: item
                            .get("models")
                            .and_then(Value::as_array)
                            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                            .unwrap_or_default(),
                        model_count: item
                            .get("model_count")
                            .and_then(Value::as_u64)
                            .map(|value| value as usize)
                            .unwrap_or_else(|| {
                                item.get("models")
                                    .and_then(Value::as_array)
                                    .map(|list| list.len())
                                    .unwrap_or(0)
                            }),
                        loaded: item
                            .get("loaded")
                            .and_then(Value::as_array)
                            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                            .unwrap_or_default(),
                        has_metrics: item
                            .get("metrics")
                            .and_then(Value::as_str)
                            .map(|metrics| !metrics.is_empty())
                            .unwrap_or(false),
                        primary: item.get("primary").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        engine.model = text_at(&document, &["vllm", "model"]).unwrap_or_default().to_owned();
        engine.running = number_at(&document, &["vllm", "running"]) as u32;
        engine.waiting = number_at(&document, &["vllm", "waiting"]) as u32;
        engine.kv_cache_pct = number_at(&document, &["vllm", "kv_cache_pct"]);
        engine.gen_tok_s = number_at(&document, &["vllm", "gen_tok_s"]);
        engine.prompt_tok_s = number_at(&document, &["vllm", "prompt_tok_s"]);
        engine.gen_tokens_total = number_at(&document, &["vllm", "gen_tokens_total"]) as u64;
        engine.prompt_tokens_total = number_at(&document, &["vllm", "prompt_tokens_total"]) as u64;
        engine.ttft_ms = number_at(&document, &["vllm", "ttft_ms"]);
        engine.preemptions_total = number_at(&document, &["vllm", "preemptions_total"]) as u64;
        engine.spec_accept_pct = optional_number_at(&document, &["vllm", "spec_accept_pct"]);

        (
            engine.available,
            engine.gen_tok_s,
            engine.prompt_tok_s,
            engine.kv_cache_pct,
        )
    };

    if engine_available {
        if let Some(previous) = guard.last_engine_rx.replace(now) {
            let seconds = now.duration_since(previous).as_secs_f32();
            if (0.2..30.0).contains(&seconds) {
                guard.interval_s = if guard.interval_s <= 0.0 {
                    seconds
                } else {
                    guard.interval_s * 0.7 + seconds * 0.3
                };
            }
        }
        push(&mut guard.gen_hist, gen_rate);
        push(&mut guard.prompt_hist, prompt_rate);
        push(&mut guard.kv_hist, kv_pct);
    }
}

fn push(history: &mut VecDeque<f32>, value: f32) {
    if history.len() >= SPARK_HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(value);
}

/// Builds the WebSocket URL for one configured target.
pub(crate) fn agent_url(target: &str, token: Option<&str>) -> String {
    let mut base = if target.starts_with("ws://") || target.starts_with("wss://") {
        target.to_owned()
    } else if target.contains(':') {
        format!("ws://{target}")
    } else {
        format!("ws://{target}:{DEFAULT_AGENT_PORT}")
    };
    if !base.ends_with('/') {
        base.push('/');
    }
    match token {
        Some(token) if !token.is_empty() => format!("{base}?token={token}"),
        _ => base,
    }
}

/// Short, human readable connection error for the popup.
pub(crate) fn short_error(message: &str) -> String {
    let message = message.trim();
    let message = message
        .strip_prefix("IO error: ")
        .or_else(|| message.strip_prefix("Url error: "))
        .unwrap_or(message);
    if message.chars().count() > 90 {
        let cut: String = message.chars().take(90).collect();
        format!("{cut}…")
    } else {
        message.to_owned()
    }
}

fn set_read_timeout(socket: &mut tungstenite::WebSocket<MaybeTlsStream<std::net::TcpStream>>) {
    #[allow(unreachable_patterns, irrefutable_let_patterns)]
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    }
}

fn value_at<'a>(document: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = document;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn number_at(document: &Value, path: &[&str]) -> f32 {
    optional_number_at(document, path).unwrap_or(0.0)
}

fn optional_number_at(document: &Value, path: &[&str]) -> Option<f32> {
    let value = value_at(document, path)?;
    value
        .as_f64()
        .map(|number| number as f32)
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn text_at<'a>(document: &'a Value, path: &[&str]) -> Option<&'a str> {
    value_at(document, path)?.as_str()
}

fn flag_at(document: &Value, path: &[&str]) -> bool {
    value_at(document, path)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = r#"{
        "agent": "megatron-sysmon", "version": "1.0", "host": "node-01",
        "ts": 1789849808.175, "uptime_s": 182087.5,
        "node": {"cpu_pct": 12.5, "cpu_count": 20, "load1": 0.06, "load5": 0.31, "load15": 0.38,
                 "mem_total_gib": 121.63, "mem_used_gib": 111.93, "mem_avail_gib": 9.7,
                 "mem_pct": 92.0, "swap_total_gib": 16.0, "swap_used_gib": 7.55},
        "gpu": {"available": true, "name": "NVIDIA GB10", "util_pct": 42.0, "mem_util_pct": 0.0,
                "temp_c": 61.0, "power_w": 118.42, "sm_mhz": 2411.0},
        "clients": {"port": 8000, "connections": [{"ip": "192.0.2.10", "count": 2}]},
        "llm": {"autodiscover": true, "endpoint": "http://127.0.0.1:8000/metrics",
                "engines": [{"port": 8000, "kind": "vllm", "endpoint": "http://127.0.0.1:8000",
                             "metrics": "http://127.0.0.1:8000/metrics", "primary": true,
                             "models": ["unsloth/Qwen3.8-Flash-Next-FP8", "Qwen3-Embedding"]}]},
        "vllm": {"available": true, "error": null, "model": "unsloth/Qwen3.8-Flash-Next-FP8",
                 "running": 3, "waiting": 1, "kv_cache_pct": 7.03, "gen_tokens_total": 18328,
                 "prompt_tokens_total": 48001, "preemptions_total": 2, "ttft_ms": 268.2,
                 "gen_tok_s": 58.4, "prompt_tok_s": 12.5, "spec_accept_pct": 66.2}
    }"#;

    const HEADLESS_DOCUMENT: &str = r#"{
        "host": "node-02", "uptime_s": 69592.9,
        "node": {"cpu_pct": 0.2, "cpu_count": 20, "mem_total_gib": 121.63, "mem_used_gib": 113.05,
                 "mem_pct": 92.9, "swap_total_gib": 16.0, "swap_used_gib": 3.42},
        "gpu": {"available": true, "name": "NVIDIA GB10", "power_w": 10.37, "temp_c": 46.0},
        "vllm": {"available": false, "reason": "no-endpoint",
                 "error": "kein lokaler vLLM-Endpunkt (headless Rank?)"}
    }"#;

    fn monitor_with(targets: &[&str]) -> (SparkMonitor, SparkConfig) {
        let config = SparkConfig {
            targets: targets.iter().map(|item| (*item).to_owned()).collect(),
            token: None,
            enabled: false,
            use_discovered: true,
            shared_cache: true,
            show_local_charts: true,
            client_names: Vec::new(),
            spark_section_open: true,
        };
        (SparkMonitor::start(config.clone()), config)
    }

    #[test]
    fn parses_a_full_document() {
        let (monitor, _config) = monitor_with(&["node-01"]);
        apply_document(&monitor.inner, 0, DOCUMENT);

        let snapshot = monitor.snapshot();
        let agent = &snapshot.agents[0];
        assert!(agent.online);
        assert_eq!(agent.host, "node-01");
        assert_eq!(agent.node.cpu_count, 20);
        assert!((agent.node.mem_pct - 92.0).abs() < 0.01);
        assert!((agent.node.gpu_power_w - 118.42).abs() < 0.01);
        assert!(agent.node.gpu_available);

        let engine = &agent.engine;
        assert!(engine.available);
        assert_eq!(engine.model, "unsloth/Qwen3.8-Flash-Next-FP8");
        assert_eq!(engine.running, 3);
        assert_eq!(engine.waiting, 1);
        assert!((engine.kv_cache_pct - 7.03).abs() < 0.01);
        assert!((engine.gen_tok_s - 58.4).abs() < 0.01);
        assert_eq!(engine.spec_accept_pct, Some(66.2));
        assert_eq!(engine.preemptions_total, 2);

        assert_eq!(snapshot.gen_tok_s, vec![58.4]);
        assert_eq!(snapshot.kv_pct, vec![7.03]);
        assert_eq!(snapshot.online_count(), 1);
        assert_eq!(snapshot.gpu_node_count(), 1);
        assert!((snapshot.total_gpu_watts() - 118.42).abs() < 0.01);
    }

    #[test]
    fn headless_node_is_online_without_engine_values() {
        let (monitor, _config) = monitor_with(&["node-01", "node-02"]);
        apply_document(&monitor.inner, 0, DOCUMENT);
        apply_document(&monitor.inner, 1, HEADLESS_DOCUMENT);

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.online_count(), 2);
        let engine_agent = snapshot.engine_agent().expect("engine agent");
        assert_eq!(engine_agent.host, "node-01");
        assert!(snapshot.engine_error().is_some());
        assert_eq!(snapshot.gen_tok_s.len(), 1, "history follows the engine only");
        assert!((snapshot.total_gpu_watts() - 128.79).abs() < 0.01);
    }

    #[test]
    fn offline_agents_report_age_and_keep_the_last_values() {
        let (monitor, _config) = monitor_with(&["node-01"]);
        apply_document(&monitor.inner, 0, DOCUMENT);

        let snapshot = monitor.snapshot();
        assert!(snapshot.agents[0].online);
        assert!(snapshot.agents[0].age_s < 1.0);
    }

    #[test]
    fn discovered_engines_are_parsed() {
        let (monitor, _config) = monitor_with(&["node-01"]);
        apply_document(&monitor.inner, 0, DOCUMENT);

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.engine_count(), 1);
        assert_eq!(snapshot.client_total(), 2);
        assert_eq!(snapshot.agents[0].version, "1.0");
        // 1.0 is older than the shipped agent, so an update is offered.
        assert_eq!(snapshot.outdated_agents().len(), 1);
        assert_eq!(snapshot.agents[0].client_port, 8000);
        assert_eq!(snapshot.agents[0].clients[0].ip, "192.0.2.10");
        assert_eq!(snapshot.agents[0].engines[0].kind, "vllm");
        assert_eq!(snapshot.agents[0].engines[0].port, 8000);
        assert!(snapshot.agents[0].engines[0].has_metrics);
        assert_eq!(
            snapshot.models(),
            vec!["Qwen3-Embedding".to_owned(), "unsloth/Qwen3.8-Flash-Next-FP8".to_owned()]
        );
        let lines = snapshot.engine_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("node-01"));
        assert!(lines[0].contains("vllm:8000"));
        assert!(lines[0].contains("2 Modelle"));
    }

    #[test]
    fn cache_round_trips_and_respects_the_lock() {
        let dir = std::env::temp_dir().join(format!("sysmon-cache-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cache = dir.join("spark-cache.json");
        let lock = dir.join("spark-refresh.lock");

        let targets = vec!["node-01".to_owned(), "node-02".to_owned()];
        let documents = vec![Ok("{\"ts\": 1.0}".to_owned()), Err("Connection refused".to_owned())];
        write_cache(&cache, &targets, &documents);

        let (timestamp, read_back) = read_cache(&cache).expect("cache readable");
        assert!(timestamp > 0.0);
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].as_ref().unwrap(), "{\"ts\": 1.0}");
        assert_eq!(read_back[1].as_ref().unwrap_err(), "Connection refused");

        assert!(acquire_lock(&lock), "first instance takes the lock");
        assert!(!acquire_lock(&lock), "second instance must not fetch");
        release_lock(&lock);
        assert!(acquire_lock(&lock), "lock is free again after release");
        release_lock(&lock);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_agent_versions_offer_no_update() {
        let mut inner = Inner::default();
        inner.agents.push(AgentSlot {
            state: AgentState {
                label: "node-01:8787".to_owned(),
                online: true,
                version: crate::install::expected_agent_version().to_owned(),
                ..AgentState::default()
            },
            last_rx: Some(Instant::now()),
        });
        let snapshot = SparkSnapshot {
            enabled: true,
            agents: inner.agents.iter().map(|slot| slot.state.clone()).collect(),
            ..SparkSnapshot::default()
        };
        assert!(snapshot.version_mismatches().is_empty());
        assert!(snapshot.outdated_agents().is_empty());
    }

    #[test]
    fn older_agent_versions_are_flagged_once() {
        let agent = AgentState {
            label: "desktop-01:8787".to_owned(),
            online: true,
            version: "0.1".to_owned(),
            ..AgentState::default()
        };
        let snapshot = SparkSnapshot {
            enabled: true,
            agents: vec![agent],
            ..SparkSnapshot::default()
        };
        let outdated = snapshot.outdated_agents();
        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].0, "desktop-01");
        assert_eq!(outdated[0].1, "desktop-01:8787");
        assert_eq!(outdated[0].2, "0.1");
    }

    #[test]
    fn config_saves_and_loads_again() {
        let dir = std::env::temp_dir().join(format!("sysmon-config-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let mut config = SparkConfig::default();
        config.targets = vec!["spark-09:8787".to_owned(), "node-b".to_owned()];
        config.use_discovered = false;
        let path = config.save().expect("configuration must be writable");
        assert!(path.exists(), "config file was not created");

        let loaded = SparkConfig::load();
        assert_eq!(loaded.targets, vec!["spark-09:8787".to_owned(), "node-b".to_owned()]);
        assert!(!loaded.use_discovered);
        assert!(loaded.enabled);

        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_round_trips_through_the_file() {
        let mut config = SparkConfig::default();
        config.targets = vec!["node-a:8787".to_owned(), "node-b".to_owned()];
        config.use_discovered = false;
        let value = serde_json::json!({
            "enabled": config.enabled,
            "use_discovered": config.use_discovered,
            "agents": config.targets,
            "token": "",
        });
        let mut reloaded = SparkConfig::default();
        reloaded.apply_file(&value);
        assert_eq!(reloaded.targets, config.targets);
        assert!(!reloaded.use_discovered);
        assert!(reloaded.enabled);
    }

    #[test]
    fn headless_reason_is_parsed_and_does_not_warn() {
        let (monitor, _config) = monitor_with(&["node-01", "node-02"]);
        apply_document(&monitor.inner, 1, HEADLESS_DOCUMENT);

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.agents[1].engine.reason, "no-endpoint");
        assert!(snapshot.warnings().is_empty());
    }

    #[test]
    fn thresholds_produce_warnings() {
        let (monitor, _config) = monitor_with(&["node-01"]);
        let hot = DOCUMENT.replace("\"temp_c\": 61.0", "\"temp_c\": 84.0")
                          .replace("\"preemptions_total\": 2", "\"preemptions_total\": 3")
                          .replace("\"waiting\": 1", "\"waiting\": 2");
        apply_document(&monitor.inner, 0, &hot);

        let snapshot = monitor.snapshot();
        let warnings = snapshot.warnings();
        assert_eq!(warnings.len(), 3, "expected three warnings, got {warnings:?}");
        assert!(warnings.iter().any(|text| text.contains("Präemptionen")));
        assert!(warnings.iter().any(|text| text.contains("warten")));
        assert!(warnings.iter().any(|text| text.contains("84 °C")));
        assert!(snapshot.node_is_hot(&snapshot.agents[0]));
    }

    #[test]
    fn invalid_json_is_ignored() {
        let (monitor, _config) = monitor_with(&["node-01"]);
        apply_document(&monitor.inner, 0, "{not json");
        let snapshot = monitor.snapshot();
        assert!(!snapshot.agents[0].online);
    }

    #[test]
    fn urls_are_normalised() {
        assert_eq!(agent_url("node-01", None), "ws://node-01:8787/");
        assert_eq!(agent_url("node-01:9000", None), "ws://node-01:9000/");
        assert_eq!(agent_url("ws://10.0.0.5:8787/", None), "ws://10.0.0.5:8787/");
        assert_eq!(
            agent_url("node-01", Some("secret")),
            "ws://node-01:8787/?token=secret"
        );
    }

    #[test]
    fn config_file_values_override_defaults() {
        let mut config = SparkConfig::default();
        let value: Value = serde_json::from_str(
            r#"
            {
              "enabled": true,
              "token": " t ",
              "agents": ["node-a:8787", " node-b "]
            }
            "#,
        )
        .unwrap();
        config.apply_file(&value);
        assert!(config.enabled);
        assert_eq!(config.token.as_deref(), Some("t"));
        assert_eq!(config.targets, vec!["node-a:8787", "node-b"]);
    }
}
