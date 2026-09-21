// SPDX-License-Identifier: GPL-3.0-only
//! Process table for the applet: a local `/proc` scan, the node-agent protocol
//! (`processes` / `kill`) and the two signals the process page offers.
//!
//! The local scan runs in its own thread and only while the process page is
//! open, so a closed popup costs nothing. Requests to a node agent are answered
//! over a short-lived WebSocket connection that the applet opens per request,
//! exactly like the model list and the agent installation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::Message;

/// Terminate request: the process may clean up and is expected to exit.
pub const SIGTERM: i32 = 15;
/// Hard kill: the kernel removes the process immediately, without cleanup.
pub const SIGKILL: i32 = 9;

/// Rows requested from a node agent. The agent clamps to 1000.
pub const REMOTE_LIMIT: usize = 400;
/// Local scan interval while the process page is visible.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// Agent version that learned the `processes` and `kill` actions.
const PROCESS_API_VERSION: (u32, u32) = (2, 2);
/// Longest command line shown in the popup.
const CMD_MAX: usize = 160;

/// One process row, identical on the local host and on a node agent.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub user: String,
    /// Share of one core in percent; can exceed 100 on a multi-threaded process.
    pub cpu: f32,
    pub mem_kb: u64,
    pub state: String,
    pub cmd: String,
}

impl ProcessEntry {
    /// Resident memory in MiB, rounded for display.
    pub fn mem_mib(&self) -> f32 {
        self.mem_kb as f32 / 1024.0
    }

    /// Short state letter explained in the page footnote.
    pub fn state_label(&self) -> &'static str {
        match self.state.as_str() {
            "R" => "läuft",
            "S" => "schlafend",
            "D" => "ungestört",
            "I" => "untätig",
            "T" => "angehalten",
            "t" => "getraced",
            "Z" => "Zombie",
            "X" | "x" => "beendet",
            _ => "unbekannt",
        }
    }
}

/// One process list: rows plus the counters of the answering host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessList {
    pub entries: Vec<ProcessEntry>,
    pub total: usize,
    pub cores: usize,
    pub error: Option<String>,
}

impl ProcessList {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

/// Column the list is sorted by; the active one is highlighted in the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Cpu,
    Memory,
    Pid,
    Name,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Memory => "RAM",
            SortKey::Pid => "PID",
            SortKey::Name => "Name",
        }
    }
}

/// Case-insensitive match of the search text against pid, name, command, user.
pub fn matches(entry: &ProcessEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    entry.pid.to_string().contains(&query)
        || entry.name.to_lowercase().contains(&query)
        || entry.cmd.to_lowercase().contains(&query)
        || entry.user.to_lowercase().contains(&query)
}

/// Filters, sorts and truncates the rows for the view.
pub fn filter_and_sort(
    entries: &[ProcessEntry],
    query: &str,
    key: SortKey,
) -> Vec<ProcessEntry> {
    let mut rows: Vec<ProcessEntry> = entries
        .iter()
        .filter(|entry| matches(entry, query))
        .cloned()
        .collect();
    match key {
        SortKey::Cpu => rows.sort_by(|left, right| {
            right
                .cpu
                .partial_cmp(&left.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.pid.cmp(&right.pid))
        }),
        SortKey::Memory => rows.sort_by(|left, right| {
            right
                .mem_kb
                .cmp(&left.mem_kb)
                .then_with(|| left.pid.cmp(&right.pid))
        }),
        SortKey::Pid => rows.sort_by_key(|entry| entry.pid),
        SortKey::Name => rows.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.pid.cmp(&right.pid))
        }),
    }
    rows
}

/// Whether an agent version knows the process actions.
pub fn supports_processes(version: &str) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|item| item.parse::<u32>().ok());
    let minor = parts.next().and_then(|item| item.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => (major, minor) >= PROCESS_API_VERSION,
        _ => false,
    }
}

// --------------------------------------------------------------------------- protocol

