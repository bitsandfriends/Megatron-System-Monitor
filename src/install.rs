// SPDX-License-Identifier: GPL-3.0-only
//! Installs the monitor agent on a remote host over SSH.
//!
//! The applet carries the agent (aarch64 binary, Python fallback), the systemd
//! unit and the install script, uploads them, runs the installer with sudo and
//! verifies the service. Everything is logged line by line so the popup can show
//! the progress while it happens.
//!
//! Passwords are only used through an SSH askpass helper and sudo's stdin; they
//! never appear in a command line or in the log.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The agents are embedded so that installation works without the repository.
const AGENT_AARCH64: &[u8] = include_bytes!("../agent/rust/dist/megatron-sysmon-agent-aarch64");
const AGENT_X86_64: &[u8] = include_bytes!("../agent/rust/dist/megatron-sysmon-agent-x86_64");
const AGENT_PYTHON: &str = include_str!("../agent/megatron_sysmon_agent.py");
const INSTALL_SCRIPT: &str = include_str!("../agent/install-agent.sh");
const UNIT_FILE: &str = include_str!("../agent/megatron-sysmon-agent.service");
/// Version of the agent binaries embedded in this applet (written by
/// `agent/build-agent.sh`), so every node can be checked against it.
pub const AGENT_VERSION: &str = include_str!("../agent/rust/dist/agent-version.txt");

/// The expected agent version without surrounding whitespace.
pub fn expected_agent_version() -> &'static str {
    AGENT_VERSION.trim()
}

/// Compares two dotted versions; missing parts count as zero.
pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    fn parts(value: &str) -> Vec<u64> {
        value
            .trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (left_parts, right_parts) = (parts(left), parts(right));
    let length = left_parts.len().max(right_parts.len());
    for index in 0..length {
        let left_value = left_parts.get(index).copied().unwrap_or(0);
        let right_value = right_parts.get(index).copied().unwrap_or(0);
        match left_value.cmp(&right_value) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// What to do about the version an agent reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentVersionState {
    /// Same version as the binaries in this applet.
    Current,
    /// Older agent: an update can be offered.
    Outdated,
    /// Newer agent: the applet itself is older.
    AppletOutdated,
    /// The agent did not report a version.
    Unknown,
}

pub fn agent_version_state(reported: &str) -> AgentVersionState {
    let reported = reported.trim();
    if reported.is_empty() {
        return AgentVersionState::Unknown;
    }
    match compare_versions(reported, expected_agent_version()) {
        std::cmp::Ordering::Equal => AgentVersionState::Current,
        std::cmp::Ordering::Less => AgentVersionState::Outdated,
        std::cmp::Ordering::Greater => AgentVersionState::AppletOutdated,
    }
}

const REMOTE_DIR: &str = "/tmp/megatron-sysmon-agent";
const CONNECT_TIMEOUT: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    Idle,
    Running,
    Done,
    Failed,
}

impl Default for InstallStatus {
    fn default() -> Self {
        Self::Idle
    }
}

/// Progress of one installation, read by the popup.
#[derive(Debug, Clone, Default)]
pub struct InstallState {
    pub lines: Vec<String>,
    pub status: InstallStatus,
}

impl InstallState {
    pub fn is_running(&self) -> bool {
        self.status == InstallStatus::Running
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.status, InstallStatus::Done | InstallStatus::Failed)
    }

    pub fn succeeded(&self) -> bool {
        self.status == InstallStatus::Done
    }

    /// Last lines for the log view.
    pub fn tail(&self, count: usize) -> Vec<String> {
        let start = self.lines.len().saturating_sub(count);
        self.lines[start..].to_vec()
    }
}

/// What to install where.
#[derive(Debug, Clone, Default)]
pub struct InstallRequest {
    pub host: String,
    pub user: String,
    pub password: String,
    pub port: u16,
}