/// Parses the reply of a `processes` request.
pub fn parse_reply(text: &str) -> Result<ProcessList, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("ungültige Antwort: {error}"))?;
    if let Some(action) = value.get("action").and_then(Value::as_str) {
        if action != "processes" {
            return Err(format!("unerwartete Antwort: {action}"));
        }
    }
    if !value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("der Agent hat die Prozessliste abgelehnt");
        return Err(error.to_owned());
    }
    let entries = value
        .get("processes")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_entry).collect())
        .unwrap_or_default();
    Ok(ProcessList {
        entries,
        total: value
            .get("total")
            .and_then(Value::as_u64)
            .map(|count| count as usize)
            .unwrap_or(0),
        cores: value
            .get("cores")
            .and_then(Value::as_u64)
            .map(|count| count as usize)
            .unwrap_or(0),
        error: None,
    })
}

fn parse_entry(value: &Value) -> Option<ProcessEntry> {
    let pid = value.get("pid").and_then(Value::as_u64)? as u32;
    Some(ProcessEntry {
        pid,
        ppid: value.get("ppid").and_then(Value::as_u64).unwrap_or(0) as u32,
        name: text_field(value, "name"),
        user: text_field(value, "user"),
        cpu: value.get("cpu").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        mem_kb: value.get("mem_kb").and_then(Value::as_u64).unwrap_or(0),
        state: text_field(value, "state"),
        cmd: text_field(value, "cmd"),
    })
}

fn text_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Parses the reply of a `kill` request.
fn parse_kill_reply(text: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("ungültige Antwort: {error}"))?;
    let pid = value.get("pid").and_then(Value::as_u64).unwrap_or(0);
    let signal = value.get("signal").and_then(Value::as_i64).unwrap_or(0);
    if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(format!(
            "Signal {signal} ({}) an PID {pid} gesendet",
            signal_name(signal as i32)
        ))
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("der Agent hat das Signal abgelehnt")
            .to_owned())
    }
}

/// German name of the two supported signals, used in messages.
pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        SIGKILL => "SIGKILL",
        _ => "SIGTERM",
    }
}

// --------------------------------------------------------------------------- local scan

/// Reads the local process table and derives CPU shares between two scans.
#[derive(Default)]
pub struct ProcSampler {
    previous: HashMap<u32, f64>,
    previous_total: f64,
    users: HashMap<u32, String>,
    cores: usize,
}

impl ProcSampler {
    pub fn new() -> Self {
        let cores = thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);
        Self {
            users: user_names(),
            cores,
            ..Self::default()
        }
    }

    /// Total CPU jiffies of all cores, from the `cpu ` line of `/proc/stat`.
    fn cpu_total() -> Option<f64> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let line = text.lines().next()?;
        let mut parts = line.split_whitespace();
        if parts.next() != Some("cpu") {
            return None;
        }
        Some(parts.filter_map(|item| item.parse::<f64>().ok()).take(8).sum())
    }

    /// One local scan; the first scan reports 0 % for every process because a
    /// percentage needs a previous counter value.
    pub fn sample(&mut self) -> ProcessList {
        let Some(total) = Self::cpu_total() else {
            return ProcessList::failed("/proc/stat ist nicht lesbar");
        };
        let total_delta = total - self.previous_total;
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return ProcessList::failed("/proc ist nicht lesbar");
        };

        let mut rows: Vec<ProcessEntry> = Vec::new();
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
            let cpu = match self.previous.get(&pid) {
                Some(before) if total_delta > 0.0 => {
                    ((jiffies - before) / total_delta * self.cores as f64 * 100.0).max(0.0)
                }
                _ => 0.0,
            };
            rows.push(ProcessEntry {
                pid,
                ppid,
                name: name.clone(),
                user,
                cpu: ((cpu * 10.0).round() / 10.0) as f32,
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

        ProcessList {
            total: rows.len(),
            entries: rows,
            cores: self.cores,
            error: None,
        }
    }
}

/// Local process list, sampled in a background thread only while it is wanted.
pub struct LocalProcesses {
    inner: Arc<Mutex<ProcessList>>,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl LocalProcesses {
    pub fn start() -> Self {
        let inner = Arc::new(Mutex::new(ProcessList::default()));
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let inner = Arc::clone(&inner);
            let active = Arc::clone(&active);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("sysmon-procs".to_owned())
                .spawn(move || {
                    let mut sampler = ProcSampler::new();
                    while !stop.load(Ordering::SeqCst) {
                        if active.load(Ordering::SeqCst) {
                            let list = sampler.sample();
                            if let Ok(mut guard) = inner.lock() {
                                *guard = list;
                            }
                            sleep_in_steps(&stop, SAMPLE_INTERVAL);
                        } else {
                            sleep_in_steps(&stop, Duration::from_millis(300));
                        }
                    }
                })
                .ok();
        }
        Self {
            inner,
            active,
            stop,
        }
    }