impl InstallRequest {
    pub fn target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// Runs one installation in a background thread.
pub struct Installer {
    state: Arc<Mutex<InstallState>>,
    stop: Arc<AtomicBool>,
}

impl Installer {
    pub fn start(request: InstallRequest) -> Self {
        let state = Arc::new(Mutex::new(InstallState {
            status: InstallStatus::Running,
            ..InstallState::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("sysmon-install".to_owned())
            .spawn(move || run(request, &thread_state, &thread_stop))
            .ok();
        Self { state, stop }
    }

    pub fn snapshot(&self) -> InstallState {
        self.state.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    pub fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn log(state: &Arc<Mutex<InstallState>>, line: impl Into<String>) {
    if let Ok(mut guard) = state.lock() {
        guard.lines.push(line.into());
        // Keep the buffer bounded; the popup shows the tail anyway.
        if guard.lines.len() > 400 {
            guard.lines.remove(0);
        }
    }
}

fn finish(state: &Arc<Mutex<InstallState>>, status: InstallStatus) {
    if let Ok(mut guard) = state.lock() {
        guard.status = status;
    }
}

/// Temporary files for the SSH askpass; removed on every exit path.
struct PasswordFiles {
    directory: PathBuf,
}

impl PasswordFiles {
    fn create(password: &str) -> Option<Self> {
        if password.is_empty() {
            return None;
        }
        let directory = super::spark::runtime_directory().join("install");
        fs::create_dir_all(&directory).ok()?;
        let secret = directory.join("askpass.secret");
        let script = directory.join("askpass.sh");
        fs::write(&secret, password.as_bytes()).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o600));
        }
        let script_body = format!(
            "#!/bin/sh\ncat {}\n",
            secret.display()
        );
        fs::write(&script, script_body).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
        }
        Some(Self { directory })
    }

    fn script(&self) -> PathBuf {
        self.directory.join("askpass.sh")
    }
}

impl Drop for PasswordFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join("askpass.secret"));
        let _ = fs::remove_file(self.directory.join("askpass.sh"));
    }
}

fn ssh_command(request: &InstallRequest, files: Option<&PasswordFiles>) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &format!("ConnectTimeout={CONNECT_TIMEOUT}"),
        "-o",
        "BatchMode=no",
        "-p",
        &request.port.to_string(),
    ]);
    if let Some(files) = files {
        command.args([
            "-o",
            "PreferredAuthentications=password",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "NumberOfPasswordPrompts=1",
        ]);
        command.env("SSH_ASKPASS", files.script());
        command.env("SSH_ASKPASS_REQUIRE", "force");
        command.env("DISPLAY", ":0");
    }
    command.arg(request.target());
    command
}

fn scp_command(request: &InstallRequest, files: Option<&PasswordFiles>) -> Command {
    let mut command = Command::new("scp");
    command.args([
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &format!("ConnectTimeout={CONNECT_TIMEOUT}"),
        "-P",
        &request.port.to_string(),
    ]);
    if let Some(files) = files {
        command.args([
            "-o",
            "PreferredAuthentications=password",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "NumberOfPasswordPrompts=1",
        ]);
        command.env("SSH_ASKPASS", files.script());
        command.env("SSH_ASKPASS_REQUIRE", "force");
        command.env("DISPLAY", ":0");
    }
    command
}