    /// Turns the scan on while the process page is visible and off afterwards.
    pub fn set_active(&self, wanted: bool) {
        self.active.store(wanted, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> ProcessList {
        self.inner
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Drop for LocalProcesses {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn sleep_in_steps(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100).min(deadline - Instant::now()));
    }
}

/// Splits one `/proc/<pid>/stat` line; the command name may contain spaces and
/// parentheses, so the numbers are read after the *last* `)`.
fn parse_stat_line(line: &str) -> Option<(u32, String, String, u32, f64, f64)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid = line[..open].trim().parse::<u32>().ok()?;
    let name = line[open + 1..close].to_owned();
    // state, ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt,
    // majflt, cmajflt, utime, stime
    let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
    if rest.len() < 13 {
        return None;
    }
    Some((
        pid,
        name,
        rest[0].to_owned(),
        rest[1].parse::<u32>().unwrap_or(0),
        rest[11].parse::<f64>().unwrap_or(0.0),
        rest[12].parse::<f64>().unwrap_or(0.0),
    ))
}

/// uid and resident memory from `/proc/<pid>/status`.
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
    truncate(&mut cmd, CMD_MAX);
    cmd
}

fn truncate(text: &mut String, max: usize) {
    if text.chars().count() <= max {
        return;
    }
    let cut = text
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(cut);
    text.push('…');
}

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

// --------------------------------------------------------------------------- signals

/// Signals one local process. `Ok` carries the confirmation text for the page.
pub fn kill_local(pid: u32, signal: i32) -> Result<String, String> {
    if signal != SIGTERM && signal != SIGKILL {
        return Err("nur SIGTERM (15) und SIGKILL (9) sind erlaubt".to_owned());
    }
    if pid <= 1 {
        return Err("PID 1 kann nicht beendet werden".to_owned());
    }
    if pid == std::process::id() {
        return Err("das Applet kann sich nicht selbst beenden".to_owned());
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        return Ok(format!(
            "Signal {signal} ({}) an PID {pid} gesendet",
            signal_name(signal)
        ));
    }
    Err(os_error_text(&std::io::Error::last_os_error()))
}

fn os_error_text(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EPERM) => "keine Berechtigung (EPERM) – nur eigene Prozesse".to_owned(),
        Some(libc::ESRCH) => "Prozess existiert nicht mehr".to_owned(),
        Some(libc::EINVAL) => "ungültiges Signal".to_owned(),
        Some(code) => format!("{error} (errno {code})"),
        None => error.to_string(),
    }
}

// --------------------------------------------------------------------------- node agents

fn request(target: &str, token: Option<&str>, message: Value, action: &str) -> Result<String, String> {
    let url = crate::spark::agent_url(target, token);
    let (mut socket, _) = tungstenite::connect(url.as_str())
        .map_err(|error| crate::spark::short_error(&error.to_string()))?;
    socket
        .send(Message::Text(message.to_string().into()))
        .map_err(|error| crate::spark::short_error(&error.to_string()))?;

    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => {
                // The agent keeps pushing its 2 s snapshot; only the reply with
                // the matching action belongs to this request.
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if value.get("action").and_then(Value::as_str) != Some(action) {
                    continue;
                }
                let _ = socket.close(None);
                return Ok(text.as_str().to_owned());
            }
            Ok(Message::Close(_)) => return Err("Verbindung geschlossen".to_owned()),
            Ok(_) => {}
            Err(error) => return Err(crate::spark::short_error(&error.to_string())),
        }
    }
    Err("Zeitüberschreitung".to_owned())
}

/// Process list of one node agent.
pub fn fetch_remote(target: &str, token: Option<&str>) -> Result<ProcessList, String> {
    let reply = request(
        target,
        token,
        serde_json::json!({"action": "processes", "limit": REMOTE_LIMIT}),
        "processes",
    )?;
    parse_reply(&reply)
}

/// Sends a signal to a process on one node agent.
pub fn kill_remote(
    target: &str,
    token: Option<&str>,
    pid: u32,
    signal: i32,
) -> Result<String, String> {
    let reply = request(
        target,
        token,
        serde_json::json!({"action": "kill", "pid": pid, "signal": signal}),
        "kill",
    )?;
    parse_kill_reply(&reply)
}

/// Result of one asynchronous agent request.
#[derive(Clone, Debug, Default)]
pub struct TaskSnapshot {
    pub running: bool,
    pub list: Option<ProcessList>,
    pub kill: Option<Result<String, String>>,
}

#[derive(Default)]
struct TaskState {
    running: bool,
    list: Option<ProcessList>,
    kill: Option<Result<String, String>>,
}

/// One request to a node agent, executed in its own thread so the popup never
/// waits on the network.
pub struct ProcessTask {
    state: Arc<Mutex<TaskState>>,
}

impl ProcessTask {
    /// Requests the process list of a node.
    pub fn list(target: String, token: Option<String>) -> Self {
        Self::spawn(move || {
            let list = fetch_remote(&target, token.as_deref())
                .unwrap_or_else(ProcessList::failed);
            TaskState {
                running: false,
                list: Some(list),
                kill: None,
            }
        })
    }

    /// Sends a signal through a node agent.
    pub fn kill(target: String, token: Option<String>, pid: u32, signal: i32) -> Self {
        Self::spawn(move || TaskState {
            running: false,
            list: None,
            kill: Some(kill_remote(&target, token.as_deref(), pid, signal)),
        })
    }