/// Runs a remote command and returns its combined output.
fn run_remote(
    request: &InstallRequest,
    files: Option<&PasswordFiles>,
    remote: &str,
    stdin_secret: Option<&str>,
) -> Result<String, String> {
    let mut command = ssh_command(request, files);
    command.arg(remote);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin_secret.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    if let (Some(secret), Some(mut stdin)) = (stdin_secret, child.stdin.take()) {
        let _ = stdin.write_all(format!("{secret}\n").as_bytes());
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

fn push_file(
    request: &InstallRequest,
    files: Option<&PasswordFiles>,
    content: &[u8],
    remote_name: &str,
) -> Result<(), String> {
    let local = super::spark::runtime_directory().join("install").join(remote_name);
    if let Some(parent) = local.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&local, content).map_err(|error| error.to_string())?;
    let attempt = |legacy: bool| -> Result<(), String> {
        let mut command = scp_command(request, files);
        if legacy {
            // Some hosts (NAS systems) have a broken SFTP subsystem; the legacy
            // scp protocol then works while the modern one fails with
            // "dest open ... No such file or directory".
            command.arg("-O");
        }
        command.arg(&local);
        command.arg(format!("{}:{REMOTE_DIR}/{remote_name}", request.target()));
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        let output = command.output().map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    };

    let result = attempt(false).or_else(|error| {
        log_once(&format!("Transfer mit SFTP fehlgeschlagen ({error}); versuche das alte scp-Protokoll"));
        attempt(true)
    });
    let _ = fs::remove_file(&local);
    result
}

/// One-time log helper for functions without access to the install state.
fn log_once(message: &str) {
    eprintln!("[installer] {message}");
}

fn run(
    request: InstallRequest,
    state: &Arc<Mutex<InstallState>>,
    stop: &Arc<AtomicBool>,
) {
    let files = PasswordFiles::create(&request.password);
    match install(&request, state, stop, files.as_ref()) {
        Ok(()) => {
            log(state, "Fertig: Dienst läuft und der Knoten wird übernommen.");
            finish(state, InstallStatus::Done);
        }
        Err(error) => {
            log(state, format!("Fehlgeschlagen: {error}"));
            finish(state, InstallStatus::Failed);
        }
    }
    // Best effort cleanup on the target.
    let _ = run_remote(&request, files.as_ref(), &format!("rm -rf {REMOTE_DIR}"), None);
}

fn install(
    request: &InstallRequest,
    state: &Arc<Mutex<InstallState>>,
    stop: &Arc<AtomicBool>,
    files: Option<&PasswordFiles>,
) -> Result<(), String> {
    if request.host.is_empty() {
        return Err("Kein Hostname angegeben".to_owned());
    }
    if request.user.is_empty() {
        return Err("Kein Benutzername angegeben".to_owned());
    }
    log(state, format!("Verbinde zu {} (Port {})", request.target(), request.port));
    log(
        state,
        format!("Zielversion des Agenten: {}", expected_agent_version()),
    );
    let probe = run_remote(
        request,
        files,
        "uname -m; id -un; id -u; (command -v getenforce >/dev/null && getenforce) || echo Disabled",
        None,
    )?;
    let mut lines = probe.lines();
    let architecture = lines.next().unwrap_or_default().trim().to_owned();
    let login = lines.next().unwrap_or_default().trim().to_owned();
    let uid = lines.next().unwrap_or_default().trim().to_owned();
    let selinux = lines.next().unwrap_or_default().trim().to_owned();
    let user_service = selinux == "Enforcing";
    if user_service {
        log(
            state,
            "SELinux ist aktiv: der Agent wird als Benutzerdienst installiert \
             (ein Systemdienst dürfte keine lokalen Ports verbinden).",
        );
    }
    log(state, format!("Verbunden als {login} ({architecture}, uid {uid})"));
    if architecture.is_empty() {
        return Err("Keine Antwort auf uname".to_owned());
    }
    if stop.load(Ordering::SeqCst) {
        return Err("Abgebrochen".to_owned());
    }

    let (agent_bytes, agent_name): (Vec<u8>, &str) = match architecture.as_str() {
        "aarch64" => (AGENT_AARCH64.to_vec(), "megatron-sysmon-agent-aarch64"),
        "x86_64" => (AGENT_X86_64.to_vec(), "megatron-sysmon-agent-x86_64"),
        other => {
            log(
                state,
                format!("Hinweis: kein Rust-Binary für {other}; es wird der Python-Agent installiert."),
            );
            (AGENT_PYTHON.as_bytes().to_vec(), "megatron_sysmon_agent.py")
        }
    };

    run_remote(request, files, &format!("mkdir -p {REMOTE_DIR}"), None)?;
    log(state, format!("Übertrage Agent ({}, {} kB)", agent_name, agent_bytes.len() / 1024));
    push_file(request, files, &agent_bytes, agent_name)?;
    push_file(request, files, UNIT_FILE.as_bytes(), "megatron-sysmon-agent.service")?;
    push_file(request, files, INSTALL_SCRIPT.as_bytes(), "install-agent.sh")?;
    if stop.load(Ordering::SeqCst) {
        return Err("Abgebrochen".to_owned());
    }

    let plain_install = format!("bash {REMOTE_DIR}/install-agent.sh install");
    let outcome = if user_service {
        // The user service must belong to the login user, not to root.
        log(state, "Installiere Benutzerdienst …");
        run_remote(request, files, &plain_install, None)
    } else if uid == "0" {
        log(state, "Installiere Dienst …");
        run_remote(request, files, &plain_install, None)
    } else if request.password.is_empty() {
        log(state, "Installiere Dienst (passwortloses sudo) …");
        run_remote(request, files, &format!("sudo -n {plain_install}"), None)
    } else {
        log(state, "Installiere Dienst (sudo) …");
        run_remote(request, files, &format!("sudo -S -p '' {plain_install}"), Some(&request.password))
    };
    match outcome {
        Ok(output) => {
            for line in output.lines().filter(|line| !line.trim().is_empty()).take(12) {
                log(state, line.trim().to_owned());
            }
        }
        Err(error) => {
            let hint = if error.to_lowercase().contains("sudo") {
                " (sudo braucht hier ein Passwort; bitte Benutzer mit sudo-Rechten angeben)"
            } else {
                ""
            };
            return Err(format!("{}{hint}", error.lines().last().unwrap_or("unbekannter Fehler")));
        }
    }

    log(state, "Prüfe den Dienst …");
    // The agent runs either as a system service or, on SELinux systems, as a
    // user service. A stale agent from an earlier installation can keep the
    // port while the freshly installed unit fails to bind, so "unit active"
    // plus "some listener exists" is not enough: the remote command also
    // reports which process answers on the port and which version it prints.
    thread::sleep(Duration::from_millis(300));
    let status = run_remote(
        request,
        files,
        "state=$(systemctl is-active megatron-sysmon-agent 2>/dev/null); \
         if [ \"$state\" != active ]; then \
             user_state=$(XDG_RUNTIME_DIR=/run/user/$(id -u) \
                 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus \
                 systemctl --user is-active megatron-sysmon-agent 2>/dev/null); \
             [ \"$user_state\" = active ] && state=active; \
         fi; \
         lpid=$(ss -ltnp 2>/dev/null | grep -m1 -E ':8787([[:space:]]|$)' \
             | sed -n 's/.*pid=\\([0-9]*\\).*/\\1/p'); \
         ver=; cmd=; \
         if [ -n \"$lpid\" ] && [ -r \"/proc/$lpid/cmdline\" ]; then \
             cmd=$(tr '\\0' ' ' < \"/proc/$lpid/cmdline\" | cut -c1-160); \
             lexe=$(readlink \"/proc/$lpid/exe\" 2>/dev/null); \
             case \"$lexe\" in */megatron-sysmon-agent) \
                 ver=$(\"$lexe\" --version 2>/dev/null | head -1);; esac; \
         fi; \
         listeners=$(ss -ltn | grep -c ':8787' || true); \
         echo \"state=${state:-unknown}\"; echo \"listeners=${listeners:-0}\"; \
         echo \"version=${ver}\"; echo \"cmd=${cmd}\"",
        None,
    )
    .unwrap_or_default();
    let probe = parse_probe(&status);
    log(
        state,
        format!(
            "Dienst: {}, Port 8787: {} Listener, lauschender Agent: {}",
            probe.state,
            probe.listeners,
            probe.running_version().unwrap_or("unbekannt")
        ),
    );
    if probe.state != "active" {
        return Err(format!("Dienst ist nicht aktiv ({})", probe.state));
    }
    if probe.listeners == 0 {
        return Err("Kein Agent lauscht auf Port 8787, obwohl der Dienst aktiv ist.".to_owned());
    }
    if let Some(version) = probe.running_version() {
        if compare_versions(version, expected_agent_version()) != std::cmp::Ordering::Equal {
            return Err(format!(
                "Auf Port 8787 antwortet Agent {version}, erwartet wird {} - \
                 ein alter Agent hält den Port.",
                expected_agent_version()
            ));
        }
    } else if !probe.cmd.contains("megatron") {
        return Err("Der Prozess auf Port 8787 ist nicht der Sysmon-Agent.".to_owned());
    }
    Ok(())
}

/// Answer of the post-install probe: which unit is active, how many sockets
/// listen on the agent port, and which process answers there.
#[derive(Debug, Default)]
struct PostInstallProbe {
    state: String,
    listeners: u32,
    version: String,
    cmd: String,
}

impl PostInstallProbe {
    /// The version printed by the listening binary, when that binary is the
    /// Rust agent (the Python fallback reports no version here; its command
    /// line identifies it instead).
    fn running_version(&self) -> Option<&str> {
        self.version
            .split_whitespace()
            .next_back()
            .filter(|value| value.chars().any(|c| c.is_ascii_digit()))
    }
}

fn parse_probe(text: &str) -> PostInstallProbe {
    let mut probe = PostInstallProbe {
        state: "unknown".to_owned(),
        ..Default::default()
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("state=") {
            probe.state = value.to_owned();
        } else if let Some(value) = line.strip_prefix("listeners=") {
            probe.listeners = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("version=") {
            probe.version = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("cmd=") {
            probe.cmd = value.trim().to_owned();
        }
    }
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_numerically() {
        assert_eq!(compare_versions("2.0.0", "2.0.0"), std::cmp::Ordering::Equal);
        assert_eq!(compare_versions("2.0", "2.0.0"), std::cmp::Ordering::Equal);
        assert_eq!(compare_versions("1.9", "2.0.0"), std::cmp::Ordering::Less);
        assert_eq!(compare_versions("2.1", "2.0.0"), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions("2.0.10", "2.0.9"), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions("v2.2", "2.1.9"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn agent_state_uses_the_shipped_version() {
        let expected = expected_agent_version();
        assert_eq!(agent_version_state(expected), AgentVersionState::Current);
        assert_eq!(agent_version_state(""), AgentVersionState::Unknown);
        assert_eq!(agent_version_state("0.9"), AgentVersionState::Outdated);
        assert_eq!(agent_version_state("99.0"), AgentVersionState::AppletOutdated);
    }

    #[test]
    fn probe_names_the_listening_agent_version() {
        let probe = parse_probe(concat!(
            "state=active\n",
            "listeners=1\n",
            "version=megatron-sysmon-agent 2.2.0\n",
            "cmd=/usr/local/lib/megatron-sysmon/megatron-sysmon-agent --bind 0.0.0.0\n",
        ));
        assert_eq!(probe.state, "active");
        assert_eq!(probe.listeners, 1);
        assert_eq!(probe.running_version(), Some("2.2.0"));
    }

    #[test]
    fn probe_without_listener_or_version_stays_empty() {
        let probe = parse_probe("state=activating\nlisteners=0\nversion=\ncmd=\n");
        assert_eq!(probe.state, "activating");
        assert_eq!(probe.listeners, 0);
        assert_eq!(probe.running_version(), None);
        let probe = parse_probe("nonsense output");
        assert_eq!(probe.state, "unknown");
        assert_eq!(probe.listeners, 0);
    }
}

/// Command line mode: install on one host and print the log (diagnostic aid).
pub fn run_cli(request: InstallRequest) -> i32 {
    let state = Arc::new(Mutex::new(InstallState {
        status: InstallStatus::Running,
        ..InstallState::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    run(request, &state, &stop);
    let snapshot = state.lock().map(|guard| guard.clone()).unwrap_or_default();
    for line in &snapshot.lines {
        println!("{line}");
    }
    if snapshot.succeeded() {
        0
    } else {
        1
    }
}