    fn spawn(work: impl FnOnce() -> TaskState + Send + 'static) -> Self {
        let state = Arc::new(Mutex::new(TaskState {
            running: true,
            ..TaskState::default()
        }));
        {
            let state = Arc::clone(&state);
            thread::Builder::new()
                .name("sysmon-proc-task".to_owned())
                .spawn(move || {
                    let result = work();
                    if let Ok(mut guard) = state.lock() {
                        *guard = result;
                    }
                })
                .ok();
        }
        Self { state }
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        self.state
            .lock()
            .map(|guard| TaskSnapshot {
                running: guard.running,
                list: guard.list.clone(),
                kill: guard.kill.clone(),
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const REPLY: &str = r#"{"ok":true,"action":"processes","ts":1789995563.17,"total":564,
        "cores":16,"processes":[
        {"pid":16005,"ppid":1,"name":"chrome","user":"sysmon","cpu":26.3,"mem_kb":701000,"state":"S","cmd":"/opt/google/chrome/chrome --type=renderer"},
        {"pid":1,"ppid":0,"name":"systemd","user":"root","cpu":0.0,"mem_kb":12000,"state":"S","cmd":"/usr/lib/systemd/systemd"}]}"#;

    #[test]
    fn parses_a_process_reply() {
        let list = parse_reply(REPLY).expect("Antwort");
        assert_eq!(list.total, 564);
        assert_eq!(list.cores, 16);
        assert_eq!(list.entries.len(), 2);
        assert_eq!(list.entries[0].pid, 16005);
        assert_eq!(list.entries[0].name, "chrome");
        assert_eq!(list.entries[0].cpu, 26.3);
        assert_eq!(list.entries[0].mem_mib().round(), 685.0);
        assert_eq!(list.entries[1].user, "root");
    }

    #[test]
    fn error_replies_become_a_message() {
        let error = parse_reply(r#"{"ok":false,"action":"processes","error":"Prozessliste ist belegt"}"#)
            .expect_err("muss fehlschlagen");
        assert_eq!(error, "Prozessliste ist belegt");
        assert!(parse_reply("kein json").is_err());
    }

    #[test]
    fn parses_a_kill_reply() {
        let ok = parse_kill_reply(r#"{"ok":true,"action":"kill","pid":42,"signal":9}"#).unwrap();
        assert_eq!(ok, "Signal 9 (SIGKILL) an PID 42 gesendet");
        let error =
            parse_kill_reply(r#"{"ok":false,"action":"kill","pid":42,"signal":15,"error":"Prozess existiert nicht mehr"}"#)
                .unwrap_err();
        assert_eq!(error, "Prozess existiert nicht mehr");
    }

    #[test]
    fn filters_and_sorts_rows() {
        let list = parse_reply(REPLY).unwrap();
        let by_cpu = filter_and_sort(&list.entries, "", SortKey::Cpu);
        assert_eq!(by_cpu[0].pid, 16005);
        let by_pid = filter_and_sort(&list.entries, "", SortKey::Pid);
        assert_eq!(by_pid[0].pid, 1);
        let by_name = filter_and_sort(&list.entries, "", SortKey::Name);
        assert_eq!(by_name[0].name, "chrome");
        let by_memory = filter_and_sort(&list.entries, "", SortKey::Memory);
        assert_eq!(by_memory[0].pid, 16005);

        assert_eq!(filter_and_sort(&list.entries, "systemd", SortKey::Cpu).len(), 1);
        assert_eq!(filter_and_sort(&list.entries, "16005", SortKey::Cpu).len(), 1);
        assert_eq!(filter_and_sort(&list.entries, "ROOT", SortKey::Cpu).len(), 1);
        assert_eq!(filter_and_sort(&list.entries, "renderer", SortKey::Cpu).len(), 1);
        assert!(filter_and_sort(&list.entries, "gibtsnicht", SortKey::Cpu).is_empty());
    }

    #[test]
    fn knows_which_agents_support_processes() {
        assert!(!supports_processes("2.1.0"));
        assert!(!supports_processes("2.0.1"));
        assert!(!supports_processes(""));
        assert!(supports_processes("2.2.0"));
        assert!(supports_processes("2.10.0"));
        assert!(supports_processes("3.0.0"));
    }

    #[test]
    fn parses_stat_lines_with_tricky_command_names() {
        let line = "1234 (Web Content) S 1 1234 1234 0 -1 4194560 1 0 0 0 12 34 0 0";
        let (pid, name, state, ppid, utime, stime) = parse_stat_line(line).expect("stat");
        assert_eq!((pid, name.as_str(), state.as_str(), ppid), (1234, "Web Content", "S", 1));
        assert_eq!((utime, stime), (12.0, 34.0));
        assert!(parse_stat_line("keine klammern").is_none());
    }

    #[test]
    fn reads_uid_and_rss_from_status() {
        let text = "Name:\tbash\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t  20480 kB\n";
        assert_eq!(parse_status(text), (Some(1000), 20480));
        assert_eq!(parse_status("Name:\tinit\n"), (None, 0));
    }

    #[test]
    fn scans_the_real_process_table() {
        let mut sampler = ProcSampler::new();
        let first = sampler.sample();
        assert!(first.error.is_none());
        assert!(first.total > 1);
        assert_eq!(first.cores, thread::available_parallelism().unwrap().get());
        assert!(first.entries.iter().all(|entry| entry.cpu == 0.0));

        thread::sleep(Duration::from_millis(400));
        let second = sampler.sample();
        let own = second
            .entries
            .iter()
            .find(|entry| entry.pid == std::process::id())
            .expect("eigener Prozess fehlt");
        assert!(!own.name.is_empty());
        assert!(!own.cmd.is_empty());
        assert!(own.mem_kb > 0);
        assert!(!own.user.is_empty());
        assert!(second.entries.iter().any(|entry| entry.cpu > 0.0));
    }

    #[test]
    fn refuses_unsafe_signals() {
        assert_eq!(
            kill_local(1, SIGTERM).unwrap_err(),
            "PID 1 kann nicht beendet werden"
        );
        assert_eq!(
            kill_local(std::process::id(), SIGKILL).unwrap_err(),
            "das Applet kann sich nicht selbst beenden"
        );
        assert_eq!(
            kill_local(4242, 1).unwrap_err(),
            "nur SIGTERM (15) und SIGKILL (9) sind erlaubt"
        );
    }

    #[test]
    fn terminates_a_real_process() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .expect("sleep startet");
        let pid = child.id();
        let message = kill_local(pid, SIGTERM).expect("SIGTERM muss ankommen");
        assert_eq!(message, format!("Signal 15 (SIGTERM) an PID {pid} gesendet"));
        let status = child.wait().expect("sleep endet");
        assert!(!status.success());
        // A second signal must report the gone process instead of pretending.
        let error = kill_local(pid, SIGTERM).unwrap_err();
        assert!(
            error.contains("existiert nicht mehr") || error.contains("keine Berechtigung"),
            "{error}"
        );
    }
}
